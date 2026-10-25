//! sdframe against a real booted simulator. Runs only with SIMDRIVER_IT=1.
//! Other frame consumers (e.g. pip-mini's simframe) may share the device; that is fine.
mod support;

use simdriver::__internal::{FrameMsg, ShmReader, parse_frame_line};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use support::FrameStats;

enum Ev {
    Shm { name: String, cap: u64 },
    Frame { at: Instant, dims: Option<(u32, u32, u32, u32)> },
    Garbage(String),
}

/// Kills the helper if the test bails out early, so a failing run never orphans it.
struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn it_sdframe_60fps() {
    let Some(udid) = support::booted_udid() else { return };
    let _server = support::open_page(&udid, "anim.html");
    std::thread::sleep(Duration::from_secs(2)); // let Safari load and start animating

    let bin = simdriver::baked_helper_dir().join("sdframe");
    let mut child = Reap(
        Command::new(&bin)
            .args([udid.as_str(), "--fps", "60", "--scale", "2"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sdframe"),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("{line}");
        }
    });

    // Reader thread: parse each line and copy the frame out of shm right away, the way the
    // driver will, so a slot is read before the writer laps it.
    let (tx, rx) = mpsc::channel::<Ev>();
    std::thread::spawn(move || {
        let mut out = BufReader::new(stdout);
        let mut reader: Option<ShmReader> = None;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match out.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf).into_owned();
            let ev = match parse_frame_line(&line) {
                Some(FrameMsg::Shm { name, slots, cap }) => {
                    reader = Some(ShmReader::open(&name, slots, cap).expect("open announced shm"));
                    Ev::Shm { name, cap }
                }
                Some(FrameMsg::Frame { slot, seq }) => {
                    let at = Instant::now();
                    let f = reader.as_ref().expect("frame before shm").read(slot, seq);
                    if let Some(f) = &f {
                        assert_eq!(f.seq, seq);
                        assert_eq!(f.data.len(), (f.width * f.height * 4) as usize);
                    }
                    Ev::Frame { at, dims: f.map(|f| (f.width, f.height, f.source_width, f.source_height)) }
                }
                Some(FrameMsg::Note(_)) => continue,
                None => Ev::Garbage(line.chars().take(80).collect()),
            };
            if tx.send(ev).is_err() {
                break;
            }
        }
    });

    // A freshly opened origin animates for a moment, then Safari stalls it for several
    // seconds (seen with and without sdframe attached). So measure 5 contiguous seconds
    // starting once the stream is live: >= 25 frames within the trailing 500 ms.
    let started = Instant::now();
    let mut names: Vec<(String, u64)> = Vec::new();
    let mut warm: Vec<Instant> = Vec::new();
    let mut times: Vec<Instant> = Vec::new();
    let mut torn = 0usize;
    let mut window_end: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let deadline = window_end.unwrap_or(started + Duration::from_secs(40));
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Ev::Shm { name, cap }) => names.push((name, cap)),
            Ok(Ev::Frame { at, dims }) => {
                let Some((w, h, sw, sh)) = dims else {
                    torn += 1;
                    continue;
                };
                if sw == 1206 && sh == 2622 {
                    assert_eq!((w, h), (603, 1311), "iPhone 18 Pro at --scale 2");
                }
                assert_eq!((w, h), (sw / 2, sh / 2), "--scale 2 halves the source");
                if window_end.is_some() {
                    times.push(at);
                    continue;
                }
                warm.push(at);
                warm.retain(|t| at.duration_since(*t) <= Duration::from_millis(500));
                if warm.len() >= 25 {
                    eprintln!("stream live after {:.1} s; measuring 5 s", started.elapsed().as_secs_f64());
                    window_end = Some(at + Duration::from_secs(5));
                    times.push(at);
                }
            }
            Ok(Ev::Garbage(l)) => panic!("stdout carried a non-protocol line: {l:?}"),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("sdframe stdout closed early"),
        }
    }
    assert!(window_end.is_some(), "frame stream never reached a live rate within 40 s (page not animating?)");

    let stats = FrameStats::from_times(&times);
    eprintln!("sdframe stats: {stats:?}, torn/overwritten reads: {torn}, segments: {names:?}");
    assert!(!names.is_empty(), "no shm announced");
    assert!(stats.frames > 0, "no frames received");
    assert!(stats.fps >= 55.0, "fps {:.1} < 55", stats.fps);
    assert!(stats.p99_gap_ms <= 34.0, "p99 gap {:.1} ms > 34", stats.p99_gap_ms);

    // Closing stdin must stop the helper within 1 s and unlink every segment.
    drop(child.0.stdin.take());
    let closed = Instant::now();
    let status = loop {
        if let Some(st) = child.0.try_wait().unwrap() {
            break st;
        }
        assert!(closed.elapsed() < Duration::from_secs(1), "sdframe still running 1 s after stdin EOF");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(0));
    for (name, cap) in &names {
        assert!(ShmReader::open(name, 3, *cap).is_err(), "{name} still opens after exit");
    }
}
