//! `record` against a real booted simulator, in Safari on the core fixtures. Runs only
//! with SIMDRIVER_IT=1 and while no host streams the device (`support::input_allowed`);
//! point it at a spare simulator with SIMDRIVER_TEST_UDID. Run with `--test-threads=1`.
#[path = "../../simdriver/tests/support/mod.rs"]
mod support;

use simdriver::{DeviceSelector, Driver, Event, Options, State};
use simdriver_agent::{Agent, Ended, RecordOptions, Recording, Step};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LIVE_WAIT: Duration = Duration::from_secs(40);

#[derive(Default)]
struct Log {
    frames: Vec<Instant>,
    /// Latched: a frame showed tap.html's white bar (the page really loaded).
    bar: bool,
}

/// True when the frame has a white band a few rows tall with black above and below it
/// (tap.html's bar; Safari's black loading canvas and an all-white page have none).
fn frame_shows_bar(f: &simdriver::Frame) -> bool {
    let (w, h) = (f.width as usize, f.height as usize);
    let lum = |i: usize| {
        let o = i * 4;
        (f.data[o] as u32 + f.data[o + 1] as u32 + f.data[o + 2] as u32) / 3
    };
    let row_is = |row: usize, white: bool| {
        let samples = (w / 20).max(8);
        (0..samples)
            .map(|s| lum(row * w + s * (w - 1) / samples))
            .filter(|&l| if white { l > 215 } else { l < 60 })
            .count()
            * 20
            >= samples * 19
    };
    let mut white = 0;
    for row in (100..h.saturating_sub(100)).step_by(2) {
        if row_is(row, true) {
            white += 1;
            if white >= 3
                && row >= 108
                && row + 8 < h
                && row_is(row - 8, false)
                && row_is(row + 8, false)
            {
                return true;
            }
        } else {
            white = 0;
        }
    }
    false
}

struct Rig {
    agent: Agent<Driver>,
    log: Arc<Mutex<Log>>,
    /// The cell at the screen centre.
    centre: String,
    _server: support::Server,
}

