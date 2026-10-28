//! Recording what the screen does around a step: a few timed frames plus a contact sheet.
//!
//! [`Recorder::start`] takes the baseline (the newest frame, `t = 0`), opens a sampled
//! [`FrameTap`] (25–30 fps) and hands it to a collector thread. The collector judges each
//! sampled frame as it arrives: a frame is kept only when its change print (32×64 cells
//! of mean colour, every 2nd pixel) differs from the last kept frame's in any cell by
//! more than 12 on a channel; anything else is dropped at once. Over `max_frames` it thins
//! (see `thin_victim`), so memory is bounded by the cap whatever the length. Nothing is
//! encoded while recording. The collector only ever reads the tap, which never blocks its
//! producer: a collector that falls behind costs the pipeline nothing (the tap skips).
//!
//! The caller marks [`actions_started`](Recorder::actions_started) /
//! [`actions_ended`](Recorder::actions_ended) around its steps; with no actions it calls
//! `actions_ended` right after `start`. Nothing ends a recording before `actions_ended`,
//! [`step_failed`](Recorder::step_failed) or `finish`, however still the screen (the
//! caller may be slow to begin). Then, every ≤ 10 ms, the collector checks for the end, with
//! `due = t0 + time` and `quiet = max(action end, last frame arrival or t0)`. Actions
//! still running at `due` keep it going until they end (`extended`); then it ends the
//! normal way, with the time clock restarted at the action end so a screen that never
//! stills cannot run on:
//!
//! | ends | when (actions ended) |
//! |---|---|
//! | `Idle` | not `full`: nothing arrived for `idle` since `quiet` |
//! | `Time` | not `full`: `now ≥ due`, or `now ≥ action end + time` if extended |
//! | `Time` | `full`: `now ≥ max(due, action end)` (at once when extended) |
//! | `StepFailed` | `idle` after [`step_failed`](Recorder::step_failed) (a tail of what followed) |
//! | `Error` | the tap closed (the pipeline went away): "the frame stream ended" |
//!
//! [`finish`](Recorder::finish) waits for the end (it marks unfinished actions ended, so
//! it cannot wait on a mark that never comes), adds a last `capture` if it differs from
//! the last kept frame, then encodes the PNGs and the sheet. Whatever happened, it
//! returns a recording: at least the baseline.
use crate::draw::{Tile, TilePhase, contact_sheet, frame_png};
use crate::{AgentError, Device, changed, fingerprint_with, lock};
use serde::Serialize;
use simdriver::{Frame, FrameTap, TapHealth, TapRecv};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Change print: columns × rows of mean colour, finer than the settle print so a
/// spinner's few pixels count.
const CHANGE_PRINT: (usize, usize) = (32, 64);
/// Pixels read per cell: every 2nd each way.
const CHANGE_STEP: usize = 2;
/// The collector checks the end at least this often.
const TICK: Duration = Duration::from_millis(10);
/// Longest wait for the baseline frame.
const BASELINE_WAIT: Duration = Duration::from_secs(2);
/// Longest wait for the last frame, after the end.
const FINAL_WAIT: Duration = Duration::from_millis(200);
const MAX_TIME: Duration = Duration::from_secs(30);
const IDLE: std::ops::RangeInclusive<Duration> =
    Duration::from_millis(50)..=Duration::from_millis(5000);
const FPS: std::ops::RangeInclusive<u32> = 25..=30;

/// How long and how densely to record. [`Recorder::start`] applies [`clamped`](Self::clamped).
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// How long to record (at most 30 s); actions still running extend it.
    pub time: Duration,
    /// Without `full`, end once the actions ended and no frame arrived for this long.
    pub idle: Duration,
    /// Record the whole `time` (or until the actions end, if later), never ending idle.
    pub full: bool,
    /// Frames judged a second, 25..=30.
    pub fps: u32,
    /// Most frames kept (at least 2: the baseline and the newest).
    pub max_frames: usize,
}

impl Default for RecordOptions {
    fn default() -> Self {
        RecordOptions {
            time: Duration::from_secs(5),
            idle: Duration::from_millis(400),
            full: false,
            fps: 30,
            max_frames: 24,
        }
    }
}

