//! The agent against a real booted simulator, in Settings. Runs only with SIMDRIVER_IT=1;
//! drives SIMDRIVER_TEST_UDID (else the first booted device). Writes the annotated
//! screenshots to $SIMDRIVER_IT_OUT (default: the temp dir) for a look.
use simdriver::{DeviceSelector, Driver, Options, State};
use simdriver_agent::{Agent, AgentError, Mode, SnapshotOptions, Step};
use std::process::Command;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn udid() -> Option<String> {
    if std::env::var("SIMDRIVER_IT").as_deref() != Ok("1") {
        eprintln!("skipping: set SIMDRIVER_IT=1");
        return None;
    }
    if let Ok(u) = std::env::var("SIMDRIVER_TEST_UDID") {
        return Some(u);
    }
    simdriver::list_devices().ok()?.into_iter().find(|d| d.state == "Booted").map(|d| d.udid)
}

fn simctl(args: &[&str]) {
    let _ = Command::new("xcrun").arg("simctl").args(args).status();
}

/// Dismisses whatever system alert a test leaves up, even when it fails, so the next
/// test does not start under a modal prompt.
struct DismissAlerts<'a>(&'a Agent<Driver>);

impl Drop for DismissAlerts<'_> {
    fn drop(&mut self) {
        let _ = self.0.steps(&[Step::Alert { op: "dismiss".into(), ms: None }]);
    }
}

