//! Multi-step flows in one call: `[{ action: "fill", target: "Email", value: "…" },
//! { action: "tap", target: "Log in" }, { action: "waitFor", text: "Welcome" }]`.
//! Each step resolves its target on a fresh, settled screen; the run stops at the first
//! step that fails and reports it with the screen as it is then, so an agent never acts
//! on a screen it has not seen.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Step {
    /// `target`: an element ref from the last snapshot (`e4`), a label or identifier on
    /// the current screen (`"Log in"`), or a grid cell (`C4`).
    Tap { target: String },
    /// Press and hold `target` (a ref, label, or grid cell) — iOS's long press (context
    /// menus, rearrange mode). `ms` defaults to 800.
    #[serde(rename_all = "camelCase")]
    LongPress {
        target: String,
        #[serde(default)]
        ms: Option<u64>,
    },
    /// Tap `target` (a text field), then type `value`.
    Fill { target: String, value: String },
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

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepFailure {
    pub index: usize,
    pub step: Step,
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
        assert_eq!(steps[0], Step::Fill { target: "Email".into(), value: "a@b.c".into() });
        assert_eq!(steps[2], Step::WaitFor { text: "Welcome".into(), timeout_ms: Some(2000) });
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"action":"longPress","target":"e4"},
                {"action":"longPress","target":"General","ms":1500}]"#,
        )
        .unwrap();
        assert_eq!(steps[0], Step::LongPress { target: "e4".into(), ms: None });
        assert_eq!(steps[1], Step::LongPress { target: "General".into(), ms: Some(1500) });
        assert!(serde_json::from_str::<Step>(r#"{"action":"tap","targt":"x"}"#).is_err(), "typos are errors");
    }
}