impl RecordOptions {
    /// Within the limits: time ≤ 30 s, idle 50 ms..=5 s, fps 25..=30, max_frames ≥ 2.
    pub fn clamped(self) -> RecordOptions {
        RecordOptions {
            time: self.time.min(MAX_TIME),
            idle: self.idle.clamp(*IDLE.start(), *IDLE.end()),
            fps: self.fps.clamp(*FPS.start(), *FPS.end()),
            max_frames: self.max_frames.max(2),
            ..self
        }
    }
}

/// How a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Ended {
    /// The actions ended and the screen stayed still for `idle`.
    Idle,
    /// The time ran out (after any extension).
    Time,
    /// A step failed; this is one `idle` after it.
    StepFailed,
    /// The frame stream ended; see `error`.
    Error,
}

/// A kept frame. `t_ms` is when it arrived, from the baseline; `change` is the share of
/// change-print cells that changed from the previous frame the recorder kept at the time;
/// thinning can remove that frame.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedFrame {
    pub index: usize,
    pub t_ms: u64,
    pub change: f32,
    #[serde(skip)]
    pub png: Vec<u8>,
    /// Where the PNG was written, if it was.
    pub file: Option<String>,
}

/// The stream over the recording: frames received and sampled, frames the pipeline
/// coalesced, frames the recorder skipped because it was behind, and arrival gaps
/// (shorter than a pause) — a stutter is reported, not just seen.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordHealth {
    pub received: u64,
    pub sampled: u64,
    pub skipped_by_pipeline: u64,
    pub skipped_by_recorder: u64,
    pub max_gap_ms: f32,
    pub gaps_over_34ms: u32,
}

/// A finished recording. Times are ms from the baseline (`t = 0`), from host receive
/// instants (a few ms of transport, consistent with each other).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Recording {
    pub ended: Ended,
    /// Actions ran past `time`, and the recording with them.
    pub extended: bool,
    pub requested_ms: u64,
    pub duration_ms: u64,
    pub action_start_ms: Option<u64>,
    pub action_end_ms: Option<u64>,
    /// The last kept change; `None` = still changing when it ended.
    pub settled_ms: Option<u64>,
    pub frames: Vec<RecordedFrame>,
    pub health: RecordHealth,
    pub error: Option<String>,
    /// Contact sheets (PNG), usually one.
    #[serde(skip)]
    pub sheets: Vec<Vec<u8>>,
    pub sheet_files: Vec<String>,
}

/// A recording in progress. See the module docs.
pub struct Recorder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    t0: Instant,
    opts: RecordOptions,
}

#[derive(Default, Clone, Copy)]
struct Marks {
    start: Option<Instant>,
    end: Option<Instant>,
    /// When a failed step's tail is over.
    fail_end: Option<Instant>,
}

struct Kept {
    at: Instant,
    frame: Frame,
    print: Vec<[u8; 3]>,
    change: f32,
}

/// What the collector built; behind a lock so `finish` has it even if the thread died.
#[derive(Default)]
struct Collected {
    kept: Vec<Kept>,
    end: Option<(Ended, Instant)>,
    error: Option<String>,
    health: TapHealth,
}

struct Shared {
    marks: Mutex<Marks>,
    out: Mutex<Collected>,
    /// The recorder was dropped without `finish`: stop.
    stop: AtomicBool,
}

impl Recorder {
    /// Takes the baseline and starts collecting. `NotReady` without a frame or a tap.
    pub fn start<D: Device + ?Sized>(
        device: &D,
        opts: RecordOptions,
    ) -> Result<Recorder, AgentError> {
        let opts = opts.clamped();
        let baseline = device
            .capture(BASELINE_WAIT)
            .ok_or_else(|| AgentError::NotReady("no frame yet".into()))?;
        let t0 = Instant::now();
        let tap = device
            .frame_tap(opts.fps)
            .ok_or_else(|| AgentError::NotReady("this device cannot record".into()))?;
        let print = fingerprint_with(&baseline, CHANGE_PRINT, CHANGE_STEP);
        let shared = Arc::new(Shared {
            marks: Mutex::default(),
            out: Mutex::new(Collected {
                kept: vec![Kept {
                    at: t0,
                    frame: baseline,
                    print,
                    change: 0.0,
                }],
                ..Collected::default()
            }),
            stop: AtomicBool::new(false),
        });
        let (s, o) = (shared.clone(), opts.clone());
        let thread = thread::Builder::new()
            .name("simdriver-record".into())
            .spawn(move || collect(&tap, &s, t0, &o))
            .map_err(|e| AgentError::NotReady(format!("cannot start the recorder: {e}")))?;
        Ok(Recorder {
            shared,
            thread: Some(thread),
            t0,
            opts,
        })
    }