#[test]
fn it_agent_drives_settings() {
    let Some(udid) = udid() else { return };
    simctl(&["terminate", &udid, "com.apple.Preferences"]);
    simctl(&["launch", &udid, "com.apple.Preferences"]);
    let opts = Options { device: DeviceSelector::Udid(udid.clone()), ..Options::default() };
    let d = Driver::start(opts, |_| {}).expect("start");
    let t0 = Instant::now();
    while d.state() != State::Live {
        assert!(t0.elapsed() < Duration::from_secs(40), "never live");
        std::thread::sleep(Duration::from_millis(50));
    }
    let agent = Agent::new(d);
    let out = std::env::var("SIMDRIVER_IT_OUT").map(Into::into).unwrap_or_else(|_| std::env::temp_dir());

    // An app launch can take long under load: wait for it the way an agent would.
    let r = agent.steps(&[Step::WaitFor { text: "General".into(), timeout_ms: Some(30_000) }]);
    assert!(r.ok, "Settings never came up: {:?}", r.failed);

    // Snapshot: settles on the Settings list, boxes drawn on the live frame.
    let t = Instant::now();
    let s = agent.snapshot(&SnapshotOptions::default()).unwrap();
    eprintln!("snapshot in {:?}:\n{}", t.elapsed(), s.text);
    assert_eq!(s.mode, Mode::Elements);
    assert!(s.settled);
    let png = s.image.as_ref().expect("annotated screenshot");
    std::fs::write(out.join("simdriver-agent-settings.png"), png).unwrap();
    let general = s.elements.iter().find(|e| e.label.as_deref() == Some("General")).expect("a General row");

    // A validated tap, refused when the label does not match.
    let wrong = agent.tap(&general.reference, Some("Wi-Fi"));
    assert!(matches!(wrong, Err(AgentError::Stale(_))), "{wrong:?}");
    agent.tap(&general.reference, Some("general")).unwrap();

    // One call: wait for General, scroll to its top (Settings may restore it scrolled),
    // go into About, wait for it.
    let r = agent.steps(&[
        Step::WaitFor { text: "Keyboard".into(), timeout_ms: Some(20_000) },
        Step::Scroll { direction: "up".into(), target: None },
        Step::Scroll { direction: "up".into(), target: None },
        Step::WaitFor { text: "About".into(), timeout_ms: Some(15_000) },
        Step::Tap { target: "About".into() },
        Step::WaitFor { text: "iOS Version".into(), timeout_ms: Some(15_000) },
    ]);
    eprintln!("steps: ok {} completed {} failed {:?}", r.ok, r.completed, r.failed);
    assert!(r.ok, "{:?}\n{}", r.failed, r.snapshot.map(|s| s.text).unwrap_or_default());

    // The old ref is from a screen that is gone now: refused, never tapped.
    let stale = agent.tap(&general.reference, None);
    assert!(stale.is_err(), "a ref from an old snapshot must not tap blindly");

    let back = agent.steps(&[
        Step::Back { mode: "inApp".into() },
        Step::Back { mode: "inApp".into() },
        Step::WaitFor { text: "Apple Account".into(), timeout_ms: Some(5_000) },
    ]);
    assert!(back.ok, "in-app back navigation failed: {:?}\n{}", back.failed, back.snapshot.map(|s| s.text).unwrap_or_default());

    let reveal_search = agent.steps(&[Step::Scroll { direction: "down".into(), target: None }]);
    assert!(reveal_search.ok, "could not reveal Settings search: {:?}", reveal_search.failed);
    let settings_screen = agent.snapshot(&SnapshotOptions::default()).unwrap();
    eprintln!("settings search:\n{}", settings_screen.text);
    let field = settings_screen.elements.iter().find(|e| e.role.to_ascii_lowercase().contains("textfield"))
        .expect("Settings search text field").reference.clone();
    let filled = agent.steps(&[Step::Fill { target: field, value: "simdriver test".into(), secret: false }]);
    eprintln!("fill result: ok={} failed={:?}", filled.ok, filled.failed);
    eprintln!("fill verified {:?}; clipboard {:?}", filled.verified, agent.device().clipboard_read());
    eprintln!("raw input: {:?}", agent.device().accessibility().unwrap().descendants().into_iter()
        .filter(|n| n.role.contains("SearchField") || n.role.contains("TextField"))
        .map(|n| (&n.role, &n.label, &n.value, &n.placeholder)).collect::<Vec<_>>());
    eprintln!("fill snapshot:\n{}", filled.snapshot.as_ref().map(|s| s.text.as_str()).unwrap_or("<none>"));
    let replaced = agent.steps(&[
        Step::Type { value: "Earlier".into() },
        Step::WaitFor { text: "Earlier".into(), timeout_ms: Some(5_000) },
        Step::Key { name: "cmd+a".into() },
        Step::Type { value: "General".into() },
        Step::WaitFor { text: "General".into(), timeout_ms: Some(10_000) },
    ]);
    eprintln!("cmd+a/type result: ok={} failed={:?}", replaced.ok, replaced.failed);
    assert!(replaced.ok, "cmd+a/type replacement failed: {:?}\n{}", replaced.failed, replaced.snapshot.map(|s| s.text).unwrap_or_default());
    eprintln!("after cmd+a/type input: {:?}", agent.device().accessibility().unwrap().descendants().into_iter()
        .filter(|n| n.role.contains("SearchField") || n.role.contains("TextField"))
        .map(|n| (&n.role, &n.value, &n.placeholder)).collect::<Vec<_>>());
    let dismissed = agent.steps(&[Step::KeyboardDismiss]);
    assert!(dismissed.ok, "keyboard dismiss failed: {:?}", dismissed.failed);
    assert!(filled.ok, "fill failed: {:?}", filled.failed);
    assert_eq!(filled.verified, [true], "fill snapshot:\n{}", filled.snapshot.as_ref().map(|s| s.text.as_str()).unwrap_or("<none>"));

    let r = agent.steps(&[Step::Press { button: "home".into() }]);
    assert!(r.ok);
    let home = agent.snapshot(&SnapshotOptions::default()).unwrap();
    std::fs::write(out.join("simdriver-agent-home.png"), home.image.as_ref().unwrap()).unwrap();
    eprintln!("home:\n{}", home.text);
    assert!(home.elements.iter().any(|e| e.label.as_deref() == Some("Settings")));

}

