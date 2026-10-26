//! The agent against a real booted simulator, in Settings. Runs only with SIMDRIVER_IT=1;
//! drives SIMDRIVER_TEST_UDID (else the first booted device). Writes the annotated
//! screenshots to $SIMDRIVER_IT_OUT (default: the temp dir) for a look.
use simdriver::{DeviceSelector, Driver, Options, State};
use simdriver_agent::{Agent, AgentError, Mode, SnapshotOptions, Step};
use std::process::Command;
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
        Step::WaitFor { text: "Keyboard".into(), timeout_ms: Some(10_000) },
        Step::Scroll { direction: "up".into(), target: None },
        Step::Scroll { direction: "up".into(), target: None },
        Step::WaitFor { text: "About".into(), timeout_ms: Some(5000) },
        Step::Tap { target: "About".into() },
        Step::WaitFor { text: "iOS Version".into(), timeout_ms: Some(5000) },
    ]);
    eprintln!("steps: ok {} completed {} failed {:?}", r.ok, r.completed, r.failed);
    assert!(r.ok, "{:?}\n{}", r.failed, r.snapshot.map(|s| s.text).unwrap_or_default());

    // The old ref is from a screen that is gone now: refused, never tapped.
    let stale = agent.tap(&general.reference, None);
    assert!(stale.is_err(), "a ref from an old snapshot must not tap blindly");

    let r = agent.steps(&[Step::Press { button: "home".into() }]);
    assert!(r.ok);
    let home = agent.snapshot(&SnapshotOptions::default()).unwrap();
    std::fs::write(out.join("simdriver-agent-home.png"), home.image.as_ref().unwrap()).unwrap();
    eprintln!("home:\n{}", home.text);
    assert!(home.elements.iter().any(|e| e.label.as_deref() == Some("Settings")));
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

    agent.steps(&[Step::Press { button: "home".into() }]).ok.then_some(()).unwrap();
    agent.steps(&[Step::WaitFor { text: "Settings".into(), timeout_ms: Some(10_000) }]).ok.then_some(()).unwrap();
    // Long-press the Settings icon: iOS offers its home-screen context menu.
    let r = agent.steps(&[
        Step::LongPress { target: "Settings".into(), ms: Some(900) },
        Step::WaitFor { text: "Edit Home Screen".into(), timeout_ms: Some(10_000) },
    ]);
    assert!(r.ok, "{:?}\n{}", r.failed, r.snapshot.map(|s| s.text).unwrap_or_default());

    // Leave edit mode (the icons wiggle) before the next test finds the screen.
    agent.steps(&[Step::Press { button: "home".into() }]).ok.then_some(()).unwrap();
}