    /// The first action begins now.
    pub fn actions_started(&self) {
        let mut m = lock(&self.shared.marks);
        m.start.get_or_insert_with(Instant::now);
    }

    /// The last action returned now (with no actions: call it right after `start`).
    /// Until this, `step_failed` or `finish`, the recording does not end.
    pub fn actions_ended(&self) {
        lock(&self.shared.marks)
            .end
            .get_or_insert_with(Instant::now);
    }

    /// A step failed now: the actions are over, and the recording ends after one
    /// `idle` of what follows.
    pub fn step_failed(&self) {
        let mut m = lock(&self.shared.marks);
        let now = Instant::now();
        if m.start.is_some() {
            m.end.get_or_insert(now);
        }
        m.fail_end
            .get_or_insert(now.checked_add(self.opts.idle).unwrap_or(now));
    }

    /// Waits for the end, adds the last frame if it differs, and encodes.
    pub fn finish<D: Device + ?Sized>(mut self, device: &D) -> Recording {
        // Unfinished (or unmarked) actions end here, so the wait below is bounded by the
        // end rules.
        self.actions_ended();
        if let Some(t) = self.thread.take() {
            // A panicked collector still left what it kept in `out`.
            let _ = t.join();
        }
        let marks = *lock(&self.shared.marks);
        let Collected {
            mut kept,
            end,
            mut error,
            health,
        } = std::mem::take(&mut *lock(&self.shared.out));
        let (ended, end_at) = end.unwrap_or_else(|| {
            error.get_or_insert_with(|| "the recorder stopped".into());
            (Ended::Error, Instant::now())
        });
        if let Some(f) = device.capture(FINAL_WAIT)
            && let Some(last) = kept.last()
        {
            let print = fingerprint_with(&f, CHANGE_PRINT, CHANGE_STEP);
            let change = changed(&last.print, &print) as f32;
            if change > 0.0 {
                // After the last kept frame (times stay strictly increasing), by the end.
                let at = health
                    .last_arrival
                    .unwrap_or(end_at)
                    .max(last.at + Duration::from_millis(1))
                    .min(end_at);
                kept.push(Kept {
                    at,
                    frame: f,
                    print,
                    change,
                });
                thin(&mut kept, self.opts.max_frames);
            }
        }
        self.encode(kept, marks, ended, end_at, error, health)
    }