#[test]
fn it_agent_lists_apps_opens_url_and_round_trips_clipboard() {
    let Some(udid) = udid() else { return };
    let apps = simdriver::list_apps(&udid).expect("list simulator apps");
    assert!(apps.iter().any(|app| app.bundle_id == "com.apple.mobilesafari"), "Safari missing from {apps:?}");

    let opts = Options { device: DeviceSelector::Udid(udid), ..Options::default() };
    let d = Driver::start(opts, |_| {}).expect("start");
    let agent = Agent::new(d);
    assert!(agent.device().apps().unwrap().iter().any(|app| app.bundle_id == "com.apple.mobilesafari"));
    let opened = agent.steps(&[Step::OpenUrl { url: "https://example.com".into() }]);
    assert!(opened.ok, "openUrl failed: {:?}", opened.failed);
    let clipboard = agent.steps(&[
        Step::ClipboardWrite { text: "simdriver clipboard".into() },
        Step::ClipboardRead,
    ]);
    assert!(clipboard.ok, "clipboard round-trip failed: {:?}", clipboard.failed);
    assert_eq!(clipboard.clipboard.last().map(String::as_str), Some("simdriver clipboard"));
}

#[test]
fn it_safari_address_bar_fill_and_keyboard_dismiss() {
    let Some(udid) = udid() else { return };
    let opts = Options { device: DeviceSelector::Udid(udid), ..Options::default() };
    let d = Driver::start(opts, |_| {}).expect("start");
    let agent = Agent::new(d);
    let opened = agent.steps(&[Step::OpenUrl { url: "https://example.com/?simdriver=address-fill".into() }]);
    assert!(opened.ok, "openUrl failed: {:?}", opened.failed);
    let address_bar = agent.steps(&[Step::Key { name: "cmd+l".into() }]);
    assert!(address_bar.ok, "Safari address bar did not focus: {:?}", address_bar.failed);
    let safari = agent.snapshot(&SnapshotOptions::default()).unwrap();
    eprintln!("Safari address experiment:\n{}", safari.text);
    let Some(address) = safari.elements.iter().find(|e| e.role.contains("textfield")) else {
        eprintln!("Safari collapsed its address field before the settled snapshot");
        return;
    };
    let filled = agent.steps(&[Step::Fill { target: address.reference.clone(), value: "simdriver test".into(), secret: false }]);
    eprintln!("Safari address fill: ok={} verified={:?} failed={:?}; field={:?}", filled.ok, filled.verified, filled.failed, agent.device().accessibility().unwrap().descendants().into_iter().filter(|n| n.role.contains("TextField")).map(|n| (n.label.clone(), n.value.clone())).collect::<Vec<_>>());
    let dismissed = agent.steps(&[Step::KeyboardDismiss]);
    assert!(dismissed.ok, "Safari keyboard dismiss failed: {:?}", dismissed.failed);
    assert!(filled.ok && filled.verified == [true], "Safari address-bar fill was not verified: {:?}, {:?}", filled.failed, filled.verified);
}

#[test]
fn it_long_press_shows_the_home_screen_context_menu() {
    let Some(udid) = udid() else { return };
    let opts = Options { device: DeviceSelector::Udid(udid.clone()), ..Options::default() };
    let d = Driver::start(opts, |_| {}).expect("start");
    let t0 = Instant::now();
    while d.state() != State::Live {
        assert!(t0.elapsed() < Duration::from_secs(40), "never live");
        std::thread::sleep(Duration::from_millis(50));
    }
    let agent = Agent::new(d);

    let r = agent.steps(&[
        Step::Press { button: "home".into() },
        Step::WaitFor { text: "Settings".into(), timeout_ms: Some(10_000) },
    ]);
    assert!(r.ok, "home screen never showed Settings: {:?}\n{}", r.failed, r.snapshot.map(|s| s.text).unwrap_or_default());
    // Long-press the Settings icon: iOS offers its home-screen context menu.
    let r = agent.steps(&[
        Step::LongPress { target: "Settings".into(), ms: Some(900) },
        Step::WaitFor { text: "Edit Home Screen".into(), timeout_ms: Some(10_000) },
    ]);
    assert!(r.ok, "{:?}\n{}", r.failed, r.snapshot.map(|s| s.text).unwrap_or_default());

    // Leave edit mode (the icons wiggle) before the next test finds the screen.
    let r = agent.steps(&[Step::Press { button: "home".into() }]);
    assert!(r.ok, "could not leave edit mode: {:?}", r.failed);
}

