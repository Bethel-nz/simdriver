//! Shared helpers for the integration tests that need a booted simulator.
//! Each test binary pulls in only part of this module.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The simulator to run against, or `None` (and a skip note) unless `SIMDRIVER_IT=1`.
/// `SIMDRIVER_TEST_UDID` picks the device; otherwise the first booted one.
pub fn booted_udid() -> Option<String> {
    if std::env::var("SIMDRIVER_IT").as_deref() != Ok("1") {
        eprintln!("skipping: set SIMDRIVER_IT=1 to run simulator integration tests");
        return None;
    }
    if let Ok(udid) = std::env::var("SIMDRIVER_TEST_UDID") {
        if !udid.is_empty() {
            return Some(udid);
        }
    }
    let out = Command::new("xcrun")
        .args(["simctl", "list", "devices", "booted", "-j"])
        .output()
        .expect("run xcrun simctl list");
    assert!(out.status.success(), "simctl list failed");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("simctl json");
    let devices = v["devices"].as_object().expect("devices map");
    for list in devices.values() {
        for d in list.as_array().into_iter().flatten() {
            if d["state"] == "Booted" {
                if let Some(udid) = d["udid"].as_str() {
                    return Some(udid.to_string());
                }
            }
        }
    }
    panic!("SIMDRIVER_IT=1 but no simulator is booted");
}

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A tiny HTTP server serving one fixture on a free localhost port. Stops on drop.
pub struct Server {
    pub port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // unblock accept()
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const PREFERRED_PORT: u16 = 47811;

/// Serve `fixture` on a free localhost port and open it in the simulator's Safari.
pub fn open_page(udid: &str, fixture: &str) -> Server {
    let body = std::fs::read(fixture_path(fixture)).expect("read fixture");
    // Prefer one stable port: re-opening the same URL reuses Safari's tab, while every
    // new origin opens another tab and stalls its animation for several seconds.
    let listener = TcpListener::bind(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        }
    });
    let url = format!("http://localhost:{port}/");
    let st = Command::new("xcrun")
        .args(["simctl", "openurl", udid, &url])
        .status()
        .expect("run simctl openurl");
    assert!(st.success(), "simctl openurl {url} failed");
    Server { port, stop, thread: Some(thread) }
}

/// Delivery statistics over a run of frame arrival times.
#[derive(Debug, Clone, Copy)]
pub struct FrameStats {
    pub frames: usize,
    pub fps: f64,
    pub p50_gap_ms: f64,
    pub p99_gap_ms: f64,
}

impl FrameStats {
    pub fn from_times(times: &[Instant]) -> FrameStats {
        if times.len() < 2 {
            return FrameStats { frames: times.len(), fps: 0.0, p50_gap_ms: 0.0, p99_gap_ms: 0.0 };
        }
        let span = times[times.len() - 1].duration_since(times[0]).as_secs_f64();
        let mut gaps: Vec<f64> =
            times.windows(2).map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0).collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| gaps[(((gaps.len() - 1) as f64) * p).round() as usize];
        FrameStats {
            frames: times.len(),
            fps: (times.len() - 1) as f64 / span,
            p50_gap_ms: pct(0.50),
            p99_gap_ms: pct(0.99),
        }
    }
}