fn uptime() -> String {
    std::process::Command::new("uptime")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Open `fixture`, start a driver, wait until it is live and the page is up.
/// `None` (a skip) unless SIMDRIVER_IT=1 and nobody is driving the device. The guard runs
/// before the driver starts, whose own helpers would otherwise trip it.
fn rig(fixture: &str) -> Option<Rig> {
    let udid = support::booted_udid()?;
    if !support::input_allowed(&udid) {
        return None;
    }
    let server = support::open_page(&udid, fixture);
    let log = Arc::new(Mutex::new(Log::default()));
    let l = log.clone();
    let tap_page = fixture == "tap.html";
    let d = Driver::start(
        Options {
            device: DeviceSelector::Udid(udid),
            ..Options::default()
        },
        move |ev| {
            if let Event::Frame(f) = ev {
                let mut l = l.lock().unwrap();
                l.frames.push(Instant::now());
                if tap_page && !l.bar {
                    l.bar = frame_shows_bar(&f);
                }
            }
        },
    )
    .unwrap_or_else(|e| panic!("Driver::start: {e}"));
    let t0 = Instant::now();
    while d.state() != State::Live {
        assert!(t0.elapsed() < LIVE_WAIT, "never live");
        std::thread::sleep(Duration::from_millis(50));
    }
    let t0 = Instant::now();
    loop {
        let ready = {
            let l = log.lock().unwrap();
            if tap_page {
                l.bar
            } else {
                // Streaming steadily, not just started: Safari's animation stalls for a
                // few hundred ms after a cold launch, which would end a recording idle.
                // Over the last 2 s: >= 100 frames and no gap over 120 ms.
                let now = Instant::now();
                let cut = now - Duration::from_secs(2);
                let recent: Vec<Instant> = l
                    .frames
                    .iter()
                    .rev()
                    .take_while(|t| **t >= cut)
                    .copied()
                    .collect();
                recent.len() >= 100
                    && recent
                        .windows(2)
                        .all(|w| w[0] - w[1] <= Duration::from_millis(120))
                    && now - recent[0] <= Duration::from_millis(120)
            }
        };
        if ready {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(40),
            "{fixture} never came up"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let centre = d
        .grid(simdriver::Grid::DEFAULT_COLS)
        .expect("grid")
        .at(0.5, 0.5);
    Some(Rig {
        agent: Agent::new(d),
        log,
        centre,
        _server: server,
    })
}

fn opts(time_ms: u64) -> RecordOptions {
    RecordOptions {
        time: Duration::from_millis(time_ms),
        ..RecordOptions::default()
    }
}

fn summary(name: &str, r: &Recording) {
    eprintln!(
        "{name}: ended {:?} extended {} duration {} ms, frames kept {}, settled {:?}, action {:?}..{:?}, health {:?}",
        r.ended,
        r.extended,
        r.duration_ms,
        r.frames.len(),
        r.settled_ms,
        r.action_start_ms,
        r.action_end_ms,
        r.health
    );
}

/// Mean luminance (0..255) of a PNG.
fn mean_luma(png_bytes: &[u8]) -> f64 {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png data");
    let n = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        c => panic!("unexpected png colour type {c:?}"),
    };
    let px = &buf[..info.buffer_size()];
    let (mut sum, mut count) = (0u64, 0u64);
    for p in px.chunks_exact(n) {
        sum += if n >= 3 {
            (u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) as u64 / 3
        } else {
            u64::from(p[0])
        };
        count += 1;
    }
    sum as f64 / count as f64
}

#[test]
fn it_record_static_screen_ends_idle() {
    let Some(rig) = rig("tap.html") else { return };
    let report = rig.agent.record(opts(5000), &[]);
    let rec = report.record.expect("a recording");
    summary("static", &rec);
    assert!(report.ok, "{:?}", report.failed);
    assert_eq!(rec.ended, Ended::Idle);
    assert_eq!(rec.frames.len(), 1);
    // idle (400 ms default) + 300 ms of slack.
    assert!(
        rec.duration_ms <= 700,
        "ended after {} ms; {}",
        rec.duration_ms,
        uptime()
    );
}

#[test]
fn it_record_tap_shows_the_change() {
    let Some(rig) = rig("tap.html") else { return };
    let report = rig.agent.record(
        opts(5000),
        &[Step::Tap {
            target: rig.centre.clone(),
        }],
    );
    let rec = report.record.expect("a recording");
    summary("tap", &rec);
    assert!(report.ok, "{:?}", report.failed);
    assert_eq!(rec.ended, Ended::Idle);
    assert!(rec.frames.len() >= 2, "{} frames", rec.frames.len());
    assert!(rec.settled_ms.is_some(), "never settled");
    assert!(rec.action_end_ms.expect("action end") <= rec.duration_ms);
    // tap.html is white only while touched and black again at touchend, so a tap leaves a
    // black last frame and the white flash lasts about one sampled frame: whether it was
    // caught is reported, not asserted (the long-press test asserts a held white frame).
    let lumas: Vec<f64> = rec.frames.iter().map(|f| mean_luma(&f.png)).collect();
    eprintln!(
        "tap frame luminance: {lumas:?} at {:?} ms (white flash caught: {})",
        rec.frames.iter().map(|f| f.t_ms).collect::<Vec<_>>(),
        lumas.iter().any(|l| *l > 200.0)
    );
}

#[test]
fn it_record_animation_hits_time_capped() {
    let Some(rig) = rig("anim.html") else { return };
    let report = rig.agent.record(opts(2000), &[]);
    let rec = report.record.expect("a recording");
    summary("anim", &rec);
    assert!(report.ok, "{:?}", report.failed);
    assert_eq!(rec.ended, Ended::Time);
    assert_eq!(rec.settled_ms, None);
    assert!(
        (2..=24).contains(&rec.frames.len()),
        "{} frames kept",
        rec.frames.len()
    );
    assert!(
        (45..=62).contains(&rec.health.sampled),
        "sampled {}; {}",
        rec.health.sampled,
        uptime()
    );
}

#[test]
fn it_record_extends_past_time() {
    let Some(rig) = rig("tap.html") else { return };
    let report = rig.agent.record(
        opts(500),
        &[Step::LongPress {
            target: rig.centre.clone(),
            ms: Some(1500),
        }],
    );
    let rec = report.record.expect("a recording");
    summary("extends", &rec);
    assert!(report.ok, "{:?}", report.failed);
    assert!(rec.extended);
    assert!(rec.duration_ms >= 1500, "{} ms", rec.duration_ms);
    // The page is white for the whole hold: a kept frame must show it.
    let lumas: Vec<f64> = rec.frames.iter().map(|f| mean_luma(&f.png)).collect();
    eprintln!("long-press frame luminance: {lumas:?}");
    assert!(
        lumas.iter().any(|l| *l > 200.0),
        "no white frame among {lumas:?}"
    );
}

#[test]
fn it_recording_does_not_slow_the_stream() {
    let Some(rig) = rig("anim.html") else { return };
    let mut o = opts(3000);
    o.full = true;
    let t0 = Instant::now();
    let report = rig.agent.record(o, &[]);
    let rec = report.record.expect("a recording");
    summary("stream", &rec);
    // The recording's own span; encoding the sheets afterwards is not recording load.
    let end = t0 + Duration::from_millis(rec.duration_ms);
    let times = rig.log.lock().unwrap().frames.clone();
    let stats = support::FrameStats::over_window(&times, t0, end);
    eprintln!("stream during recording: {stats:?}");
    let load = uptime();
    let r = std::panic::catch_unwind(|| stats.assert_live_60());
    if let Err(e) = r {
        let msg = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        panic!("{msg}; host load: {load}");
    }
}