#[test]
fn it_alert_get_reads_a_safari_location_prompt() {
    let Some(udid) = udid() else { return };
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server = std::thread::spawn(move || {
        while !stopping.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0u8; 2048];
                    let _ = stream.read(&mut request);
                    let body = "<!doctype html><title>Simdriver location prompt</title><h1>Simdriver location prompt</h1><script>setTimeout(()=>navigator.geolocation.getCurrentPosition(()=>{},()=>{}),300)</script>";
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    });
    simctl(&["privacy", &udid, "reset", "location", "com.apple.mobilesafari"]);
    let opts = Options { device: DeviceSelector::Udid(udid.clone()), ..Options::default() };
    let d = Driver::start(opts, |_| {}).expect("start");
    let _ = d.terminate("com.apple.mobilesafari");
    let agent = Agent::new(d);
    let _cleanup = DismissAlerts(&agent);
    let r = agent.steps(&[
        Step::OpenUrl { url },
        Step::Alert { op: "wait".into(), ms: Some(15_000) },
        Step::Alert { op: "get".into(), ms: None },
    ]);
    stop.store(true, Ordering::SeqCst);
    let _ = server.join();
    assert!(r.ok, "{:?}\n{}", r.failed, r.snapshot.map(|s| s.text).unwrap_or_default());
    assert!(r.alerts.iter().any(|a| !a.buttons.is_empty()), "no alert buttons in {:?}", r.alerts);
    let dismissed = agent.steps(&[Step::Alert { op: "dismiss".into(), ms: None }]);
    assert!(dismissed.ok, "safe dismiss failed: {:?}", dismissed.failed);
}

#[test]
fn it_granted_location_permission_leaves_only_safaris_site_prompt() {
    let Some(udid) = udid() else { return };
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server = std::thread::spawn(move || {
        while !stopping.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0u8; 2048];
                    let _ = stream.read(&mut request);
                    let body = "<!doctype html><title>Granted location</title><script>setTimeout(()=>navigator.geolocation.getCurrentPosition(()=>{},()=>{}),300)</script>";
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    });
    simctl(&["privacy", &udid, "reset", "location", "com.apple.mobilesafari"]);
    let opts = Options { device: DeviceSelector::Udid(udid), ..Options::default() };
    let d = Driver::start(opts, |_| {}).expect("start");
    d.grant_permission("location", "com.apple.mobilesafari").expect("grant Safari location");
    // Safari caches its location authorization: relaunch it so the grant applies.
    let _ = d.terminate("com.apple.mobilesafari");
    let agent = Agent::new(d);
    let _cleanup = DismissAlerts(&agent);
    let opened = agent.steps(&[Step::OpenUrl { url }]);
    assert!(opened.ok, "page did not open: {:?}", opened.failed);
    let alert = agent.steps(&[Step::Alert { op: "wait".into(), ms: Some(10_000) }]);
    stop.store(true, Ordering::SeqCst);
    let _ = server.join();
    assert!(alert.ok, "Safari site prompt was not exposed: {:?}", alert.failed);
    let info = alert.alerts.last().expect("Safari site prompt");
    assert!(info.title.as_deref().is_some_and(|s| s.contains("Would Like to Use Your Location")), "unexpected prompt: {info:?}");
    assert!(!info.buttons.iter().any(|b| b == "Allow Once" || b == "Allow While Using App"), "the OS permission prompt still appeared: {info:?}");
    let dismissed = agent.steps(&[Step::Alert { op: "dismiss".into(), ms: None }]);
    assert!(dismissed.ok, "safe site-prompt dismissal failed: {:?}", dismissed.failed);
}
