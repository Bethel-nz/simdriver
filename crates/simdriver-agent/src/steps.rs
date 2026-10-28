//! Multi-step flows in one call: `[{ action: "fill", target: "Email", value: "…" },
//! { action: "tap", target: "Log in" }, { action: "waitFor", text: "Welcome" }]`.
//! Each step resolves its target on a fresh, settled screen; the run stops at the first
//! step that fails and reports it with the screen as it is then, so an agent never acts
//! on a screen it has not seen.
use crate::RecordOptions;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SwipeTarget {
    Target(String),
    Point([f64; 2]),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Step {
    /// `target`: an element ref from the last snapshot (`e4`), a label or identifier on
    /// the current screen (`"Log in"`), or a grid cell (`C4`).
    Tap { target: String },
    /// Double tap a validated element ref, label, identifier, or grid cell.
    DoubleTap { target: String },
    /// Drag from one target to another (reorder handles, sliders): `from` and `to` are
    /// refs, labels, or grid cells, resolved like `tap` resolves. The touch holds ~300 ms
    /// before moving (so iOS sees a drag, not a scroll), then moves over `ms` (default 600).
    Drag {
        from: String,
        to: String,
        #[serde(default, rename = "sourceHoldMs")]
        source_hold_ms: Option<u64>,
        #[serde(default, rename = "moveMs", alias = "ms")]
        move_ms: Option<u64>,
        #[serde(default, rename = "destinationHoldMs")]
        destination_hold_ms: Option<u64>,
    },
    /// Pinch around a target's centre (screen centre by default). A scale above 1 zooms in.
    Pinch {
        #[serde(default)]
        target: Option<String>,
        scale: f64,
    },
    /// Rotate around a target's centre (screen centre by default), in degrees.
    Rotate {
        #[serde(default)]
        target: Option<String>,
        degrees: f64,
    },
    /// Open a URL; this can switch the foreground app.
    OpenUrl { url: String },
    /// Launch an installed app by bundle identifier (`bundleId`).
    #[serde(rename_all = "camelCase")]
    Launch { bundle_id: String },
    /// Terminate an app by bundle identifier (`bundleId`).
    #[serde(rename_all = "camelCase")]
    Terminate { bundle_id: String },
    /// Set the simulator's location.
    SetLocation { lat: f64, lon: f64 },
    /// Set the simulator appearance to `light` or `dark`.
    Appearance { appearance: String },
    /// Read the simulator pasteboard; the value is included in the step report.
    ClipboardRead,
    /// Write text to the simulator pasteboard. An empty value clears it.
    ClipboardWrite { text: String },
    /// Dismiss the on-screen keyboard and confirm it is gone.
    KeyboardDismiss,
    /// Return using the system edge gesture or an in-app navigation button.
    Back { mode: String },
    /// Open the iOS app switcher with a double home-button press.
    AppSwitcher,
    /// Capture the target's frame as a cropped PNG attached to the step report.
    ScreenshotElement { target: String },
    /// Read, wait for, accept, or dismiss a system alert. Dismiss only uses explicit safe labels.
    Alert {
        op: String,
        #[serde(default)]
        ms: Option<u64>,
    },
    /// Move two fingers together around a point, with momentum disabled.
    Pan {
        #[serde(default)]
        target: Option<String>,
        dx: f64,
        dy: f64,
        #[serde(default)]
        ms: Option<u64>,
    },
    /// One continuous two-finger pan, scale, and rotation stroke.
    Transform {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
        scale: f64,
        #[serde(default)]
        degrees: f64,
    },
    /// Record the screen over the steps that follow (valid only as the first step): a
    /// few timed frames and a contact sheet, in the report's `record`. Unset fields take
    /// the recorder's defaults and limits.
    #[serde(rename_all = "camelCase")]
    Record {
        #[serde(default)]
        time_ms: Option<u64>,
        #[serde(default)]
        idle_ms: Option<u64>,
        #[serde(default)]
        full: bool,
        #[serde(default)]
        fps: Option<u32>,
        #[serde(default)]
        max_frames: Option<usize>,
    },
    /// Fast directional swipe with momentum.
    Fling { direction: String },
    /// Swipe between refs, labels, grid cells, or normalized `[x, y]` points.
    Swipe {
        from: SwipeTarget,
        to: SwipeTarget,
        #[serde(default)]
        count: Option<u32>,
        #[serde(default)]
        #[serde(rename = "pauseMs")]
        pause_ms: Option<u64>,
    },
    /// Press and hold `target` (a ref, label, or grid cell) — iOS's long press (context
    /// menus, rearrange mode). `ms` defaults to 800.
    #[serde(rename_all = "camelCase")]
    LongPress {
        target: String,
        #[serde(default)]
        ms: Option<u64>,
    },
    /// Tap `target` (a text field), then type `value`.
    Fill {
        target: String,
        value: String,
        #[serde(default)]
        secret: bool,
    },
    /// Scroll to reveal what lies `direction` (`up`, `down`, `left`, `right`): `down`
    /// drags the content up to show what is below. Inside `target` (a ref or label) when
    /// given, else the middle of the screen.
    Scroll {
        direction: String,
        #[serde(default)]
        target: Option<String>,
    },
    /// Type into whatever has focus.
    Type { value: String },
    /// A named key: `enter`, `backspace`, `tab`, `escape`, arrows…
    Key { name: String },
    /// A hardware button: `home`, `lock`, `side`, `siri`, `volume-up`, `volume-down`.
    Press { button: String },
    /// Wait until some label or value on screen contains `text` (case-insensitive).
    #[serde(rename_all = "camelCase")]
    WaitFor {
        text: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

impl Step {
    /// For `Record`: its options over [`RecordOptions::default`] (the recorder clamps).
    pub(crate) fn record_options(&self) -> Option<RecordOptions> {
        let Step::Record {
            time_ms,
            idle_ms,
            full,
            fps,
            max_frames,
        } = self
        else {
            return None;
        };
        let d = RecordOptions::default();
        Some(RecordOptions {
            time: time_ms.map_or(d.time, Duration::from_millis),
            idle: idle_ms.map_or(d.idle, Duration::from_millis),
            full: *full,
            fps: fps.unwrap_or(d.fps),
            max_frames: max_frames.unwrap_or(d.max_frames),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepFailure {
    pub index: usize,
    pub step: Step,
    pub code: String,
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_from_the_json_agents_write() {
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"action":"fill","target":"Email","value":"a@b.c"},
                {"action":"tap","target":"e4"},
                {"action":"waitFor","text":"Welcome","timeoutMs":2000},
                {"action":"press","button":"home"}]"#,
        )
        .unwrap();
        assert_eq!(
            steps[0],
            Step::Fill {
                target: "Email".into(),
                value: "a@b.c".into(),
                secret: false
            }
        );
        assert_eq!(
            steps[2],
            Step::WaitFor {
                text: "Welcome".into(),
                timeout_ms: Some(2000)
            }
        );
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"action":"longPress","target":"e4"},
                {"action":"longPress","target":"General","ms":1500},
                {"action":"drag","from":"e1","to":"C4"},
                {"action":"drag","from":"Email","to":"Log in","ms":1200},
                {"action":"pinch","scale":2.0},
                {"action":"rotate","target":"Map","degrees":90},
                {"action":"key","name":"cmd+a"},
                {"action":"openUrl","url":"https://example.com"},
                {"action":"setLocation","lat":6.5,"lon":3.3},
                {"action":"appearance","appearance":"dark"},
                {"action":"keyboardDismiss"},
                {"action":"back","mode":"inApp"},
                {"action":"screenshotElement","target":"e1"},
                {"action":"launch","bundleId":"com.apple.Preferences"},
                {"action":"swipe","from":[0.5,0.8],"to":"e2","count":2,"pauseMs":100},
                {"action":"drag","from":"e1","to":"e2","sourceHoldMs":300,"moveMs":400,"destinationHoldMs":50}]"#,
        )
        .unwrap();
        assert_eq!(
            steps[0],
            Step::LongPress {
                target: "e4".into(),
                ms: None
            }
        );
        assert_eq!(
            steps[1],
            Step::LongPress {
                target: "General".into(),
                ms: Some(1500)
            }
        );
        assert_eq!(
            steps[2],
            Step::Drag {
                from: "e1".into(),
                to: "C4".into(),
                source_hold_ms: None,
                move_ms: None,
                destination_hold_ms: None
            }
        );
        assert_eq!(
            steps[3],
            Step::Drag {
                from: "Email".into(),
                to: "Log in".into(),
                source_hold_ms: None,
                move_ms: Some(1200),
                destination_hold_ms: None
            }
        );
        assert_eq!(
            steps[4],
            Step::Pinch {
                target: None,
                scale: 2.0
            }
        );
        assert_eq!(
            steps[5],
            Step::Rotate {
                target: Some("Map".into()),
                degrees: 90.0
            }
        );
        assert_eq!(
            steps[6],
            Step::Key {
                name: "cmd+a".into()
            }
        );
        assert_eq!(
            steps[7],
            Step::OpenUrl {
                url: "https://example.com".into()
            }
        );
        assert_eq!(steps[8], Step::SetLocation { lat: 6.5, lon: 3.3 });
        assert_eq!(
            steps[9],
            Step::Appearance {
                appearance: "dark".into()
            }
        );
        assert_eq!(steps[10], Step::KeyboardDismiss);
        assert_eq!(
            steps[11],
            Step::Back {
                mode: "inApp".into()
            }
        );
        assert_eq!(
            steps[12],
            Step::ScreenshotElement {
                target: "e1".into()
            }
        );
        assert_eq!(
            steps[13],
            Step::Launch {
                bundle_id: "com.apple.Preferences".into()
            }
        );
        assert_eq!(
            steps[14],
            Step::Swipe {
                from: SwipeTarget::Point([0.5, 0.8]),
                to: SwipeTarget::Target("e2".into()),
                count: Some(2),
                pause_ms: Some(100)
            }
        );
        assert_eq!(
            steps[15],
            Step::Drag {
                from: "e1".into(),
                to: "e2".into(),
                source_hold_ms: Some(300),
                move_ms: Some(400),
                destination_hold_ms: Some(50)
            }
        );
        assert!(
            serde_json::from_str::<Step>(r#"{"action":"launch","bundle_id":"x"}"#).is_err(),
            "camelCase only"
        );
        assert!(
            serde_json::from_str::<Step>(r#"{"action":"tap","targt":"x"}"#).is_err(),
            "typos are errors"
        );
    }
}
