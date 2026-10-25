//! sdframe stdout lines -> decoded frames, newest first.
//!
//! Threading: `sink()` runs on the helper's stdout thread; it parses each line, hands
//! notes straight to `on_note`, and queues `shm`/`frame` messages on an mpsc channel. One
//! worker thread owns the `ShmReader`: it blocks for a message, drains everything else
//! already queued, applies the `shm` lines in order, and reads only the newest `frame`
//! announced after the last successful (re)open. `on_frame` runs on that worker, so at most
//! one frame is in the callback at a time; frames announced while it runs pile up in the
//! channel and collapse to the newest on the next pass.
use crate::protocol::{FrameMsg, parse_frame_line};
use crate::shm::{Frame, ShmReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

enum Msg {
    Shm { name: String, slots: u32, cap: u64 },
    Frame { slot: u32, seq: u64 },
    Stop,
}

/// Consumes sdframe lines (feed them through [`FramePipeline::sink`]). Dropping it stops
/// the worker after any in-flight callback; no further frames are delivered.
pub struct FramePipeline {
    tx: Sender<Msg>,
    lines: Arc<AtomicU64>,
    on_note: Arc<dyn Fn(String) + Send + Sync>,
}

impl FramePipeline {
    pub fn new(
        on_frame: impl Fn(Frame) + Send + 'static,
        on_note: impl Fn(String) + Send + Sync + 'static,
    ) -> FramePipeline {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("sdframe-pipeline".into())
            .spawn(move || worker(rx, on_frame))
            .expect("spawn frame pipeline thread");
        FramePipeline { tx, lines: Arc::new(AtomicU64::new(0)), on_note: Arc::new(on_note) }
    }

    /// A line consumer for [`crate::helper::Supervised::spawn`]. One per helper process;
    /// the pipeline itself outlives helper restarts.
    pub fn sink(&self) -> impl Fn(String) + Send + Sync + 'static {
        let tx = self.tx.clone();
        let lines = self.lines.clone();
        let on_note = self.on_note.clone();
        move |line: String| {
            let msg = match parse_frame_line(&line) {
                Some(FrameMsg::Shm { name, slots, cap }) => Msg::Shm { name, slots, cap },
                Some(FrameMsg::Frame { slot, seq }) => Msg::Frame { slot, seq },
                Some(FrameMsg::Note(n)) => {
                    on_note(n);
                    return;
                }
                None => return,
            };
            let _ = tx.send(msg);
            lines.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `shm` + `frame` lines handed to the worker so far.
    pub fn lines_seen(&self) -> u64 {
        self.lines.load(Ordering::SeqCst)
    }
}

impl Drop for FramePipeline {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

fn worker(rx: Receiver<Msg>, on_frame: impl Fn(Frame)) {
    let mut reader: Option<ShmReader> = None;
    loop {
        let Ok(first) = rx.recv() else { return };
        let mut newest: Option<(u32, u64)> = None;
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                Msg::Stop => return,
                // A segment can already be gone (sdframe unlinks a generation ~1 s after
                // announcing the next): keep the current reader and wait for the next line.
                Msg::Shm { name, slots, cap } => {
                    if let Ok(r) = ShmReader::open(&name, slots, cap) {
                        reader = Some(r);
                        newest = None; // earlier announcements belong to the old segment
                    }
                }
                Msg::Frame { slot, seq } => newest = Some((slot, seq)),
            }
        }
        // Torn or already overwritten (None): skip; a newer frame is on its way.
        if let (Some(r), Some((slot, seq))) = (&reader, newest)
            && let Some(frame) = r.read(slot, seq)
        {
            on_frame(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helper::{Supervised, fake_helper};
    use crate::shm::{FrameHeader, ShmWriter};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const W: Duration = Duration::from_secs(5);

    fn shm_name(tag: &str) -> String {
        format!("/sdt-{}-{tag}", std::process::id())
    }

    /// Slot for frame `seq` (even, starting at 2) in a 3-slot ring, as sdframe assigns them.
    fn slot_of(seq: u64) -> u32 {
        ((seq / 2 - 1) % 3) as u32
    }

    /// 2x2 frame whose pixels all carry `seq` (low byte), so a delivery proves which slot was read.
    fn write_frame(w: &ShmWriter, seq: u64) {
        let h = FrameHeader { width: 2, height: 2, source_width: 4, source_height: 4 };
        w.write(slot_of(seq), seq, &h, &[seq as u8; 16]);
    }

    fn shm_line(name: &str, cap: u64) -> String {
        format!(r#"{{"t":"shm","name":"{name}","slots":3,"cap":{cap}}}"#)
    }

    fn frame_line(seq: u64) -> String {
        format!(r#"{{"t":"frame","slot":{},"seq":{seq}}}"#, slot_of(seq))
    }

    /// Fake sdframe = the `echo` fake: the test writes the shm itself and has the fake print
    /// the lines. Returns the helper, the frames delivered, and a gate the callback waits on.
    struct Rig {
        helper: Supervised,
        frames: mpsc::Receiver<Frame>,
        release: mpsc::Sender<()>,
        pipeline: FramePipeline,
    }

    fn rig(gated: bool) -> Rig {
        let (ftx, frx) = mpsc::channel();
        let (rtx, rrx) = mpsc::channel::<()>();
        let pipeline = FramePipeline::new(
            move |f| {
                let _ = ftx.send(f);
                if gated {
                    let _ = rrx.recv_timeout(Duration::from_secs(10));
                }
            },
            |_| {},
        );
        let helper = Supervised::spawn(fake_helper(), vec!["echo".into()], pipeline.sink(), |_| {}).unwrap();
        Rig { helper, frames: frx, release: rtx, pipeline }
    }

    fn wait_lines(p: &FramePipeline, n: u64) {
        let t0 = Instant::now();
        while p.lines_seen() < n {
            assert!(t0.elapsed() < W, "pipeline saw {} of {n} lines", p.lines_seen());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn latest_frame_wins() {
        let n = shm_name("latest");
        let w = ShmWriter::create(&n, 3, 16).unwrap();
        let r = rig(true);

        // Prime: one frame; its callback blocks until released, so the burst below piles up.
        write_frame(&w, 2);
        r.helper.send_line(&format!("{}\n{}", shm_line(&n, 16), frame_line(2)));
        let first = r.frames.recv_timeout(W).expect("first frame");
        assert_eq!((first.seq, first.data[0]), (2, 2));

        // Burst of 5 announced at once (one write -> one read in the fake -> one burst).
        for seq in [4, 6, 8, 10, 12] {
            write_frame(&w, seq);
        }
        let burst: Vec<String> = [4, 6, 8, 10, 12].iter().map(|s| frame_line(*s)).collect();
        r.helper.send_line(&burst.join("\n"));
        wait_lines(&r.pipeline, 7);
        r.release.send(()).unwrap();

        let next = r.frames.recv_timeout(W).expect("frame after the burst");
        assert_eq!((next.seq, next.data[0], next.width), (12, 12, 2), "only the newest of the burst");
        r.release.send(()).unwrap();
        assert!(r.frames.recv_timeout(Duration::from_millis(300)).is_err(), "older burst frames must be skipped");
    }

    #[test]
    fn follows_new_generation_and_survives_vanished_segment() {
        let (n1, n2) = (shm_name("gen1"), shm_name("gen2"));
        let w1 = ShmWriter::create(&n1, 3, 16).unwrap();
        let r = rig(false);
        write_frame(&w1, 2);
        r.helper.send_line(&shm_line(&n1, 16));
        r.helper.send_line(&frame_line(2));
        assert_eq!(r.frames.recv_timeout(W).unwrap().seq, 2);

        // A shm line naming a segment that is already gone: not fatal, the old reader stays.
        r.helper.send_line(r#"{"t":"shm","name":"/sdt-gone-xyz","slots":3,"cap":16}"#);
        write_frame(&w1, 4);
        r.helper.send_line(&frame_line(4));
        assert_eq!(r.frames.recv_timeout(W).unwrap().seq, 4, "old reader kept after failed open");

        // A torn slot is skipped, not delivered.
        w1.begin(slot_of(6), 6);
        r.helper.send_line(&frame_line(6));

        // New generation with a different size.
        let w2 = ShmWriter::create(&n2, 3, 64).unwrap();
        let h = FrameHeader { width: 4, height: 4, source_width: 8, source_height: 8 };
        w2.write(slot_of(8), 8, &h, &[8; 64]);
        r.helper.send_line(&shm_line(&n2, 64));
        r.helper.send_line(&frame_line(8));
        let f = r.frames.recv_timeout(W).unwrap();
        assert_eq!((f.seq, f.width, f.height, f.data.len()), (8, 4, 4, 64));
        assert!(r.frames.recv_timeout(Duration::from_millis(200)).is_err(), "torn frame 6 must not be delivered");
    }

    #[test]
    fn notes_reach_on_note() {
        let (ntx, nrx) = mpsc::channel();
        let p = FramePipeline::new(|_| {}, move |n| {
            let _ = ntx.send(n);
        });
        let sink = p.sink();
        sink(r#"{"t":"note","detail":"hello"}"#.into());
        sink("garbage".into());
        assert_eq!(nrx.recv_timeout(W).unwrap(), "hello");
    }
}