    fn encode(
        &self,
        kept: Vec<Kept>,
        marks: Marks,
        ended: Ended,
        end_at: Instant,
        error: Option<String>,
        health: TapHealth,
    ) -> Recording {
        let ms = |at: Instant| at.saturating_duration_since(self.t0).as_millis() as u64;
        let due = self.t0 + self.opts.time;
        let last_t = kept.last().map_or(0, |k| ms(k.at));
        let still_at_end = health
            .last_arrival
            .is_none_or(|a| end_at.saturating_duration_since(a) >= self.opts.idle);
        let settled_ms = match ended {
            Ended::Idle => Some(last_t),
            Ended::Time if still_at_end => Some(last_t),
            _ => None,
        };
        let frames = kept
            .iter()
            .enumerate()
            .map(|(index, k)| RecordedFrame {
                index,
                t_ms: ms(k.at),
                change: k.change,
                png: frame_png(&k.frame),
                file: None,
            })
            .collect::<Vec<_>>();
        let tiles: Vec<Tile> = kept
            .iter()
            .zip(&frames)
            .map(|(k, f)| Tile {
                frame: &k.frame,
                label: label(f.index, f.t_ms),
                phase: phase(f.index, k.at, (marks.start, marks.end)),
            })
            .collect();
        let sheets = contact_sheet(&tiles);
        Recording {
            ended,
            extended: marks.end.is_some_and(|e| e > due),
            requested_ms: self.opts.time.as_millis() as u64,
            duration_ms: ms(end_at),
            action_start_ms: marks.start.map(ms),
            action_end_ms: marks.end.map(ms),
            settled_ms,
            frames,
            health: RecordHealth {
                received: health.received,
                sampled: health.sampled,
                skipped_by_pipeline: health.skipped_by_pipeline,
                skipped_by_recorder: health.skipped_by_tap,
                max_gap_ms: health.max_gap_ms,
                gaps_over_34ms: health.gaps_over_34ms,
            },
            error,
            sheets,
            sheet_files: Vec::new(),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The collector thread: judge frames as they arrive until an end condition. The tap
/// is dropped on return, which detaches it from the pipeline.
fn collect(tap: &FrameTap, shared: &Shared, t0: Instant, opts: &RecordOptions) {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let closed = match tap.recv_timeout(TICK) {
            Ok(f) => {
                judge(shared, f.at, f.frame, opts.max_frames);
                false
            }
            Err(TapRecv::Timeout) => false,
            Err(TapRecv::Closed) => true,
        };
        let health = tap.health();
        let now = Instant::now();
        let end = if closed {
            Some(Ended::Error)
        } else {
            let marks = *lock(&shared.marks);
            end_rule(&marks, t0, opts, health.last_arrival, now)
        };
        if let Some(ended) = end {
            let mut out = lock(&shared.out);
            out.end = Some((ended, now));
            out.health = health;
            if closed {
                out.error = Some("the frame stream ended".into());
            }
            return;
        }
    }
}

/// Keep the frame if it differs from the last kept one; thin over the cap.
fn judge(shared: &Shared, at: Instant, frame: Frame, max_frames: usize) {
    // Printed outside the lock (the only other user is `finish`, after the end).
    let print = fingerprint_with(&frame, CHANGE_PRINT, CHANGE_STEP);
    let mut out = lock(&shared.out);
    let change = out
        .kept
        .last()
        .map_or(1.0, |last| changed(&last.print, &print) as f32);
    if change > 0.0 {
        out.kept.push(Kept {
            at,
            frame,
            print,
            change,
        });
        thin(&mut out.kept, max_frames);
    }
}

fn thin(kept: &mut Vec<Kept>, max_frames: usize) {
    while kept.len() > max_frames.max(2) {
        let ts: Vec<Instant> = kept.iter().map(|k| k.at).collect();
        match thin_victim(&ts) {
            // Dropping the frame frees it at once.
            Some(i) => drop(kept.remove(i)),
            None => return,
        }
    }
}

/// Which kept frame to drop to thin: never the first (the baseline) or the last (the
/// newest); the one whose removal leaves the smallest gap between its neighbours, the
/// earliest on a tie. Repeated, this keeps the frames evenly spaced.
fn thin_victim(ts: &[Instant]) -> Option<usize> {
    (1..ts.len().saturating_sub(1)).min_by_key(|&i| ts[i + 1].saturating_duration_since(ts[i - 1]))
}

/// The end condition reached at `now`, if any (see the module docs). Nothing ends before
/// the actions are marked ended (bar a failed step's tail). Once they end, `full` stops at
/// `max(due, action end)`; otherwise a still `idle` ends it, or the time: `due`, or
/// `action end + time` when the actions outran `due` (the extension restarts the clock).
fn end_rule(
    m: &Marks,
    t0: Instant,
    opts: &RecordOptions,
    last_arrival: Option<Instant>,
    now: Instant,
) -> Option<Ended> {
    if let Some(fail_end) = m.fail_end {
        return (now >= fail_end).then_some(Ended::StepFailed);
    }
    let action_end = m.end?;
    let due = t0 + opts.time;
    if opts.full {
        return (now >= due.max(action_end)).then_some(Ended::Time);
    }
    let quiet = action_end.max(last_arrival.unwrap_or(t0));
    let time_up = if action_end > due {
        action_end.checked_add(opts.time).unwrap_or(action_end)
    } else {
        due
    };
    if now.saturating_duration_since(quiet) >= opts.idle {
        Some(Ended::Idle)
    } else if now >= time_up {
        Some(Ended::Time)
    } else {
        None
    }
}

/// A frame's place relative to the action marks: the baseline and anything before the
/// start is before; from start to end, during; after the end (or with no actions), after.
fn phase(index: usize, at: Instant, (start, end): (Option<Instant>, Option<Instant>)) -> TilePhase {
    match (start, end) {
        _ if index == 0 => TilePhase::Before,
        (Some(s), _) if at < s => TilePhase::Before,
        (Some(_), Some(e)) if at > e => TilePhase::After,
        (Some(_), _) => TilePhase::During,
        (None, _) => TilePhase::After,
    }
}

fn label(index: usize, t_ms: u64) -> String {
    format!("#{index} +{t_ms}ms")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::TilePhase;
    use crate::lock;
    use crate::marks::tests::settings;
    use crate::tests::Fake;
    use simdriver::Frame;
    use std::sync::atomic::Ordering;
    use std::thread;

    const MS: Duration = Duration::from_millis(1);

    /// A 64×128 frame, grey, with the 2×2 block of change-print cell `i` lit: any two
    /// different `i` (below 2048) differ in at least one cell.
    fn distinct(i: usize) -> Frame {
        let (w, h) = (64usize, 128usize);
        let mut data = vec![60u8; w * h * 4];
        let (cx, cy) = ((i % 32) * 2, (i / 32 % 64) * 2);
        for y in cy..cy + 2 {
            for x in cx..cx + 2 {
                data[(y * w + x) * 4..(y * w + x) * 4 + 3].fill(250);
            }
        }
        Frame {
            width: w as u32,
            height: h as u32,
            source_width: w as u32,
            source_height: h as u32,
            seq: 2 + 2 * i as u64,
            data,
        }
    }

    /// A Fake whose newest frame is `distinct(0)` (the baseline).
    fn device() -> Fake {
        let fake = Fake::new(settings());
        *lock(&fake.frame) = Some(distinct(0));
        fake
    }

    fn opts(time_ms: u64, idle_ms: u64) -> RecordOptions {
        RecordOptions {
            time: Duration::from_millis(time_ms),
            idle: Duration::from_millis(idle_ms),
            ..RecordOptions::default()
        }
    }

    fn start(fake: &Fake, o: RecordOptions) -> Recorder {
        Recorder::start(fake, o).unwrap_or_else(|e| panic!("start: {e}"))
    }

    /// A recording with no actions: marked ended right after `start`.
    fn start_still(fake: &Fake, o: RecordOptions) -> Recorder {
        let rec = start(fake, o);
        rec.actions_ended();
        rec
    }

    #[test]
    fn static_screen_ends_idle_with_baseline_only() {
        let fake = device();
        let r = start_still(&fake, opts(5000, 50)).finish(&fake);
        assert_eq!(r.ended, Ended::Idle);
        assert_eq!(r.frames.len(), 1);
        assert_eq!((r.frames[0].index, r.frames[0].t_ms), (0, 0));
        assert!((50..400).contains(&r.duration_ms), "{}", r.duration_ms);
        assert_eq!(r.settled_ms, Some(0));
        assert!(!r.extended);
        assert_eq!(r.action_start_ms, None);
        assert!(r.action_end_ms.is_some_and(|e| e < 50));
    }

    #[test]
    fn change_then_still_ends_idle_and_settles() {
        let fake = device();
        let rec = start_still(&fake, opts(5000, 200));
        // 40 ms apart (and from the baseline): every one is due at 30 fps.
        for i in 1..=3 {
            thread::sleep(40 * MS);
            fake.feed(Instant::now(), distinct(i));
        }
        let r = rec.finish(&fake);
        assert_eq!(r.ended, Ended::Idle);
        assert_eq!(r.frames.len(), 4);
        let ts: Vec<u64> = r.frames.iter().map(|f| f.t_ms).collect();
        assert!(ts.windows(2).all(|w| w[0] < w[1]), "{ts:?}");
        assert!(r.frames.iter().enumerate().all(|(i, f)| f.index == i));
        assert!(r.frames[1..].iter().all(|f| f.change > 0.0));
        assert_eq!(r.settled_ms, Some(ts[3]));
        assert!(r.duration_ms >= ts[3] + 200, "{} vs {ts:?}", r.duration_ms);
        assert_eq!((r.health.received, r.health.sampled), (3, 3));
        // Encoded after the end: every frame a PNG, one sheet.
        assert!(r.frames.iter().all(|f| f.png.starts_with(b"\x89PNG")));
        assert_eq!(r.sheets.len(), 1);
    }

    #[test]
    fn identical_frames_are_not_kept() {
        let fake = device();
        let rec = start_still(&fake, opts(5000, 150));
        for i in [0, 0, 1, 1, 1] {
            fake.feed(Instant::now(), distinct(i));
            thread::sleep(40 * MS);
        }
        let r = rec.finish(&fake);
        assert_eq!(r.frames.len(), 2, "baseline and the one change");
        assert_eq!(r.health.received, 5);
    }

    #[test]
    fn never_still_ends_at_time_unsettled() {
        let fake = device();
        let o = RecordOptions {
            max_frames: 4,
            ..opts(400, 200)
        };
        let r = thread::scope(|s| {
            let rec = start_still(&fake, o);
            let began = Instant::now();
            let feeder = &fake;
            s.spawn(move || feed_until(feeder, began, 800 * MS));
            rec.finish(&fake)
        });
        assert_eq!(r.ended, Ended::Time);
        assert_eq!(r.settled_ms, None);
        assert!(r.frames.len() <= 4, "{}", r.frames.len());
        assert_eq!(r.frames[0].t_ms, 0, "the baseline is never thinned");
        assert!((400..1200).contains(&r.duration_ms), "{}", r.duration_ms);
        assert!(r.health.received > r.health.sampled, "sampled at 30 fps");
    }

    #[test]
    fn thinning_keeps_baseline_newest_and_even_spacing() {
        let t0 = Instant::now();
        let mut kept: Vec<Instant> = Vec::new();
        for i in 0..40u32 {
            kept.push(t0 + i * 10 * MS);
            if kept.len() > 24 {
                let v = thin_victim(&kept).expect("a frame to drop");
                assert!(v != 0 && v != kept.len() - 1);
                kept.remove(v);
            }
        }
        assert_eq!(kept.len(), 24);
        assert_eq!(kept[0], t0);
        assert_eq!(kept[23], t0 + 390 * MS);
        let gaps: Vec<Duration> = kept.windows(2).map(|w| w[1] - w[0]).collect();
        let (min, max) = (gaps.iter().min().unwrap(), gaps.iter().max().unwrap());
        assert!(*max <= *min * 2, "gaps {gaps:?}");
        // Nothing to drop between two frames.
        assert_eq!(thin_victim(&kept[..2]), None);
    }

    #[test]
    fn actions_past_time_extend_the_recording() {
        let fake = device();
        let rec = start(&fake, opts(100, 50));
        rec.actions_started();
        thread::sleep(300 * MS);
        rec.actions_ended();
        let r = rec.finish(&fake);
        // Idle (50 ms) passed long before: the idle clock starts at the actions' end, and
        // the extended recording ends the normal way, after a still tail.
        assert_eq!(r.ended, Ended::Idle);
        assert!(r.extended);
        assert_eq!(r.requested_ms, 100);
        let end = r.action_end_ms.unwrap();
        assert!(end >= 300);
        assert!(
            (end + 50..end + 400).contains(&r.duration_ms),
            "{} vs {end}",
            r.duration_ms
        );
        assert!(r.action_start_ms.unwrap() < 50);
        assert_eq!(r.settled_ms, Some(0));
    }

    /// Feeds a distinct frame every 10 ms until `until` after `from`.
    fn feed_until(fake: &Fake, from: Instant, until: Duration) {
        let mut i = 1;
        while from.elapsed() < until {
            fake.feed(Instant::now(), distinct(i));
            i += 1;
            thread::sleep(10 * MS);
        }
    }

    #[test]
    fn extended_recording_waits_for_still() {
        // Actions end at 800 ms, past `time` (600 ms); frames run on to 1000 ms. The idle
        // end (~1200 ms) comes before the restarted clock (800 + 600 = 1400 ms).
        let fake = device();
        let r = thread::scope(|s| {
            let rec = start(&fake, opts(600, 200));
            let began = Instant::now();
            rec.actions_started();
            let feeder = &fake;
            s.spawn(move || feed_until(feeder, began, 1000 * MS));
            thread::sleep(800 * MS);
            rec.actions_ended();
            rec.finish(&fake)
        });
        assert_eq!(r.ended, Ended::Idle);
        assert!(r.extended);
        assert!(r.duration_ms >= 1000, "{}", r.duration_ms);
        assert!(r.settled_ms.is_some());
    }

    #[test]
    fn extended_never_still_ends_at_action_end_plus_time() {
        let fake = device();
        // Actions end at 300 ms, past `time` (200 ms); frames never stop (to 1200 ms):
        // it ends at the restarted clock, 300 + 200 ms.
        let r = thread::scope(|s| {
            let rec = start(&fake, opts(200, 200));
            let began = Instant::now();
            rec.actions_started();
            let feeder = &fake;
            s.spawn(move || feed_until(feeder, began, 1200 * MS));
            thread::sleep(300 * MS);
            rec.actions_ended();
            rec.finish(&fake)
        });
        assert_eq!(r.ended, Ended::Time);
        assert!(r.extended);
        let end = r.action_end_ms.unwrap();
        assert!(end >= 300);
        assert!(
            (end + 200..end + 600).contains(&r.duration_ms),
            "{} vs {end}",
            r.duration_ms
        );
        assert_eq!(r.settled_ms, None);
    }

    #[test]
    fn extended_full_stops_at_action_end() {
        let fake = device();
        let o = RecordOptions {
            full: true,
            ..opts(100, 400)
        };
        let rec = start(&fake, o);
        rec.actions_started();
        thread::sleep(300 * MS);
        rec.actions_ended();
        let r = rec.finish(&fake);
        assert_eq!(r.ended, Ended::Time);
        assert!(r.extended);
        let end = r.action_end_ms.unwrap();
        assert!(end >= 300);
        // At once: no idle tail (400 ms).
        assert!(
            (end..end + 300).contains(&r.duration_ms),
            "{} vs {end}",
            r.duration_ms
        );
    }

    #[test]
    fn does_not_end_before_actions_are_marked() {
        // A still screen with no marks for 3 × idle: the caller is slow to begin.
        let fake = device();
        let rec = start(&fake, opts(5000, 50));
        thread::sleep(150 * MS);
        rec.actions_started();
        thread::sleep(20 * MS);
        rec.actions_ended();
        let r = rec.finish(&fake);
        assert_eq!(r.ended, Ended::Idle);
        let (begin, end) = (r.action_start_ms.unwrap(), r.action_end_ms.unwrap());
        assert!(begin >= 150 && end >= begin, "{begin} {end}");
        assert!(r.duration_ms >= end + 50, "{} vs {end}", r.duration_ms);
        assert!(!r.extended);
    }

    #[test]
    fn full_records_the_whole_time() {
        let fake = device();
        let o = RecordOptions {
            full: true,
            ..opts(200, 50)
        };
        let r = start_still(&fake, o).finish(&fake);
        assert_eq!(r.ended, Ended::Time);
        assert!(!r.extended);
        assert!((200..600).contains(&r.duration_ms), "{}", r.duration_ms);
        assert_eq!(r.settled_ms, Some(0), "nothing arrived in the final idle");
    }

    #[test]
    fn step_failed_keeps_a_tail() {
        let fake = device();
        let rec = start(&fake, opts(5000, 100));
        let started = Instant::now();
        rec.actions_started();
        thread::sleep(30 * MS);
        let failed = Instant::now();
        rec.step_failed();
        thread::sleep(30 * MS);
        fake.feed(Instant::now(), distinct(1));
        let r = rec.finish(&fake);
        assert_eq!(r.ended, Ended::StepFailed);
        assert_eq!(r.frames.len(), 2, "the frame after the failure is kept");
        let tail = (failed - started).as_millis() as u64 + 100;
        assert!(
            (tail..tail + 400).contains(&r.duration_ms),
            "{} vs {tail}",
            r.duration_ms
        );
        assert_eq!(r.settled_ms, None);
        assert!(
            r.action_end_ms.is_some(),
            "the failing step ended the actions"
        );
    }

    #[test]
    fn closed_tap_ends_with_error_and_keeps_frames() {
        let fake = device();
        let rec = start(&fake, opts(5000, 1000));
        for i in 1..=2 {
            fake.feed(Instant::now(), distinct(i));
            thread::sleep(40 * MS);
        }
        *lock(&fake.tap) = None;
        let began = Instant::now();
        let r = rec.finish(&fake);
        assert!(began.elapsed() < Duration::from_millis(500), "ends at once");
        assert_eq!(r.ended, Ended::Error);
        assert_eq!(r.error.as_deref(), Some("the frame stream ended"));
        assert_eq!(r.frames.len(), 3);
        assert_eq!(r.settled_ms, None);
        assert_eq!(r.sheets.len(), 1);
    }

    #[test]
    fn final_capture_is_appended_when_it_differs() {
        let fake = device();
        let rec = start_still(&fake, opts(5000, 50));
        // The newest frame changes without arriving through the tap.
        *lock(&fake.frame) = Some(distinct(7));
        let r = rec.finish(&fake);
        assert_eq!(r.ended, Ended::Idle);
        assert_eq!(r.frames.len(), 2);
        assert!(r.frames[1].change > 0.0);
        assert_eq!(r.settled_ms, Some(r.frames[1].t_ms));
        assert!(r.frames[1].t_ms > r.frames[0].t_ms && r.frames[1].t_ms <= r.duration_ms);

        // Unchanged: not appended.
        let fake = device();
        let r = start_still(&fake, opts(5000, 50)).finish(&fake);
        assert_eq!(r.frames.len(), 1);
    }

    #[test]
    fn start_without_tap_is_not_ready() {
        let fake = device();
        fake.tapless.store(true, Ordering::SeqCst);
        match Recorder::start(&fake, RecordOptions::default()) {
            Err(AgentError::NotReady(m)) => assert_eq!(m, "this device cannot record"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("started without a tap"),
        }
        let fake = Fake::new(settings());
        match Recorder::start(&fake, RecordOptions::default()) {
            Err(AgentError::NotReady(m)) => assert_eq!(m, "no frame yet"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("started without a frame"),
        }
    }

    #[test]
    fn options_are_clamped() {
        let d = RecordOptions::default();
        assert_eq!(
            (d.time, d.idle, d.full, d.fps, d.max_frames),
            (Duration::from_secs(5), 400 * MS, false, 30, 24)
        );
        let c = RecordOptions {
            time: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            fps: 60,
            max_frames: 0,
            ..d.clone()
        }
        .clamped();
        assert_eq!(
            (c.time, c.idle, c.fps, c.max_frames),
            (Duration::from_secs(30), Duration::from_secs(5), 30, 2)
        );
        let c = RecordOptions {
            fps: 10,
            idle: Duration::ZERO,
            ..d.clone()
        }
        .clamped();
        assert_eq!((c.fps, c.idle), (25, 50 * MS));
        assert_eq!(d.clone().clamped().idle, d.idle, "in range: unchanged");
    }

    #[test]
    fn labels_and_phases_follow_the_action_marks() {
        let t0 = Instant::now();
        let at = |ms: u32| t0 + ms * MS;
        let marks = (Some(at(100)), Some(at(300)));
        let phases: Vec<TilePhase> = [0, 50, 100, 200, 300, 301]
            .into_iter()
            .enumerate()
            .map(|(i, ms)| phase(i, at(ms), marks))
            .collect();
        use TilePhase::*;
        assert_eq!(phases, [Before, Before, During, During, During, After]);
        // No actions: the baseline is before, everything else after.
        assert_eq!(phase(0, t0, (None, None)), Before);
        assert_eq!(phase(1, at(5), (None, None)), After);
        assert_eq!(label(4, 250), "#4 +250ms");
    }
}
