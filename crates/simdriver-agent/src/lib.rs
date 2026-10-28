//! What an AI agent needs on top of simdriver, so it can drive a simulator by meaning
//! rather than by pixels:
//!
//! - [`Agent::snapshot`]: waits for the screen to settle, then lists the interactive
//!   elements as `e1…eN` (text for the model) with an annotated screenshot (boxes and
//!   refs drawn on the live frame). Refs are stable: an element keeps its ref across
//!   snapshots while it stays on screen, and a ref is never reused for another element,
//!   so an old ref either still means the same thing or is refused as stale. A screen with no accessible elements (a map, a
//!   canvas, a game) comes back as a lettered grid instead (`A1…M6`).
//! - [`Agent::tap`]: `tap("e4")` re-reads that element right before the touch and
//!   refuses if it moved, vanished, is covered, or (with `expect`) no longer reads as
//!   expected; `tap("C4")` taps a grid cell.
//! - [`Agent::steps`]: several actions in one call, each on a fresh settled screen,
//!   stopping at the first failure with the screen as it is then.
//!
//! The agent drives any [`Device`]; [`simdriver::Driver`] is one.
pub mod draw;
mod marks;
mod record;
mod steps;

pub use marks::{Element, Visibility};
pub use record::*;
pub use steps::{Step, StepFailure, SwipeTarget};

use serde::{Deserialize, Serialize};
use simdriver::{AxNode, Button, Driver, Frame, Grid};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How far (points, any edge) an element may move before a tap on its ref is refused.
const MOVE_TOLERANCE: f64 = 8.0;
/// Between reads while waiting for the screen to settle.
const POLL: Duration = Duration::from_millis(80);
/// An empty screen must stay empty this long before it counts as settled (a loading
/// screen is empty for a moment too).
const EMPTY_SETTLE: Duration = Duration::from_secs(1);
/// Default longest settle. Only a changing screen waits this long: an app just launched
/// reads as SpringBoard's status bar for ~2 s, and its first full read takes ~2 s more
/// (seen at 4–6 s under load); a still screen settles in ~100 ms.
const SETTLE_DEFAULT: Duration = Duration::from_secs(10);
/// After tapping a text field, before typing (focus and keyboard).
const FOCUS_WAIT: Duration = Duration::from_millis(300);
const WAIT_FOR_DEFAULT: Duration = Duration::from_secs(5);
const CAPTURE_WAIT: Duration = Duration::from_secs(1);
/// How long a pinch or rotation stroke takes.
const GESTURE_STROKE: Duration = Duration::from_millis(300);
/// A scroll drags this share of the region, over this long (slow enough to not fling far).
const SCROLL_SHARE: f64 = 0.5;
const SCROLL_TIME: Duration = Duration::from_millis(350);
/// After an action, a screen this much changed (share of coarse cells) with an unchanged
/// accessibility tree means the tree lags (it trails an app switch by ~3 s).
const LAG_CHANGE: f64 = 0.25;
/// A mark older than this no longer holds settling up (the tree lag seen is ≈3.5 s).
const LAG_MAX: Duration = Duration::from_secs(8);
/// Coarse screen fingerprint: columns × rows of mean luminance.
const PRINT: (usize, usize) = (16, 32);

/// What the agent drives. Implemented for [`simdriver::Driver`] (and `Arc`s of devices).
pub trait Device: Send + Sync {
    fn accessibility(&self) -> Result<AxNode, simdriver::Error>;
    fn screen_points(&self) -> Option<(f64, f64)>;
    fn capture(&self, wait: Duration) -> Option<Frame>;
    /// A sampled stream of the frames that arrive from now on, at most `fps` a second,
    /// for [`Recorder`]. `None` when the device cannot stream (the default).
    fn frame_tap(&self, fps: u32) -> Option<simdriver::FrameTap> {
        let _ = fps;
        None
    }
    /// A tap at (x, y) in 0..1 of the screen.
    fn tap(&self, x: f64, y: f64);
    /// Two taps at (x, y) in 0..1 of the screen.
    fn double_tap(&self, x: f64, y: f64) {
        self.tap(x, y);
        std::thread::sleep(Duration::from_millis(100));
        self.tap(x, y);
    }
    /// Press at (x, y) in 0..1 of the screen, hold for `hold`, then lift.
    fn long_press(&self, x: f64, y: f64, hold: Duration);
    /// One finger from `from` to `to` (0..1 of the screen) over `duration`.
    fn swipe(&self, from: (f64, f64), to: (f64, f64), duration: Duration);
    /// A drag: down at `from`, hold `hold`, move to `to` over `duration`, lift.
    fn drag(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        source_hold: Duration,
        duration: Duration,
        destination_hold: Duration,
    );
    /// Place two fingers at exact box points for pinch and rotation gestures.
    fn touch2(&self, phase: simdriver::Phase, a: (f64, f64), b: (f64, f64));
    /// Pinch around a point in 0..1 screen coordinates. Keeps both fingers on-screen.
    /// A smooth stroke, not one jump: gesture recognizers need intermediate moves.
    fn pinch(&self, center: (f64, f64), scale: f64) -> Result<(), AgentError> {
        self.transform(center, 0.0, 0.0, scale, 0.0, GESTURE_STROKE)
    }
    /// Combined two-finger transform. Translation uses screen fractions; scale is a ratio.
    fn transform(
        &self,
        center: (f64, f64),
        dx: f64,
        dy: f64,
        scale: f64,
        degrees: f64,
        duration: Duration,
    ) -> Result<(), AgentError> {
        let (w, h) = self
            .screen_points()
            .ok_or_else(|| AgentError::NotReady("screen size is not ready".into()))?;
        if ![dx, dy, scale, degrees].iter().all(|v| v.is_finite()) || scale <= 0.0 {
            return Err(AgentError::Invalid(
                "transform values must be finite and scale positive".into(),
            ));
        }
        let scale = scale.clamp(0.25, 4.0);
        let (cx, cy) = (center.0 * w, center.1 * h);
        let (tx, ty) = (cx + dx * w, cy + dy * h);
        let room = cx
            .min(w - cx)
            .min(cy)
            .min(h - cy)
            .min(tx)
            .min(w - tx)
            .min(ty)
            .min(h - ty)
            / scale.max(1.0);
        let radius = 50.0_f64.min(room);
        if radius < 1.0 {
            return Err(AgentError::Invalid(
                "transform target is too close to a screen edge".into(),
            ));
        }
        let start = ((cx - radius) / w, cy / h, (cx + radius) / w, cy / h);
        self.touch2(
            simdriver::Phase::Down,
            (start.0, start.1),
            (start.2, start.3),
        );
        let steps = (duration.as_millis() / 16).max(1) as u32;
        let mut end = start;
        for i in 1..=steps {
            thread::sleep(duration / steps);
            let t = f64::from(i) / f64::from(steps);
            let (px, py, r, a) = (
                cx + dx * w * t,
                cy + dy * h * t,
                radius * (1.0 + (scale - 1.0) * t),
                degrees.to_radians() * t,
            );
            end = (
                (px - r * a.cos()) / w,
                (py - r * a.sin()) / h,
                (px + r * a.cos()) / w,
                (py + r * a.sin()) / h,
            );
            self.touch2(simdriver::Phase::Move, (end.0, end.1), (end.2, end.3));
        }
        self.touch2(simdriver::Phase::Up, (end.0, end.1), (end.2, end.3));
        Ok(())
    }
    /// Rotate two fingers around a point in 0..1 screen coordinates, in one smooth stroke.
    fn rotate(&self, center: (f64, f64), degrees: f64) -> Result<(), AgentError> {
        self.transform(center, 0.0, 0.0, 1.0, degrees, GESTURE_STROKE)
    }
    fn text(&self, s: &str);
    fn key(&self, name: &str) -> bool;
    fn button(&self, b: Button);
    fn open_url(&self, url: &str) -> Result<(), simdriver::Error>;
    fn launch(&self, bundle_id: &str) -> Result<(), simdriver::Error>;
    fn terminate(&self, bundle_id: &str) -> Result<(), simdriver::Error>;
    fn set_location(&self, lat: f64, lon: f64) -> Result<(), simdriver::Error>;
    fn set_appearance(&self, appearance: simdriver::Appearance) -> Result<(), simdriver::Error>;
    fn clipboard_read(&self) -> Result<String, simdriver::Error>;
    fn clipboard_write(&self, text: &str) -> Result<(), simdriver::Error>;
}

impl Device for Driver {
    fn accessibility(&self) -> Result<AxNode, simdriver::Error> {
        Driver::accessibility(self)
    }
    fn screen_points(&self) -> Option<(f64, f64)> {
        Driver::screen_points(self)
    }
    fn capture(&self, wait: Duration) -> Option<Frame> {
        Driver::capture(self, wait)
    }
    fn frame_tap(&self, fps: u32) -> Option<simdriver::FrameTap> {
        Driver::frame_tap(self, fps)
    }
    fn tap(&self, x: f64, y: f64) {
        Driver::tap(self, x, y)
    }
    fn double_tap(&self, x: f64, y: f64) {
        Driver::double_tap(self, x, y)
    }
    fn long_press(&self, x: f64, y: f64, hold: Duration) {
        Driver::long_press(self, x, y, hold)
    }
    fn drag(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        source_hold: Duration,
        duration: Duration,
        destination_hold: Duration,
    ) {
        Driver::drag_with_holds(
            self,
            from,
            to,
            simdriver::DragTiming {
                source_hold,
                movement: duration,
                destination_hold,
            },
        )
    }
    fn touch2(&self, phase: simdriver::Phase, a: (f64, f64), b: (f64, f64)) {
        Driver::touch2(self, phase, a.0, a.1, b.0, b.1)
    }
    fn swipe(&self, from: (f64, f64), to: (f64, f64), duration: Duration) {
        let none = simdriver::Modifiers::default();
        let steps = (duration.as_millis() / 16).max(2) as u32;
        Driver::pointer(self, simdriver::Phase::Down, from.0, from.1, none);
        for i in 1..=steps {
            let t = f64::from(i) / f64::from(steps);
            thread::sleep(duration / steps);
            let (x, y) = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            Driver::pointer(self, simdriver::Phase::Move, x, y, none);
        }
        Driver::pointer(self, simdriver::Phase::Up, to.0, to.1, none);
    }
    fn text(&self, s: &str) {
        Driver::text(self, s)
    }
    fn key(&self, name: &str) -> bool {
        Driver::key(self, name)
    }
    fn button(&self, b: Button) {
        Driver::button(self, b, 1, 0.0)
    }
    fn open_url(&self, url: &str) -> Result<(), simdriver::Error> {
        Driver::open_url(self, url)
    }
    fn launch(&self, bundle_id: &str) -> Result<(), simdriver::Error> {
        Driver::launch(self, bundle_id)
    }
    fn terminate(&self, bundle_id: &str) -> Result<(), simdriver::Error> {
        Driver::terminate(self, bundle_id)
    }
    fn set_location(&self, lat: f64, lon: f64) -> Result<(), simdriver::Error> {
        Driver::set_location(self, lat, lon)
    }
    fn set_appearance(&self, appearance: simdriver::Appearance) -> Result<(), simdriver::Error> {
        Driver::set_appearance(self, appearance)
    }
    fn clipboard_read(&self) -> Result<String, simdriver::Error> {
        Driver::clipboard_read(self)
    }
    fn clipboard_write(&self, text: &str) -> Result<(), simdriver::Error> {
        Driver::clipboard_write(self, text)
    }
}

impl<D: Device + ?Sized> Device for Arc<D> {
    fn accessibility(&self) -> Result<AxNode, simdriver::Error> {
        (**self).accessibility()
    }
    fn screen_points(&self) -> Option<(f64, f64)> {
        (**self).screen_points()
    }
    fn capture(&self, wait: Duration) -> Option<Frame> {
        (**self).capture(wait)
    }
    fn frame_tap(&self, fps: u32) -> Option<simdriver::FrameTap> {
        (**self).frame_tap(fps)
    }
    fn tap(&self, x: f64, y: f64) {
        (**self).tap(x, y)
    }
    fn double_tap(&self, x: f64, y: f64) {
        (**self).double_tap(x, y)
    }
    fn long_press(&self, x: f64, y: f64, hold: Duration) {
        (**self).long_press(x, y, hold)
    }
    fn swipe(&self, from: (f64, f64), to: (f64, f64), duration: Duration) {
        (**self).swipe(from, to, duration)
    }
    fn drag(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        source_hold: Duration,
        duration: Duration,
        destination_hold: Duration,
    ) {
        (**self).drag(from, to, source_hold, duration, destination_hold)
    }
    fn touch2(&self, phase: simdriver::Phase, a: (f64, f64), b: (f64, f64)) {
        (**self).touch2(phase, a, b)
    }
    fn text(&self, s: &str) {
        (**self).text(s)
    }
    fn key(&self, name: &str) -> bool {
        (**self).key(name)
    }
    fn button(&self, b: Button) {
        (**self).button(b)
    }
    fn open_url(&self, url: &str) -> Result<(), simdriver::Error> {
        (**self).open_url(url)
    }
    fn launch(&self, bundle_id: &str) -> Result<(), simdriver::Error> {
        (**self).launch(bundle_id)
    }
    fn terminate(&self, bundle_id: &str) -> Result<(), simdriver::Error> {
        (**self).terminate(bundle_id)
    }
    fn set_location(&self, lat: f64, lon: f64) -> Result<(), simdriver::Error> {
        (**self).set_location(lat, lon)
    }
    fn set_appearance(&self, appearance: simdriver::Appearance) -> Result<(), simdriver::Error> {
        (**self).set_appearance(appearance)
    }
    fn clipboard_read(&self) -> Result<String, simdriver::Error> {
        (**self).clipboard_read()
    }
    fn clipboard_write(&self, text: &str) -> Result<(), simdriver::Error> {
        (**self).clipboard_write(text)
    }
}

/// Why an action was refused. The text tells the model what to do next.
#[derive(Debug)]
pub enum AgentError {
    /// The target changed since the snapshot (moved, gone, reads differently).
    Stale(String),
    /// The target is under another element.
    Covered(String),
    /// No such ref, label, cell, or awaited text.
    NotFound(String),
    /// A label matches more than one element equally well: use a ref instead.
    Ambiguous(String),
    /// No snapshot yet, or input not connected.
    NotReady(String),
    /// A malformed step (unknown key or button).
    Invalid(String),
    NotTextInput(String),
    CommitNotObserved,
    ClearNotObserved,
    KeyboardOccludes,
    Device(simdriver::Error),
}

impl AgentError {
    /// A stable short code: `stale`, `covered`, `not-found`, `ambiguous`, `not-ready`, `invalid`, `device`.
    pub fn code(&self) -> &'static str {
        match self {
            AgentError::Stale(_) => "stale",
            AgentError::Covered(_) => "covered",
            AgentError::NotFound(_) => "not-found",
            AgentError::Ambiguous(_) => "ambiguous",
            AgentError::NotReady(_) => "not-ready",
            AgentError::Invalid(_) => "invalid",
            AgentError::NotTextInput(_) => "not-text-input",
            AgentError::CommitNotObserved => "commit-not-observed",
            AgentError::ClearNotObserved => "clear-not-observed",
            AgentError::KeyboardOccludes => "keyboard-occludes",
            AgentError::Device(_) => "device",
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Stale(m)
            | AgentError::Covered(m)
            | AgentError::NotFound(m)
            | AgentError::Ambiguous(m)
            | AgentError::NotReady(m)
            | AgentError::Invalid(m)
            | AgentError::NotTextInput(m) => write!(f, "{}: {m}", self.code()),
            AgentError::CommitNotObserved
            | AgentError::ClearNotObserved
            | AgentError::KeyboardOccludes => {
                write!(f, "{}", self.code())
            }
            AgentError::Device(e) => write!(f, "device: {e}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<simdriver::Error> for AgentError {
    fn from(e: simdriver::Error) -> Self {
        AgentError::Device(e)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Longest wait for the screen to stop changing; past it the snapshot is taken as
    /// is and marked unsettled.
    pub settle: Duration,
    /// Draw the annotated screenshot (PNG).
    pub image: bool,
    /// Grid mode even when there are elements.
    pub grid: bool,
    /// Grid columns.
    pub cols: u32,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions {
            settle: SETTLE_DEFAULT,
            image: true,
            grid: false,
            cols: Grid::DEFAULT_COLS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Elements e1…eN are the targets (grid cells work too).
    Elements,
    /// No accessible elements: grid cells are the targets.
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSize {
    pub rows: u32,
    pub cols: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Increases with every snapshot; refs belong to the snapshot that made them.
    pub id: u64,
    pub mode: Mode,
    /// Screen size in points.
    pub screen: [f64; 2],
    pub elements: Vec<Element>,
    /// Interactive elements wholly off screen (scroll to reach them).
    pub offscreen: usize,
    pub grid: GridSize,
    /// False if the screen was still changing when the settle time ran out.
    pub settled: bool,
    /// The snapshot as text for the model: one line per element.
    pub text: String,
    /// The annotated screenshot (PNG), when asked for and a frame was available.
    #[serde(skip)]
    pub image: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tapped {
    /// The ref, label or cell that was tapped.
    pub target: String,
    /// What it is called on screen (empty for a grid cell).
    pub name: String,
    /// Where the touch went, in screen points.
    pub at: [f64; 2],
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepsReport {
    pub ok: bool,
    /// Steps that ran to completion.
    pub completed: usize,
    pub failed: Option<StepFailure>,
    /// Verification result for each fill step that completed, in order.
    pub verified: Vec<bool>,
    /// Values returned by clipboardRead steps, in order.
    pub clipboard: Vec<String>,
    /// Cropped PNG images requested by screenshotElement steps.
    pub screenshots: Vec<ElementScreenshot>,
    /// Alerts returned by alert steps or accepted open prompts.
    pub alerts: Vec<AlertInfo>,
    /// The screen after the run (or after the failure), without an image.
    pub snapshot: Option<Snapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementScreenshot {
    pub target: String,
    pub png: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertInfo {
    pub title: Option<String>,
    pub message: Option<String>,
    pub buttons: Vec<String>,
}

pub struct Agent<D: Device> {
    device: D,
    last: Mutex<Option<Snapshot>>,
    next_id: AtomicU64,
    refs: Mutex<Refs>,
    /// The screen as it was when the agent last acted, until accessibility shows a change.
    acted: Mutex<Option<Mark>>,
    fill_results: Mutex<Vec<bool>>,
    clipboard_results: Mutex<Vec<String>>,
    screenshots: Mutex<Vec<ElementScreenshot>>,
    secret_fields: Mutex<Vec<Element>>,
    alerts: Mutex<Vec<AlertInfo>>,
}

/// What the screen looked like (tree and pixels) just before an action.
#[derive(Clone)]
struct Mark {
    tree: TreeKey,
    print: Option<Vec<[u8; 3]>>,
    at: Instant,
    /// The action switches apps (home, lock): the tree must change, whatever the pixels say.
    switches: bool,
}

type TreeKey = (Option<String>, Signature);

fn tree_key(root: &AxNode, els: &[Element]) -> TreeKey {
    (root.label.clone(), signature(els))
}

/// Mean colour (R, G, B) over a coarse grid of the frame.
fn fingerprint(f: &Frame) -> Vec<[u8; 3]> {
    fingerprint_with(f, PRINT, 4)
}

/// Mean colour (R, G, B) over `cols × rows` cells, reading every `step`th pixel each way.
fn fingerprint_with(f: &Frame, (cols, rows): (usize, usize), step: usize) -> Vec<[u8; 3]> {
    let (w, h) = (f.width as usize, f.height as usize);
    let step = step.max(1);
    let mut out = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        for c in 0..cols {
            let (mut sum, mut n) = ([0u64; 3], 0u64);
            for y in (r * h / rows..(r + 1) * h / rows).step_by(step) {
                for x in (c * w / cols..(c + 1) * w / cols).step_by(step) {
                    if let Some(p) = f.data.get((y * w + x) * 4..(y * w + x) * 4 + 3) {
                        for (s, v) in sum.iter_mut().zip([p[2], p[1], p[0]]) {
                            *s += u64::from(v);
                        }
                        n += 1;
                    }
                }
            }
            out.push(sum.map(|s| s.checked_div(n).unwrap_or(0) as u8));
        }
    }
    out
}

/// Share of fingerprint cells whose colour changed noticeably.
fn changed(a: &[[u8; 3]], b: &[[u8; 3]]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    let differs = |x: &[u8; 3], y: &[u8; 3]| x.iter().zip(y).any(|(p, q)| p.abs_diff(*q) > 12);
    a.iter().zip(b).filter(|(x, y)| differs(x, y)).count() as f64 / a.len() as f64
}

/// Every ref handed out: the last number, and what each ref named in which snapshot.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Refs {
    pub count: u64,
    pub issued: HashMap<String, (String, u64)>,
}

/// Everything a fresh process needs to keep using the refs an earlier one issued: the
/// last snapshot (whose elements carry their refs), the ref ledger, and the snapshot
/// counter. Serialize it, hand it to `Agent::restore`, and `e4` means what it meant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    pub last: Option<Snapshot>,
    pub next_id: u64,
    pub refs: Refs,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `e` followed by digits, in either case: an element ref. Grid rows skip E, so a cell
/// name never reads as one.
fn is_ref(s: &str) -> bool {
    s.len() > 1 && s.starts_with(['e', 'E']) && s[1..].bytes().all(|b| b.is_ascii_digit())
}

type Signature = Vec<(String, Option<String>, [i64; 4])>;

fn signature(els: &[Element]) -> Signature {
    els.iter()
        .map(|e| {
            (
                e.role.clone(),
                e.label.clone(),
                e.frame.map(|v| v.round() as i64),
            )
        })
        .collect()
}

fn button(name: &str) -> Option<Button> {
    Some(match name {
        "home" => Button::Home,
        "lock" => Button::Lock,
        "side" => Button::SideButton,
        "siri" => Button::Siri,
        "volume-up" => Button::VolumeUp,
        "volume-down" => Button::VolumeDown,
        _ => return None,
    })
}

impl<D: Device> Agent<D> {
    pub fn new(device: D) -> Agent<D> {
        Agent {
            device,
            last: Mutex::new(None),
            next_id: AtomicU64::new(0),
            refs: Mutex::default(),
            acted: Mutex::new(None),
            fill_results: Mutex::default(),
            clipboard_results: Mutex::default(),
            screenshots: Mutex::default(),
            secret_fields: Mutex::default(),
            alerts: Mutex::default(),
        }
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    /// The latest snapshot, if any.
    pub fn last_snapshot(&self) -> Option<Snapshot> {
        lock(&self.last).clone()
    }

    /// The ref state to carry into another process (see `AgentState`).
    pub fn state(&self) -> AgentState {
        AgentState {
            last: lock(&self.last).clone(),
            next_id: self.next_id.load(Ordering::SeqCst),
            refs: lock(&self.refs).clone(),
        }
    }

    /// Continue from a state an earlier process saved. Refs it issued still resolve;
    /// new snapshot ids continue after the restored ones.
    pub fn restore(&self, state: AgentState) {
        *lock(&self.last) = state.last;
        *lock(&self.refs) = state.refs;
        self.next_id.store(state.next_id, Ordering::SeqCst);
    }

    fn screen(&self) -> Result<(f64, f64), AgentError> {
        self.device
            .screen_points()
            .ok_or_else(|| AgentError::NotReady("input is not connected yet".into()))
    }

    /// Read until the element list stops changing (or `settle` runs out).
    fn settled(
        &self,
        screen: (f64, f64),
        settle: Duration,
    ) -> Result<(AxNode, Vec<Element>, usize, bool), AgentError> {
        let t0 = Instant::now();
        let mut prev: Option<(Signature, Instant)> = None;
        loop {
            let root = self.device.accessibility()?;
            let (els, off) = marks::elements(&root, screen);
            let sig = signature(&els);
            let now = Instant::now();
            // Mid-transition (an app launching or closing), the frontmost element is an
            // unnamed SpringBoard with only the status bar: never a screen to settle on.
            let between =
                els.is_empty() && root.label.as_deref().is_none_or(|l| l.trim().is_empty());
            let lagging = self.tree_lags(&root, &els);
            let since = match prev {
                Some((p, since)) if p == sig && !between && !lagging => since,
                _ => now,
            };
            let need = if els.is_empty() {
                EMPTY_SETTLE
            } else {
                Duration::ZERO
            };
            if since < now && now - since >= need {
                return Ok((root, els, off, true));
            }
            if t0.elapsed() >= settle {
                return Ok((root, els, off, false));
            }
            prev = Some((sig, since));
            thread::sleep(POLL);
        }
    }

    /// Since the agent last acted, has the screen changed a lot while the tree has not?
    /// Then the tree is behind the pixels (an app switch) and must not be settled on.
    /// Clears the mark once the tree changes.
    fn tree_lags(&self, root: &AxNode, els: &[Element]) -> bool {
        let mut acted = lock(&self.acted);
        let Some(mark) = acted.as_ref() else {
            return false;
        };
        if tree_key(root, els) != mark.tree || mark.at.elapsed() > LAG_MAX {
            *acted = None; // caught up, or never will
            return false;
        }
        if mark.switches {
            return true;
        }
        let Some(before) = mark.print.as_ref() else {
            return false;
        };
        self.device
            .capture(Duration::from_millis(200))
            .is_some_and(|f| changed(before, &fingerprint(&f)) > LAG_CHANGE)
    }

    /// Remember the screen before an action (`root`: the tree just read, if any).
    /// `switches`: the action leaves the frontmost app (a home or lock press).
    fn mark(&self, root: Option<AxNode>, screen: (f64, f64), switches: bool) {
        let root = match root {
            Some(r) => r,
            None => match self.device.accessibility() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        let (els, _) = marks::elements(&root, screen);
        let print = self
            .device
            .capture(Duration::from_millis(200))
            .map(|f| fingerprint(&f));
        // Home on the home screen (SpringBoard's root is unnamed) switches nothing.
        let switches = switches && root.label.as_deref().is_some_and(|l| !l.trim().is_empty());
        *lock(&self.acted) = Some(Mark {
            tree: tree_key(&root, &els),
            print,
            at: Instant::now(),
            switches,
        });
    }

    /// Wait for the screen to settle, list its elements (or grid), and draw them.
    pub fn snapshot(&self, opts: &SnapshotOptions) -> Result<Snapshot, AgentError> {
        let screen = self.screen()?;
        let (_, mut elements, offscreen, settled) = self.settled(screen, opts.settle)?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.assign_refs(&mut elements, id);
        let grid = Grid::new(screen, opts.cols);
        let mode = if opts.grid || elements.is_empty() {
            Mode::Grid
        } else {
            Mode::Elements
        };
        let image = if opts.image {
            self.device.capture(CAPTURE_WAIT).map(|f| match mode {
                Mode::Elements => draw::elements(&f, screen, &elements),
                Mode::Grid => draw::grid(&f, &grid),
            })
        } else {
            None
        };
        let size = GridSize {
            rows: grid.rows,
            cols: grid.cols,
        };
        let text = describe(id, mode, screen, &elements, offscreen, &grid, settled);
        let snap = Snapshot {
            id,
            mode,
            screen: [screen.0, screen.1],
            elements,
            offscreen,
            grid: size,
            settled,
            text,
            image,
        };
        *lock(&self.last) = Some(Snapshot {
            image: None,
            ..snap.clone()
        });
        Ok(snap)
    }

    /// Elements that were in the last snapshot keep their refs (the nearest same-named
    /// one, for repeats like two "Edit" buttons); new ones get the next unused number.
    fn assign_refs(&self, els: &mut [Element], snapshot: u64) {
        let prev = lock(&self.last)
            .as_ref()
            .map(|s| s.elements.clone())
            .unwrap_or_default();
        let mut claimed = vec![false; prev.len()];
        let mut refs = lock(&self.refs);
        for e in els.iter_mut() {
            let same = prev
                .iter()
                .enumerate()
                .filter(|(i, p)| !claimed[*i] && p.same_identity(e))
                .min_by(|a, b| a.1.moved_by(e).total_cmp(&b.1.moved_by(e)));
            e.reference = match same {
                Some((i, p)) => {
                    claimed[i] = true;
                    p.reference.clone()
                }
                None => {
                    refs.count += 1;
                    format!("e{}", refs.count)
                }
            };
            refs.issued
                .insert(e.reference.clone(), (e.name().to_string(), snapshot));
        }
    }

    /// Tap an element by ref from the last snapshot (`e4`), by its label on screen
    /// (`"Log in"`), or a grid cell (`C4`). A label matching two reachable elements
    /// equally well is refused as `ambiguous` — use the snapshot's ref instead. A ref
    /// is re-read just before the touch:
    /// refused if it vanished or moved, is covered, or — with `expect` — its name no
    /// longer contains that text.
    pub fn tap(&self, target: &str, expect: Option<&str>) -> Result<Tapped, AgentError> {
        if is_ref(target) {
            let screen = self.screen()?;
            return self.tap_ref(&target.to_ascii_lowercase(), expect, screen, None, false);
        }
        let screen = self.screen()?;
        let (root, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
        if let Some(el) = self.find(&els, target, false)? {
            if let Some(want) = expect
                && !el.name().to_lowercase().contains(&want.to_lowercase())
            {
                return Err(AgentError::Stale(format!(
                    "{target:?} is {:?}, not {want:?}",
                    el.name()
                )));
            }
            return self.touch(target, &el.clone(), root, screen, None, false);
        }
        let cols = lock(&self.last)
            .as_ref()
            .map_or(Grid::DEFAULT_COLS, |s| s.grid.cols);
        self.tap_cell(target, screen, None, cols, false)
    }

    /// Double tap a ref from the last snapshot, a current label, or a grid cell.
    pub fn double_tap(&self, target: &str) -> Result<Tapped, AgentError> {
        self.act_on_mode(target, false, None, true)
    }

    fn tap_ref(
        &self,
        target: &str,
        expect: Option<&str>,
        screen: (f64, f64),
        hold: Option<Duration>,
        double: bool,
    ) -> Result<Tapped, AgentError> {
        let last = lock(&self.last).clone().ok_or_else(|| {
            AgentError::NotReady("no snapshot yet: take one, then use its refs".into())
        })?;
        let Some(el) = last.elements.iter().find(|e| e.reference == target) else {
            return Err(match lock(&self.refs).issued.get(target) {
                Some((name, seen)) => AgentError::Stale(format!(
                    "{target} {name:?} (snapshot {seen}) is not on the current screen (snapshot {}); use the current refs",
                    last.id
                )),
                None => AgentError::NotFound(format!(
                    "{target} was never issued; take a snapshot and use its refs"
                )),
            });
        };
        let root = self.device.accessibility()?;
        let (fresh, _) = marks::elements(&root, screen);
        let now = fresh
            .iter()
            .filter(|f| f.same_identity(el))
            .min_by(|a, b| a.moved_by(el).total_cmp(&b.moved_by(el)))
            .ok_or_else(|| {
                AgentError::Stale(format!(
                    "{target} {:?} is no longer on screen; take a new snapshot",
                    el.name()
                ))
            })?;
        let moved = now.moved_by(el);
        if moved > MOVE_TOLERANCE {
            return Err(AgentError::Stale(format!(
                "{target} {:?} moved {moved:.0} pt since snapshot {}; take a new snapshot",
                el.name(),
                last.id
            )));
        }
        if let Some(want) = expect
            && !now.name().to_lowercase().contains(&want.to_lowercase())
        {
            return Err(AgentError::Stale(format!(
                "{target} is {:?}, not {want:?}; take a new snapshot",
                now.name()
            )));
        }
        let now = now.clone();
        self.touch(target, &now, root, screen, hold, double)
    }

    /// Touch the element: a tap, or with `hold` a long press (iOS context menus).
    fn touch(
        &self,
        target: &str,
        el: &Element,
        root: AxNode,
        screen: (f64, f64),
        hold: Option<Duration>,
        double: bool,
    ) -> Result<Tapped, AgentError> {
        let p = el.tap.ok_or_else(|| {
            AgentError::Covered(format!(
                "{target} {:?} is covered by another element",
                el.name()
            ))
        })?;
        self.mark(Some(root), screen, false);
        let at = (p[0] / screen.0, p[1] / screen.1);
        match hold {
            Some(h) => self.device.long_press(at.0, at.1, h),
            None if double => self.device.double_tap(at.0, at.1),
            None => self.device.tap(at.0, at.1),
        }
        Ok(Tapped {
            target: target.to_string(),
            name: el.name().to_string(),
            at: p,
        })
    }

    /// Touch a grid cell's centre: a tap, or with `hold` a long press.
    fn tap_cell(
        &self,
        target: &str,
        screen: (f64, f64),
        hold: Option<Duration>,
        cols: u32,
        double: bool,
    ) -> Result<Tapped, AgentError> {
        let (x, y) = Grid::new(screen, cols).center(target).ok_or_else(|| {
            AgentError::NotFound(format!(
                "{target:?} is neither an element ref (e4) nor a grid cell (C4)"
            ))
        })?;
        self.mark(None, screen, false);
        match hold {
            Some(h) => self.device.long_press(x, y, h),
            None if double => self.device.double_tap(x, y),
            None => self.device.tap(x, y),
        }
        Ok(Tapped {
            target: target.to_string(),
            name: String::new(),
            at: [x * screen.0, y * screen.1],
        })
    }

    /// Touch a ref, a label on the current (settled) screen, or a grid cell: a tap, or
    /// with `hold` a long press.
    fn act_on(
        &self,
        target: &str,
        text_input: bool,
        hold: Option<Duration>,
    ) -> Result<Tapped, AgentError> {
        self.act_on_mode(target, text_input, hold, false)
    }

    fn act_on_mode(
        &self,
        target: &str,
        text_input: bool,
        hold: Option<Duration>,
        double: bool,
    ) -> Result<Tapped, AgentError> {
        if is_ref(target) {
            let screen = self.screen()?;
            return self.tap_ref(&target.to_ascii_lowercase(), None, screen, hold, double);
        }
        let screen = self.screen()?;
        let (root, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
        match self.find(&els, target, text_input)? {
            Some(el) => self.touch(target, &el.clone(), root, screen, hold, double),
            None if Grid::new(screen, Grid::DEFAULT_COLS)
                .center(target)
                .is_some() =>
            {
                let cols = lock(&self.last)
                    .as_ref()
                    .map_or(Grid::DEFAULT_COLS, |s| s.grid.cols);
                self.tap_cell(target, screen, hold, cols, double)
            }
            None => Err(AgentError::NotFound(format!(
                "nothing on screen is called {target:?}"
            ))),
        }
    }

    /// Resolve `target` to a touch point in screen points, validated against the screen
    /// now — the same rules as a tap, without touching: a ref is re-read and refused if
    /// it moved or vanished; a label is found on a settled screen; a grid cell resolves
    /// to its centre. `from_end` also refuses a covered element (a drag starts there).
    fn locate(&self, target: &str, from_end: bool) -> Result<[f64; 2], AgentError> {
        if is_ref(target) {
            let key = target.to_ascii_lowercase();
            let last = lock(&self.last).clone().ok_or_else(|| {
                AgentError::NotReady("no snapshot yet: take one, then use its refs".into())
            })?;
            let Some(el) = last.elements.iter().find(|e| e.reference == key) else {
                return Err(match lock(&self.refs).issued.get(&key) {
                    Some((name, seen)) => AgentError::Stale(format!(
                        "{target} {name:?} (snapshot {seen}) is not on the current screen (snapshot {}); use the current refs",
                        last.id
                    )),
                    None => AgentError::NotFound(format!(
                        "{target} was never issued; take a snapshot and use its refs"
                    )),
                });
            };
            let screen = (last.screen[0], last.screen[1]);
            let root = self.device.accessibility()?;
            let (fresh, _) = marks::elements(&root, screen);
            let now = fresh
                .iter()
                .filter(|f| f.same_identity(el))
                .min_by(|a, b| a.moved_by(el).total_cmp(&b.moved_by(el)))
                .ok_or_else(|| {
                    AgentError::Stale(format!(
                        "{target} {:?} is no longer on screen; take a new snapshot",
                        el.name()
                    ))
                })?;
            let moved = now.moved_by(el);
            if moved > MOVE_TOLERANCE {
                return Err(AgentError::Stale(format!(
                    "{target} {:?} moved {moved:.0} pt since snapshot {}; take a new snapshot",
                    el.name(),
                    last.id
                )));
            }
            return now.tap.ok_or_else(|| {
                AgentError::Covered(format!(
                    "{target} {:?} is covered by another element",
                    now.name()
                ))
            });
        }
        let screen = self.screen()?;
        let (_, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
        if let Some(el) = self.find(&els, target, false)? {
            if !from_end {
                // Dropping onto an element needs a point, which a covered element still has.
                let f = el.frame;
                return Ok(el.tap.unwrap_or([f[0] + f[2] / 2.0, f[1] + f[3] / 2.0]));
            }
            return el.tap.ok_or_else(|| {
                AgentError::Covered(format!(
                    "{target} {:?} is covered by another element",
                    el.name()
                ))
            });
        }
        let cols = lock(&self.last)
            .as_ref()
            .map_or(Grid::DEFAULT_COLS, |s| s.grid.cols);
        match Grid::new(screen, cols).center(target) {
            Some((x, y)) => Ok([x * screen.0, y * screen.1]),
            None => Err(AgentError::NotFound(format!(
                "nothing on screen is called {target:?}"
            ))),
        }
    }

    /// The element a label, identifier or value names: exact over partial, reachable
    /// first, text fields first for `fill`. Equal top-ranked matches are refused as
    /// ambiguous — the caller must use a ref — so this never silently picks one of
    /// several identical labels.
    fn find<'a>(
        &self,
        els: &'a [Element],
        target: &str,
        text_input: bool,
    ) -> Result<Option<&'a Element>, AgentError> {
        let want = target.to_lowercase();
        let rank = |e: &Element| {
            let names = [
                e.label.as_deref(),
                e.identifier.as_deref(),
                e.value.as_deref(),
            ];
            let exact = names.iter().flatten().any(|n| n.to_lowercase() == want);
            let partial = names
                .iter()
                .flatten()
                .any(|n| n.to_lowercase().contains(&want));
            let score = if exact {
                2
            } else if partial {
                1
            } else {
                0
            };
            (score, e.tap.is_some(), !text_input || e.takes_text())
        };
        let mut best: Option<&'a Element> = None;
        let mut best_rank = (0, false, false);
        let mut tied: Vec<&'a Element> = Vec::new();
        for e in els {
            let r = rank(e);
            if r.0 == 0 {
                continue;
            }
            if best.is_none_or(|_| r > best_rank) {
                best = Some(e);
                best_rank = r;
                tied.clear();
                tied.push(e);
            } else if r == best_rank {
                tied.push(e);
            }
        }
        let Some(found) = best else {
            return Ok(None);
        };
        if tied.len() > 1 {
            let list = tied
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {:?} at [{:.0}, {:.0}, {:.0}, {:.0}]",
                        e.reference,
                        e.role,
                        e.name(),
                        e.frame[0],
                        e.frame[1],
                        e.frame[2],
                        e.frame[3]
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AgentError::Ambiguous(format!(
                "{target:?} matches {} elements ({list}); take a snapshot and use its ref",
                tied.len()
            )));
        }
        Ok(Some(found))
    }

    fn field_value(
        &self,
        seed: &Element,
        screen: (f64, f64),
    ) -> Result<Option<String>, AgentError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let root = self.device.accessibility()?;
            let (fresh, _) = marks::elements(&root, screen);
            let inputs = fresh.iter().filter(|e| e.takes_text()).collect::<Vec<_>>();
            // The same field; else the nearest input; else the only one — a focused field
            // can move far and change its label (Settings' search jumps above the keyboard).
            let by_distance =
                |a: &&Element, b: &&Element| a.moved_by(seed).total_cmp(&b.moved_by(seed));
            let field = inputs
                .iter()
                .copied()
                .filter(|e| e.same_identity(seed))
                .min_by(by_distance)
                .or_else(|| {
                    inputs
                        .iter()
                        .copied()
                        .min_by(by_distance)
                        .filter(|e| e.moved_by(seed) < 80.0)
                })
                .or_else(|| (inputs.len() == 1).then(|| inputs[0]));
            if let Some(field) = field {
                return Ok(field.value.clone());
            }
            if Instant::now() >= deadline {
                return Err(AgentError::CommitNotObserved);
            }
            thread::sleep(POLL);
        }
    }

    fn element_frame(&self, target: &str, screen: (f64, f64)) -> Result<[f64; 4], AgentError> {
        if is_ref(target) {
            let last = lock(&self.last).clone().ok_or_else(|| {
                AgentError::NotReady("no snapshot yet: take one, then use its refs".into())
            })?;
            let seed = last
                .elements
                .iter()
                .find(|e| e.reference.eq_ignore_ascii_case(target))
                .ok_or_else(|| {
                    AgentError::NotFound(format!("{target} was not in the last snapshot"))
                })?;
            let _ = self.locate(target, false)?;
            let root = self.device.accessibility()?;
            let (fresh, _) = marks::elements(&root, screen);
            return fresh
                .iter()
                .filter(|e| e.same_identity(seed))
                .min_by(|a, b| a.moved_by(seed).total_cmp(&b.moved_by(seed)))
                .map(|e| e.frame)
                .ok_or_else(|| AgentError::Stale(format!("{target} is no longer on screen")));
        }
        let (_, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
        if let Some(e) = self.find(&els, target, false)? {
            return Ok(e.frame);
        }
        let cols = lock(&self.last)
            .as_ref()
            .map_or(Grid::DEFAULT_COLS, |s| s.grid.cols);
        Grid::new(screen, cols)
            .rect(target)
            .map(|r| {
                [
                    r[0] * screen.0,
                    r[1] * screen.1,
                    r[2] * screen.0,
                    r[3] * screen.1,
                ]
            })
            .ok_or_else(|| AgentError::NotFound(format!("nothing on screen is called {target:?}")))
    }

    fn fill(&self, target: &str, value: &str, secret: bool) -> Result<(), AgentError> {
        let screen = self.screen()?;
        let field = if is_ref(target) {
            let last = lock(&self.last).clone().ok_or_else(|| {
                AgentError::NotReady("no snapshot yet: take one, then use its refs".into())
            })?;
            last.elements
                .iter()
                .find(|e| e.reference.eq_ignore_ascii_case(target))
                .cloned()
        } else {
            let (_, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
            self.find(&els, target, false)?.cloned()
        }
        .ok_or_else(|| AgentError::NotFound(format!("nothing on screen is called {target:?}")))?;
        if !field.takes_text() {
            return Err(AgentError::NotTextInput(target.to_string()));
        }
        if secret {
            lock(&self.secret_fields).push(field.clone());
        }
        self.act_on(target, true, None)?;
        thread::sleep(FOCUS_WAIT);
        if !self.device.key("cmd+a") || !self.device.key("backspace") {
            return Err(AgentError::ClearNotObserved);
        }
        // Some system fields (Safari's address bar, Settings search) change their
        // keyboard/input mode asynchronously after clearing.
        thread::sleep(Duration::from_millis(500));
        let cleared_value = self.field_value(&field, screen)?;
        let is_empty = cleared_value.as_deref().is_none_or(str::is_empty)
            || cleared_value.as_deref() == field.placeholder.as_deref();
        if !is_empty {
            return Err(AgentError::ClearNotObserved);
        }
        for chunk in value.chars().collect::<Vec<_>>().chunks(128) {
            self.device.text(&chunk.iter().collect::<String>());
            thread::sleep(Duration::from_millis(20));
        }
        // Keystrokes land over time (slower under load): read back until the value
        // matches, or a mismatch (autocorrect, an input mask) holds for the whole wait.
        let deadline = Instant::now() + Duration::from_secs(2);
        let final_value = loop {
            let v = self.field_value(&field, screen)?;
            if v.as_deref() == Some(value) || Instant::now() >= deadline {
                break v;
            }
            thread::sleep(POLL);
        };
        let verified = match final_value.as_deref() {
            Some(actual) => actual == value && field.placeholder.as_deref() != Some(actual),
            None if field.placeholder.is_some() => false,
            None => return Err(AgentError::CommitNotObserved),
        };
        lock(&self.fill_results).push(verified);
        Ok(())
    }

    fn accept_open_prompt(&self, screen: (f64, f64)) {
        thread::sleep(Duration::from_millis(400));
        let Ok(root) = self.device.accessibility() else {
            return;
        };
        let Some(info) = alert_info(&root) else {
            return;
        };
        let text = format!(
            "{} {}",
            info.title.as_deref().unwrap_or(""),
            info.message.as_deref().unwrap_or("")
        );
        if !text.to_ascii_lowercase().contains("open in") {
            return;
        }
        let open = root.descendants().into_iter().find(|n| {
            n.role.ends_with("Button")
                && n.label
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case("open"))
        });
        if let Some(open) = open {
            let (x, y) = open.center();
            self.mark(None, screen, true);
            self.device.tap(x / screen.0, y / screen.1);
            lock(&self.alerts).push(info);
        }
    }

    fn run(&self, step: &Step) -> Result<(), AgentError> {
        match step {
            Step::Tap { target } => self.act_on(target, false, None).map(drop),
            Step::DoubleTap { target } => self.act_on_mode(target, false, None, true).map(drop),
            Step::Fill {
                target,
                value,
                secret,
            } => self.fill(target, value, *secret),
            Step::Scroll { direction, target } => {
                let screen = self.screen()?;
                // The region to drag in, in points: the target's visible part, else the
                // middle of the screen (clear of the system edge zones).
                let mut region = match target {
                    // A ref names an element of the last snapshot (fresh reads number afresh).
                    Some(t) if is_ref(t) => lock(&self.last)
                        .as_ref()
                        .and_then(|s| {
                            s.elements
                                .iter()
                                .find(|e| e.reference.eq_ignore_ascii_case(t))
                                .map(|e| e.frame)
                        })
                        .ok_or_else(|| {
                            AgentError::NotFound(format!("{t} is not in the last snapshot"))
                        })?,
                    Some(t) => {
                        let (_, els, _, _) =
                            self.settled(screen, SnapshotOptions::default().settle)?;
                        let el = self.find(&els, t, false)?.ok_or_else(|| {
                            AgentError::NotFound(format!("nothing on screen is called {t:?}"))
                        })?;
                        el.frame
                    }
                    None => [
                        screen.0 * 0.1,
                        screen.1 * 0.2,
                        screen.0 * 0.8,
                        screen.1 * 0.6,
                    ],
                };
                let root = self.device.accessibility()?;
                if let Some(keyboard) = keyboard_frame(&root, screen) {
                    let bottom = (keyboard[1] - region[1]).min(region[3]);
                    if bottom < 64.0 {
                        return Err(AgentError::KeyboardOccludes);
                    }
                    region[3] = bottom;
                }
                let (cx, cy) = (region[0] + region[2] / 2.0, region[1] + region[3] / 2.0);
                let (dx, dy) = (
                    region[2] * SCROLL_SHARE / 2.0,
                    region[3] * SCROLL_SHARE / 2.0,
                );
                // Revealing what is below drags the content up, and so on.
                let ((x0, y0), (x1, y1)) = match direction.as_str() {
                    "down" => ((cx, cy + dy), (cx, cy - dy)),
                    "up" => ((cx, cy - dy), (cx, cy + dy)),
                    "right" => ((cx + dx, cy), (cx - dx, cy)),
                    "left" => ((cx - dx, cy), (cx + dx, cy)),
                    other => {
                        return Err(AgentError::Invalid(format!(
                            "unknown direction {other:?} (up, down, left, right)"
                        )));
                    }
                };
                self.mark(None, screen, false);
                let n = |x: f64, y: f64| (x / screen.0, y / screen.1);
                self.device.swipe(n(x0, y0), n(x1, y1), SCROLL_TIME);
                Ok(())
            }
            Step::Drag {
                from,
                to,
                source_hold_ms,
                move_ms,
                destination_hold_ms,
            } => {
                let screen = self.screen()?;
                let a = self.locate(from, true)?;
                let b = self.locate(to, false)?;
                self.mark(None, screen, false);
                let n = |p: [f64; 2]| (p[0] / screen.0, p[1] / screen.1);
                self.device.drag(
                    n(a),
                    n(b),
                    Duration::from_millis(source_hold_ms.unwrap_or(800)),
                    Duration::from_millis(move_ms.unwrap_or(500)),
                    Duration::from_millis(destination_hold_ms.unwrap_or(0)),
                );
                Ok(())
            }
            Step::Pinch { target, scale } => {
                let screen = self.screen()?;
                let center = match target {
                    Some(target) => {
                        let p = self.locate(target, false)?;
                        (p[0] / screen.0, p[1] / screen.1)
                    }
                    None => (0.5, 0.5),
                };
                self.mark(None, screen, false);
                self.device.pinch(center, *scale)
            }
            Step::Rotate { target, degrees } => {
                let screen = self.screen()?;
                let center = match target {
                    Some(target) => {
                        let p = self.locate(target, false)?;
                        (p[0] / screen.0, p[1] / screen.1)
                    }
                    None => (0.5, 0.5),
                };
                self.mark(None, screen, false);
                self.device.rotate(center, *degrees)
            }
            Step::OpenUrl { url } => {
                let screen = self.screen()?;
                self.mark(None, screen, true);
                self.device.open_url(url).map_err(AgentError::Device)?;
                self.accept_open_prompt(screen);
                Ok(())
            }
            Step::Launch { bundle_id } => {
                let screen = self.screen()?;
                self.mark(None, screen, true);
                self.device.launch(bundle_id).map_err(AgentError::Device)
            }
            Step::Terminate { bundle_id } => {
                let screen = self.screen()?;
                self.mark(None, screen, true);
                self.device.terminate(bundle_id).map_err(AgentError::Device)
            }
            Step::SetLocation { lat, lon } => {
                let screen = self.screen()?;
                self.mark(None, screen, false);
                self.device
                    .set_location(*lat, *lon)
                    .map_err(AgentError::Device)
            }
            Step::Appearance { appearance } => {
                let screen = self.screen()?;
                let mode = match appearance.to_ascii_lowercase().as_str() {
                    "light" => simdriver::Appearance::Light,
                    "dark" => simdriver::Appearance::Dark,
                    _ => {
                        return Err(AgentError::Invalid(
                            "appearance must be light or dark".into(),
                        ));
                    }
                };
                self.mark(None, screen, false);
                self.device.set_appearance(mode).map_err(AgentError::Device)
            }
            Step::ClipboardRead => {
                let screen = self.screen()?;
                self.mark(None, screen, false);
                let value = self.device.clipboard_read().map_err(AgentError::Device)?;
                lock(&self.clipboard_results).push(value);
                Ok(())
            }
            Step::ClipboardWrite { text } => {
                let screen = self.screen()?;
                self.mark(None, screen, false);
                self.device
                    .clipboard_write(text)
                    .map_err(AgentError::Device)
            }
            Step::KeyboardDismiss => {
                let screen = self.screen()?;
                let root = self.device.accessibility()?;
                let Some(frame) = keyboard_frame(&root, screen) else {
                    return Ok(());
                };
                let done = root.descendants().into_iter().find(|n| {
                    n.role.ends_with("Button")
                        && n.label.as_deref().is_some_and(|s| {
                            matches!(
                                s.to_ascii_lowercase().as_str(),
                                "done" | "return" | "go" | "search" | "next" | "close"
                            )
                        })
                });
                self.mark(None, screen, false);
                if let Some(button) = done {
                    let (x, y) = button.center();
                    self.device.tap(x / screen.0, y / screen.1);
                } else {
                    self.device.key("enter");
                }
                thread::sleep(FOCUS_WAIT);
                let root = self.device.accessibility()?;
                if keyboard_frame(&root, screen).is_some() {
                    let x = (frame[0] + frame[2] / 2.0) / screen.0;
                    let y = (frame[1] - 80.0).max(8.0) / screen.1;
                    self.device.tap(x, y);
                    thread::sleep(FOCUS_WAIT);
                    let root = self.device.accessibility()?;
                    if keyboard_frame(&root, screen).is_some() {
                        return Err(AgentError::NotReady(
                            "on-screen keyboard is still visible".into(),
                        ));
                    }
                }
                Ok(())
            }
            Step::Back { mode } => {
                let screen = self.screen()?;
                match mode.to_ascii_lowercase().as_str() {
                    "system" => {
                        self.mark(None, screen, true);
                        self.device
                            .swipe((0.002, 0.5), (0.75, 0.5), Duration::from_millis(450));
                        Ok(())
                    }
                    "inapp" => {
                        let root = self.device.accessibility()?;
                        let (els, _) = marks::elements(&root, screen);
                        let navigation = els
                            .iter()
                            .filter(|e| e.role == "button")
                            .filter(|e| e.frame[0] < screen.0 * 0.5 && e.frame[1] < screen.1 * 0.18)
                            .filter(|e| e.label.as_deref().is_some_and(|s| !s.trim().is_empty()))
                            .collect::<Vec<_>>();
                        let back = navigation
                            .iter()
                            .copied()
                            .find(|e| {
                                e.label
                                    .as_deref()
                                    .is_some_and(|s| s.eq_ignore_ascii_case("back"))
                            })
                            .or_else(|| {
                                navigation
                                    .into_iter()
                                    .min_by(|a, b| a.frame[0].total_cmp(&b.frame[0]))
                            });
                        let Some(back) = back else {
                            return Err(AgentError::NotFound(
                                "in-app back button not found".into(),
                            ));
                        };
                        self.mark(None, screen, false);
                        let p = back.tap.ok_or_else(|| {
                            AgentError::Covered("in-app back button is covered".into())
                        })?;
                        self.device.tap(p[0] / screen.0, p[1] / screen.1);
                        Ok(())
                    }
                    _ => Err(AgentError::Invalid(
                        "back mode must be system or inApp".into(),
                    )),
                }
            }
            Step::AppSwitcher => {
                let screen = self.screen()?;
                self.mark(None, screen, true);
                self.device.button(Button::Home);
                thread::sleep(Duration::from_millis(150));
                self.device.button(Button::Home);
                Ok(())
            }
            Step::ScreenshotElement { target } => {
                let screen = self.screen()?;
                let rect = self.element_frame(target, screen)?;
                let frame = self
                    .device
                    .capture(CAPTURE_WAIT)
                    .ok_or(AgentError::CommitNotObserved)?;
                let png = draw::crop(&frame, screen, rect).ok_or(AgentError::CommitNotObserved)?;
                lock(&self.screenshots).push(ElementScreenshot {
                    target: target.clone(),
                    png,
                });
                Ok(())
            }
            Step::Alert { op, ms } => {
                let screen = self.screen()?;
                let deadline = Instant::now() + Duration::from_millis(ms.unwrap_or(5000));
                let root = loop {
                    let root = self.device.accessibility()?;
                    if alert_info(&root).is_some() {
                        break root;
                    }
                    if op.eq_ignore_ascii_case("wait") && Instant::now() < deadline {
                        thread::sleep(POLL);
                        continue;
                    }
                    return Err(AgentError::NotFound("no system alert is visible".into()));
                };
                let info = alert_info(&root).expect("checked above");
                match op.to_ascii_lowercase().as_str() {
                    "get" | "wait" => lock(&self.alerts).push(info),
                    "accept" | "dismiss" => {
                        let accepted = op.eq_ignore_ascii_case("accept");
                        let button = alert_button(&root, accepted).ok_or_else(|| {
                            AgentError::NotFound(
                                if accepted {
                                    "no recognized allow/accept button"
                                } else {
                                    "no safe dismiss button match"
                                }
                                .into(),
                            )
                        })?;
                        self.mark(None, screen, true);
                        let (x, y) = button.center();
                        self.device.tap(x / screen.0, y / screen.1);
                        lock(&self.alerts).push(info);
                    }
                    _ => {
                        return Err(AgentError::Invalid(
                            "alert op must be get, wait, accept, or dismiss".into(),
                        ));
                    }
                }
                Ok(())
            }
            Step::Pan { target, dx, dy, ms } => {
                let screen = self.screen()?;
                let center = match target {
                    Some(target) => {
                        let p = self.locate(target, false)?;
                        (p[0] / screen.0, p[1] / screen.1)
                    }
                    None => (0.5, 0.5),
                };
                self.mark(None, screen, false);
                self.device.transform(
                    center,
                    *dx,
                    *dy,
                    1.0,
                    0.0,
                    Duration::from_millis(ms.unwrap_or(500)),
                )
            }
            Step::Transform {
                target,
                dx,
                dy,
                scale,
                degrees,
            } => {
                let screen = self.screen()?;
                let center = match target {
                    Some(target) => {
                        let p = self.locate(target, false)?;
                        (p[0] / screen.0, p[1] / screen.1)
                    }
                    None => (0.5, 0.5),
                };
                self.mark(None, screen, false);
                self.device.transform(
                    center,
                    *dx,
                    *dy,
                    *scale,
                    *degrees,
                    Duration::from_millis(500),
                )
            }
            Step::Fling { direction } => {
                let screen = self.screen()?;
                let (from, to) = match direction.to_ascii_lowercase().as_str() {
                    "up" => ((0.5, 0.78), (0.5, 0.22)),
                    "down" => ((0.5, 0.22), (0.5, 0.78)),
                    "left" => ((0.8, 0.5), (0.2, 0.5)),
                    "right" => ((0.2, 0.5), (0.8, 0.5)),
                    other => {
                        return Err(AgentError::Invalid(format!(
                            "unknown direction {other:?} (up, down, left, right)"
                        )));
                    }
                };
                self.mark(None, screen, false);
                self.device.swipe(from, to, Duration::from_millis(150));
                Ok(())
            }
            Step::Swipe {
                from,
                to,
                count,
                pause_ms,
            } => {
                let screen = self.screen()?;
                let point = |target: &SwipeTarget| -> Result<(f64, f64), AgentError> {
                    match target {
                        SwipeTarget::Target(target) => {
                            let p = self.locate(target, false)?;
                            Ok((p[0] / screen.0, p[1] / screen.1))
                        }
                        SwipeTarget::Point([x, y])
                            if x.is_finite()
                                && y.is_finite()
                                && (0.0..=1.0).contains(x)
                                && (0.0..=1.0).contains(y) =>
                        {
                            Ok((*x, *y))
                        }
                        SwipeTarget::Point(_) => Err(AgentError::Invalid(
                            "swipe points must be finite and within 0..1".into(),
                        )),
                    }
                };
                let from = point(from)?;
                let to = point(to)?;
                let count = count.unwrap_or(1);
                if !(1..=10).contains(&count) {
                    return Err(AgentError::Invalid("swipe count must be 1–10".into()));
                }
                self.mark(None, screen, false);
                for i in 0..count {
                    self.device.swipe(from, to, Duration::from_millis(500));
                    if i + 1 < count {
                        thread::sleep(Duration::from_millis(pause_ms.unwrap_or(200)));
                    }
                }
                Ok(())
            }
            Step::LongPress { target, ms } => {
                let hold = Duration::from_millis(ms.unwrap_or(800));
                self.act_on(target, false, Some(hold)).map(drop)
            }
            Step::Type { value } => {
                self.mark(None, self.screen()?, false);
                self.device.text(value);
                Ok(())
            }
            Step::Key { name } => {
                self.mark(None, self.screen()?, false);
                if self.device.key(name) {
                    Ok(())
                } else {
                    Err(AgentError::Invalid(format!("unknown key {name:?}")))
                }
            }
            Step::Press { button: name } => {
                let b = button(name)
                    .ok_or_else(|| AgentError::Invalid(format!("unknown button {name:?}")))?;
                let switches = matches!(b, Button::Home | Button::Lock | Button::SideButton);
                self.mark(None, self.screen()?, switches);
                self.device.button(b);
                Ok(())
            }
            Step::WaitFor { text, timeout_ms } => {
                let wait = timeout_ms.map_or(WAIT_FOR_DEFAULT, Duration::from_millis);
                let want = text.to_lowercase();
                let t0 = Instant::now();
                loop {
                    let root = self.device.accessibility()?;
                    if marks::texts(&root).any(|t| t.to_lowercase().contains(&want)) {
                        return Ok(());
                    }
                    if t0.elapsed() >= wait {
                        return Err(AgentError::NotFound(format!(
                            "{text:?} did not appear within {:.1} s",
                            wait.as_secs_f64()
                        )));
                    }
                    thread::sleep(POLL);
                }
            }
        }
    }

    /// Run `steps` in order; stop at the first failure. The report ends with a fresh
    /// snapshot (no image), which also becomes the one refs refer to.
    pub fn steps(&self, steps: &[Step]) -> StepsReport {
        lock(&self.fill_results).clear();
        lock(&self.clipboard_results).clear();
        lock(&self.screenshots).clear();
        lock(&self.secret_fields).clear();
        lock(&self.alerts).clear();
        let mut failed = None;
        let mut completed = 0;
        for (index, step) in steps.iter().enumerate() {
            match self.run(step) {
                Ok(()) => completed += 1,
                Err(e) => {
                    let safe_step = match step {
                        Step::Fill {
                            target,
                            secret: true,
                            ..
                        } => Step::Fill {
                            target: target.clone(),
                            value: "[redacted]".into(),
                            secret: true,
                        },
                        _ => step.clone(),
                    };
                    failed = Some(StepFailure {
                        index,
                        step: safe_step,
                        code: e.code().into(),
                        error: e.to_string(),
                    });
                    break;
                }
            }
        }
        let mut snapshot = self
            .snapshot(&SnapshotOptions {
                image: false,
                ..SnapshotOptions::default()
            })
            .ok();
        if let Some(s) = snapshot.as_mut() {
            let secrets = lock(&self.secret_fields);
            for e in &mut s.elements {
                if secrets.iter().any(|secret| e.same_identity(secret)) {
                    e.value = None;
                }
            }
            s.text = describe(
                s.id,
                s.mode,
                (s.screen[0], s.screen[1]),
                &s.elements,
                s.offscreen,
                &Grid::new((s.screen[0], s.screen[1]), s.grid.cols),
                s.settled,
            );
        }
        let verified = lock(&self.fill_results).clone();
        let clipboard = lock(&self.clipboard_results).clone();
        let screenshots = lock(&self.screenshots).clone();
        let alerts = lock(&self.alerts).clone();
        StepsReport {
            ok: failed.is_none(),
            completed,
            failed,
            verified,
            clipboard,
            screenshots,
            alerts,
            snapshot,
        }
    }
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"))
}

fn keyboard_frame(root: &AxNode, screen: (f64, f64)) -> Option<[f64; 4]> {
    if let Some(n) = root
        .descendants()
        .into_iter()
        .find(|n| n.role.to_ascii_lowercase().contains("keyboard"))
    {
        return Some(n.frame);
    }
    let keys: Vec<_> = root
        .descendants()
        .into_iter()
        .filter(|n| {
            let y = n.frame[1];
            y > screen.1 * 0.48
                && n.frame[2] > 0.0
                && n.frame[3] > 0.0
                && (n.role.ends_with("Button") || n.role.ends_with("Key"))
                && n.label.as_deref().is_some_and(|s| s.chars().count() <= 8)
        })
        .collect();
    if keys.len() < 8 {
        return None;
    }
    let x0 = keys
        .iter()
        .map(|n| n.frame[0])
        .fold(f64::INFINITY, f64::min);
    let y0 = keys
        .iter()
        .map(|n| n.frame[1])
        .fold(f64::INFINITY, f64::min);
    let x1 = keys
        .iter()
        .map(|n| n.frame[0] + n.frame[2])
        .fold(0.0, f64::max);
    let y1 = keys
        .iter()
        .map(|n| n.frame[1] + n.frame[3])
        .fold(0.0, f64::max);
    Some([x0, y0, x1 - x0, y1 - y0])
}

fn alert_info(root: &AxNode) -> Option<AlertInfo> {
    let all = root.descendants();
    let scope = match all
        .iter()
        .find(|n| n.role.to_ascii_lowercase().contains("alert"))
    {
        Some(alert) => *alert,
        // No AXAlert container (iOS 27's permission sheets, prompts found by hit-testing):
        // only a prompt's own button set counts — an allow choice with a safe denial, or
        // Open with Cancel — scoped to the smallest container holding it.
        None => all
            .iter()
            .copied()
            .filter(|n| is_prompt(n))
            .min_by_key(|n| n.descendants().len())?,
    };
    let nodes = scope.descendants();
    let text = |n: &&AxNode| n.label.clone().or_else(|| n.value.clone());
    // The status bar's clock and carrier are on screen, never part of a prompt.
    let texts: Vec<String> = nodes
        .iter()
        .filter(|n| n.role.ends_with("StaticText") && n.frame[1] + n.frame[3] > STATUS_BAR)
        .filter_map(text)
        .collect();
    let buttons: Vec<String> = nodes
        .iter()
        .filter(|n| n.role.ends_with("Button"))
        .filter_map(text)
        .collect();
    Some(AlertInfo {
        title: texts.first().cloned(),
        message: (texts.len() > 1).then(|| texts[1..].join(" ")),
        buttons,
    })
}

/// Height of the status bar strip, in points (taller on Dynamic Island phones).
const STATUS_BAR: f64 = 54.0;

fn is_prompt(n: &AxNode) -> bool {
    let labels: Vec<String> = n
        .descendants()
        .into_iter()
        .filter(|n| n.role.ends_with("Button"))
        .filter_map(|n| n.label.as_deref().map(normalized_alert_label))
        .collect();
    let has = |s: &str| labels.iter().any(|l| l == s);
    let allow = has("allow while using app") || has("allow once") || has("allow");
    (allow && has("don't allow")) || (has("open") && has("cancel"))
}

fn normalized_alert_label(s: &str) -> String {
    s.to_ascii_lowercase().replace('’', "'")
}

fn alert_button(root: &AxNode, accepted: bool) -> Option<&AxNode> {
    let rank = |n: &AxNode| {
        let label = normalized_alert_label(n.label.as_deref().or(n.value.as_deref()).unwrap_or(""));
        if accepted {
            [
                "Allow While Using App",
                "Allow Once",
                "Allow",
                "OK",
                "Open",
                "Continue",
                "Done",
            ]
            .iter()
            .position(|s| label == s.to_ascii_lowercase())
            .unwrap_or(usize::MAX)
        } else if ["cancel", "don't allow", "ask app not to track", "not now"]
            .iter()
            .any(|s| label == *s)
        {
            0
        } else {
            usize::MAX
        }
    };
    root.descendants()
        .into_iter()
        .filter(|n| n.role.ends_with("Button"))
        .map(|n| (rank(n), n))
        .filter(|(rank, _)| *rank != usize::MAX)
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, n)| n)
}

/// The snapshot as the model reads it.
fn describe(
    id: u64,
    mode: Mode,
    screen: (f64, f64),
    els: &[Element],
    offscreen: usize,
    grid: &Grid,
    settled: bool,
) -> String {
    let mut out = String::new();
    let last_row = grid.name(grid.rows - 1, 0);
    let rows = &last_row[..last_row.len() - 1];
    let unsettled = if settled { "" } else { " · still changing" };
    let (w, h) = (screen.0.round(), screen.1.round());
    match mode {
        Mode::Elements => {
            out.push_str(&format!(
                "snapshot {id} · {} elements · {w}×{h} pt{unsettled}\n",
                els.len()
            ));
            for e in els {
                out.push_str(&format!("{} {}", e.reference, e.role));
                let name = e.label.as_deref().or(e.identifier.as_deref());
                if let Some(n) = name {
                    out.push_str(&format!(" {}", quoted(n)));
                }
                if let Some(v) = e.value.as_deref().filter(|v| Some(*v) != name) {
                    out.push_str(&format!(" = {}", quoted(v)));
                }
                match e.visibility {
                    Visibility::Visible => {}
                    Visibility::Partial => out.push_str(" (partly hidden)"),
                    Visibility::Covered => out.push_str(" (covered: cannot tap)"),
                }
                out.push('\n');
            }
            if offscreen > 0 {
                out.push_str(&format!("+{offscreen} more off screen (scroll to reach)\n"));
            }
            out.push_str(&format!(
                "targets: refs above; grid cells A1–{rows}{} also work (rows A–{rows} top to bottom, no E; columns 1–{} left to right)\n",
                grid.cols, grid.cols
            ));
        }
        Mode::Grid => {
            out.push_str(&format!(
                "snapshot {id} · grid {}×{} · {w}×{h} pt{unsettled}\n",
                grid.cols, grid.rows
            ));
            if els.is_empty() {
                out.push_str("no accessible elements on screen; ");
            }
            out.push_str(&format!(
                "target grid cells: rows A–{rows} top to bottom (no E), columns 1–{} left to right (e.g. C4)\n",
                grid.cols
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::tests::{node, settings};
    use std::sync::atomic::AtomicBool;

    type OnTap = Box<dyn Fn(&Fake, f64, f64) + Send + Sync>;
    type OnButton = Box<dyn Fn(&Fake) + Send + Sync>;
    type Swipe = ((f64, f64), (f64, f64));
    type Touch2 = (simdriver::Phase, (f64, f64), (f64, f64));

    /// A scripted device: `screen` is what accessibility reads; taps and typing are logged,
    /// and `on_tap` can change the screen (navigation). `feed` plays a frame arriving
    /// from the stream: the newest frame `capture` returns, offered to an open tap.
    pub(crate) struct Fake {
        screen: Mutex<AxNode>,
        taps: Mutex<Vec<(f64, f64)>>,
        double_taps: Mutex<Vec<(f64, f64)>>,
        long_presses: Mutex<Vec<(f64, f64, Duration)>>,
        typed: Mutex<Vec<String>>,
        swipes: Mutex<Vec<Swipe>>,
        drags: Mutex<Vec<(Swipe, Duration, Duration, Duration)>>,
        touch2: Mutex<Vec<Touch2>>,
        reads: AtomicU64,
        on_tap: Mutex<Option<OnTap>>,
        on_button: Mutex<Option<OnButton>>,
        pub(crate) frame: Mutex<Option<Frame>>,
        focused: Mutex<Option<(f64, f64)>>,
        selected: AtomicBool,
        /// The feeder of the last tap opened; set to `None` to close the stream.
        pub(crate) tap: Mutex<Option<simdriver::TapFeeder>>,
        /// `frame_tap` returns `None` (a device that cannot record).
        pub(crate) tapless: AtomicBool,
    }

    impl Fake {
        pub(crate) fn new(screen: AxNode) -> Fake {
            Fake {
                screen: Mutex::new(screen),
                taps: Mutex::default(),
                double_taps: Mutex::default(),
                long_presses: Mutex::default(),
                typed: Mutex::default(),
                swipes: Mutex::default(),
                drags: Mutex::default(),
                touch2: Mutex::default(),
                reads: AtomicU64::new(0),
                on_tap: Mutex::new(None),
                on_button: Mutex::new(None),
                frame: Mutex::new(None),
                focused: Mutex::new(None),
                selected: AtomicBool::new(false),
                tap: Mutex::new(None),
                tapless: AtomicBool::new(false),
            }
        }
        /// A frame arriving at `at`: offered to the open tap, then the newest frame.
        pub(crate) fn feed(&self, at: Instant, frame: Frame) {
            if let Some(feeder) = lock(&self.tap).as_ref() {
                feeder.offer(at, &frame);
            }
            *lock(&self.frame) = Some(frame);
        }
        fn set(&self, screen: AxNode) {
            *lock(&self.screen) = screen;
        }
        fn set_field_value(node: &mut AxNode, x: f64, y: f64, value: String) -> bool {
            if node.role.ends_with("TextField")
                && x >= node.frame[0]
                && x <= node.frame[0] + node.frame[2]
                && y >= node.frame[1]
                && y <= node.frame[1] + node.frame[3]
            {
                node.value = Some(value);
                return true;
            }
            node.children
                .iter_mut()
                .any(|c| Self::set_field_value(c, x, y, value.clone()))
        }
    }

    impl Device for Fake {
        fn accessibility(&self) -> Result<AxNode, simdriver::Error> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(lock(&self.screen).clone())
        }
        fn screen_points(&self) -> Option<(f64, f64)> {
            Some((400.0, 800.0))
        }
        fn capture(&self, _: Duration) -> Option<Frame> {
            lock(&self.frame).clone()
        }
        fn frame_tap(&self, fps: u32) -> Option<simdriver::FrameTap> {
            if self.tapless.load(Ordering::SeqCst) {
                return None;
            }
            let (tap, feeder) = simdriver::FrameTap::manual(fps);
            *lock(&self.tap) = Some(feeder);
            Some(tap)
        }
        fn tap(&self, x: f64, y: f64) {
            lock(&self.taps).push((x, y));
            *lock(&self.focused) = Some((x * 400.0, y * 800.0));
            if let Some(f) = lock(&self.on_tap).as_ref() {
                f(self, x, y);
            }
        }
        fn double_tap(&self, x: f64, y: f64) {
            lock(&self.double_taps).push((x, y));
        }
        fn long_press(&self, x: f64, y: f64, hold: Duration) {
            lock(&self.long_presses).push((x, y, hold));
        }
        fn swipe(&self, from: (f64, f64), to: (f64, f64), _: Duration) {
            lock(&self.swipes).push((from, to));
        }
        fn drag(
            &self,
            from: (f64, f64),
            to: (f64, f64),
            source: Duration,
            duration: Duration,
            destination: Duration,
        ) {
            lock(&self.drags).push(((from, to), source, duration, destination));
        }
        fn touch2(&self, phase: simdriver::Phase, a: (f64, f64), b: (f64, f64)) {
            lock(&self.touch2).push((phase, a, b));
        }
        fn text(&self, s: &str) {
            lock(&self.typed).push(s.to_string());
            if let Some((x, y)) = *lock(&self.focused) {
                let mut root = lock(&self.screen);
                let old = root
                    .descendants()
                    .into_iter()
                    .find(|n| {
                        n.role.ends_with("TextField")
                            && x >= n.frame[0]
                            && x <= n.frame[0] + n.frame[2]
                            && y >= n.frame[1]
                            && y <= n.frame[1] + n.frame[3]
                    })
                    .and_then(|n| n.value.clone())
                    .unwrap_or_default();
                let next = if self.selected.swap(false, Ordering::SeqCst) {
                    s.to_string()
                } else {
                    format!("{old}{s}")
                };
                Self::set_field_value(&mut root, x, y, next);
            }
        }
        fn key(&self, name: &str) -> bool {
            match name {
                "cmd+a" => {
                    self.selected.store(true, Ordering::SeqCst);
                    true
                }
                "backspace" => {
                    if let Some((x, y)) = *lock(&self.focused) {
                        Self::set_field_value(&mut lock(&self.screen), x, y, String::new());
                        self.selected.store(false, Ordering::SeqCst);
                        true
                    } else {
                        false
                    }
                }
                "enter" => true,
                _ => false,
            }
        }
        fn button(&self, _: Button) {
            if let Some(f) = lock(&self.on_button).as_ref() {
                f(self);
            }
        }
        fn open_url(&self, _: &str) -> Result<(), simdriver::Error> {
            Ok(())
        }
        fn launch(&self, _: &str) -> Result<(), simdriver::Error> {
            Ok(())
        }
        fn terminate(&self, _: &str) -> Result<(), simdriver::Error> {
            Ok(())
        }
        fn set_location(&self, _: f64, _: f64) -> Result<(), simdriver::Error> {
            Ok(())
        }
        fn set_appearance(&self, _: simdriver::Appearance) -> Result<(), simdriver::Error> {
            Ok(())
        }
        fn clipboard_read(&self) -> Result<String, simdriver::Error> {
            Ok(String::new())
        }
        fn clipboard_write(&self, _: &str) -> Result<(), simdriver::Error> {
            Ok(())
        }
    }

    fn solid(v: u8) -> Frame {
        Frame {
            width: 400,
            height: 800,
            source_width: 400,
            source_height: 800,
            seq: 2,
            data: vec![v; 400 * 800 * 4],
        }
    }

    fn quick() -> SnapshotOptions {
        SnapshotOptions {
            settle: Duration::from_millis(300),
            ..SnapshotOptions::default()
        }
    }

    #[test]
    fn snapshot_lists_refs_as_text_and_draws_them() {
        let fake = Fake::new(settings());
        *lock(&fake.frame) = Some(solid(0));
        let a = Agent::new(fake);
        let s = a.snapshot(&quick()).unwrap();
        assert_eq!(
            (s.id, s.mode, s.settled, s.offscreen),
            (1, Mode::Elements, true, 1)
        );
        assert_eq!(
            s.text,
            "snapshot 1 · 5 elements · 400×800 pt\n\
             e1 button \"General\"\n\
             e2 button \"Screen Time\" (partly hidden)\n\
             e3 button \"Passcode\" (covered: cannot tap)\n\
             e4 button \"Privacy\" (covered: cannot tap)\n\
             e5 textfield \"Search\"\n\
             +1 more off screen (scroll to reach)\n\
             targets: refs above; grid cells A1–M6 also work (rows A–M top to bottom, no E; columns 1–6 left to right)\n"
        );
        let png = s.image.expect("image");
        assert_eq!(&png[1..4], b"PNG");
        assert!(
            a.last_snapshot().unwrap().image.is_none(),
            "the stored copy drops the image"
        );
        assert_eq!(a.snapshot(&quick()).unwrap().id, 2);
    }

    #[test]
    fn an_empty_screen_becomes_a_grid_once_it_stays_empty() {
        let a = Agent::new(Fake::new(node(
            "AXApplication",
            "Maps",
            [0.0, 0.0, 400.0, 800.0],
            vec![],
        )));
        let t0 = Instant::now();
        let s = a
            .snapshot(&SnapshotOptions {
                image: false,
                ..SnapshotOptions::default()
            })
            .unwrap();
        assert!(
            t0.elapsed() >= EMPTY_SETTLE,
            "an empty screen waits {EMPTY_SETTLE:?} to be sure"
        );
        assert_eq!((s.mode, s.settled), (Mode::Grid, true));
        assert_eq!(s.grid, GridSize { rows: 12, cols: 6 });
        assert!(
            s.text.contains(
                "no accessible elements on screen; target grid cells: rows A–M top to bottom (no E)"
            ),
            "{}",
            s.text
        );
        // Cells tap their centres.
        let t = a.tap("C4", None).unwrap();
        assert_eq!(t.at, [400.0 * 3.5 / 6.0, 800.0 * 2.5 / 12.0]);
    }

    #[test]
    fn an_app_launch_is_not_mistaken_for_an_empty_screen() {
        // SpringBoard's status bar only, as read while an app launches.
        let launching = node(
            "AXApplication",
            " ",
            [0.0, 0.0, 400.0, 800.0],
            vec![node(
                "AXStaticText",
                "08:34",
                [56.0, 21.0, 52.0, 22.0],
                vec![],
            )],
        );
        let a = Agent::new(Fake::new(launching));
        let fake = &a.device;
        let s = thread::scope(|sc| {
            sc.spawn(|| {
                thread::sleep(Duration::from_millis(1500)); // longer than EMPTY_SETTLE
                fake.set(settings());
            });
            a.snapshot(&SnapshotOptions {
                image: false,
                ..SnapshotOptions::default()
            })
            .unwrap()
        });
        assert_eq!((s.mode, s.settled), (Mode::Elements, true), "{}", s.text);
    }

    #[test]
    fn refs_stay_with_their_elements_and_are_never_reused() {
        let a = Agent::new(Fake::new(login()));
        let s1 = a.snapshot(&quick()).unwrap();
        let refs = |s: &Snapshot| {
            s.elements
                .iter()
                .map(|e| (e.reference.clone(), e.name().to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            refs(&s1),
            [
                ("e1".into(), "Email".into()),
                ("e2".into(), "Password".into()),
                ("e3".into(), "Log in".into())
            ]
        );

        // A banner appears above the form and pushes it down: the form keeps its refs, the
        // banner gets the next number.
        let mut t = login();
        for c in &mut t.children {
            c.frame[1] += 60.0;
        }
        t.children.insert(
            0,
            node("AXButton", "Dismiss", [20.0, 40.0, 360.0, 40.0], vec![]),
        );
        a.device.set(t);
        let s2 = a.snapshot(&quick()).unwrap();
        assert_eq!(
            refs(&s2),
            [
                ("e4".into(), "Dismiss".into()),
                ("e1".into(), "Email".into()),
                ("e2".into(), "Password".into()),
                ("e3".into(), "Log in".into())
            ]
        );

        // Another screen: none of the old refs are on it, and none are reused.
        a.device.set(node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![node("AXButton", "Back", [0.0, 50.0, 80.0, 44.0], vec![])],
        ));
        let s3 = a.snapshot(&quick()).unwrap();
        assert_eq!(refs(&s3), [("e5".into(), "Back".into())]);
        match a.tap("e3", None) {
            Err(AgentError::Stale(m)) => assert!(
                m.contains("\"Log in\" (snapshot 2) is not on the current screen (snapshot 3)"),
                "{m}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(matches!(a.tap("e42", None), Err(AgentError::NotFound(_))));
        assert!(lock(&a.device.taps).is_empty());
    }

    /// Run `steps` while, once `trigger` holds, the tree switches to `then` after 600 ms;
    /// returns how long the run took.
    fn lagged(
        a: &Agent<Fake>,
        steps: &[Step],
        trigger: impl Fn(&Fake) -> bool + Sync,
        then: AxNode,
    ) -> Duration {
        let fake = &a.device;
        let t0 = Instant::now();
        thread::scope(|sc| {
            sc.spawn(|| {
                while !trigger(fake) {
                    thread::sleep(Duration::from_millis(5));
                }
                thread::sleep(Duration::from_millis(600));
                fake.set(then);
            });
            assert!(a.steps(steps).ok);
        });
        t0.elapsed()
    }

    #[test]
    fn a_tree_that_lags_the_screen_is_not_settled_on() {
        // Home: accessibility keeps reporting the old app for a while (≈3 s on a real
        // simulator, 600 ms here), even when the pixels hardly change (dark on dark).
        let a = Agent::new(Fake::new(login()));
        *lock(&a.device.frame) = Some(solid(30));
        // Dark app → dark home screen: the frame barely changes.
        *lock(&a.device.on_button) = Some(Box::new(|f: &Fake| *lock(&f.frame) = Some(solid(31))));
        let pressed = |f: &Fake| lock(&f.frame).as_ref().is_some_and(|fr| fr.data[0] == 31);
        let took = lagged(
            &a,
            &[Step::Press {
                button: "home".into(),
            }],
            pressed,
            welcome(),
        );
        assert!(
            took >= Duration::from_millis(600),
            "settled on the stale tree after {took:?}"
        );
        assert!(
            lock(&a.acted).is_none(),
            "the mark clears once the tree catches up"
        );

        // A tap that changes the screen a lot (an app opening): same, from the pixels.
        a.device.set(login());
        *lock(&a.device.frame) = Some(solid(30));
        *lock(&a.device.on_tap) = Some(Box::new(|f: &Fake, _, _| {
            *lock(&f.frame) = Some(solid(200))
        }));
        let took = lagged(
            &a,
            &[Step::Tap {
                target: "Log in".into(),
            }],
            |f| lock(&f.frame).as_ref().is_some_and(|fr| fr.data[0] == 200),
            welcome(),
        );
        assert!(took >= Duration::from_millis(600), "{took:?}");

        // A tap that changes little (a cursor blinks, a row highlights) holds nothing up.
        a.device.set(login());
        *lock(&a.device.frame) = Some(solid(30));
        *lock(&a.device.on_tap) = Some(Box::new(|f: &Fake, _, _| {
            let mut fr = solid(30);
            fr.data[..4000].fill(255);
            *lock(&f.frame) = Some(fr);
        }));
        let t0 = Instant::now();
        assert!(
            a.steps(&[Step::Tap {
                target: "Log in".into()
            }])
            .ok
        );
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn settling_waits_out_a_changing_screen() {
        let a = Agent::new(Fake::new(settings()));
        // Every read moves "General" a little: never settles; the snapshot says so.
        let fake = &a.device;
        let stop = std::sync::atomic::AtomicBool::new(false);
        let s = thread::scope(|sc| {
            sc.spawn(|| {
                let mut y = 200.0;
                while !stop.load(Ordering::SeqCst) {
                    y += 1.0;
                    let mut t = settings();
                    t.children[1].children[0].frame[1] = y;
                    fake.set(t);
                    thread::sleep(Duration::from_millis(20));
                }
            });
            let s = a.snapshot(&quick()).unwrap();
            stop.store(true, Ordering::SeqCst);
            s
        });
        assert!(!s.settled);
        assert!(s.text.contains("still changing"));
    }

    #[test]
    fn taps_are_checked_against_the_screen_at_the_moment_of_the_touch() {
        let a = Agent::new(Fake::new(settings()));
        assert!(
            matches!(a.tap("e1", None), Err(AgentError::NotReady(_))),
            "no snapshot yet"
        );
        a.snapshot(&quick()).unwrap();

        let t = a.tap("E1", Some("general")).unwrap();
        assert_eq!(t.target, "e1", "refs ignore case");
        assert_eq!((t.name.as_str(), t.at), ("General", [200.0, 225.0]));
        assert_eq!(lock(&a.device.taps).last(), Some(&(0.5, 225.0 / 800.0)));

        assert!(matches!(a.tap("e3", None), Err(AgentError::Covered(m)) if m.contains("Passcode")));
        assert!(
            matches!(a.tap("e1", Some("Wi-Fi")), Err(AgentError::Stale(m)) if m.contains("\"General\""))
        );
        assert!(matches!(a.tap("e99", None), Err(AgentError::NotFound(_))));
        assert!(matches!(a.tap("hello", None), Err(AgentError::NotFound(_))));

        // A small shift is fine; a large one is refused.
        let mut t = settings();
        t.children[1].children[0].frame[1] = 205.0;
        a.device.set(t.clone());
        assert!(a.tap("e1", None).is_ok());
        t.children[1].children[0].frame[1] = 260.0;
        a.device.set(t.clone());
        assert!(
            matches!(a.tap("e1", None), Err(AgentError::Stale(m)) if m.contains("moved 60 pt"))
        );
        // Gone.
        t.children[1].children.remove(0);
        a.device.set(t);
        assert!(
            matches!(a.tap("e1", None), Err(AgentError::Stale(m)) if m.contains("no longer on screen"))
        );
        let taps = lock(&a.device.taps).len();
        assert_eq!(taps, 2, "refused taps never touch the device");
    }

    fn login() -> AxNode {
        node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![
                node("AXStaticText", "Email", [20.0, 80.0, 360.0, 20.0], vec![]),
                node("AXTextField", "Email", [20.0, 100.0, 360.0, 44.0], vec![]),
                node(
                    "AXSecureTextField",
                    "Password",
                    [20.0, 160.0, 360.0, 44.0],
                    vec![],
                ),
                node("AXButton", "Log in", [20.0, 240.0, 360.0, 50.0], vec![]),
            ],
        )
    }

    fn welcome() -> AxNode {
        node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![node(
                "AXStaticText",
                "Welcome back, Ada",
                [20.0, 100.0, 360.0, 30.0],
                vec![],
            )],
        )
    }

    #[test]
    fn steps_run_a_login_flow_in_one_call() {
        let a = Agent::new(Fake::new(login()));
        *lock(&a.device.on_tap) = Some(Box::new(|f: &Fake, _x, y| {
            if (y * 800.0 - 265.0).abs() < 1.0 {
                f.set(welcome());
            }
        }));
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"action":"fill","target":"Email","value":"ada@example.com"},
                {"action":"fill","target":"password","value":"hunter2","secret":true},
                {"action":"tap","target":"Log in"},
                {"action":"waitFor","text":"welcome back","timeoutMs":1000}]"#,
        )
        .unwrap();
        let r = a.steps(&steps);
        assert!(r.ok, "{:?}", r.failed);
        assert_eq!(r.completed, 4);
        assert_eq!(*lock(&a.device.typed), ["ada@example.com", "hunter2"]);
        assert_eq!(r.verified, [true, true]);
        // "Email" matched the text field, not the static label above it.
        assert_eq!(lock(&a.device.taps)[0], (0.5, 122.0 / 800.0));
        let s = r.snapshot.unwrap();
        assert_eq!(s.mode, Mode::Grid, "the welcome screen has nothing to tap");
    }

    #[test]
    fn secret_fill_is_redacted_from_step_and_snapshot_reports() {
        let a = Agent::new(Fake::new(login()));
        a.snapshot(&quick()).unwrap();
        let r = a.steps(&[Step::Fill {
            target: "Password".into(),
            value: "never-echo-this".into(),
            secret: true,
        }]);
        assert!(r.ok, "{:?}", r.failed);
        assert_eq!(r.verified, [true]);
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("never-echo-this"),
            "secret leaked in report: {json}"
        );
    }

    #[test]
    fn keyboard_dismisses_and_scrolls_above_keyboard() {
        let mut screen = login();
        screen.children.push(node(
            "AXKeyboard",
            "Keyboard",
            [0.0, 500.0, 400.0, 300.0],
            vec![],
        ));
        let a = Agent::new(Fake::new(screen));
        *lock(&a.device.on_tap) = Some(Box::new(|f: &Fake, _x, _y| f.set(login())));
        let r = a.steps(&[
            Step::Scroll {
                direction: "down".into(),
                target: None,
            },
            Step::KeyboardDismiss,
        ]);
        assert!(r.ok, "{:?}", r.failed);
        let swipe = lock(&a.device.swipes)[0];
        assert!(
            swipe.0.1 * 800.0 < 500.0 && swipe.1.1 * 800.0 < 500.0,
            "scroll entered keyboard: {swipe:?}"
        );
        assert!(keyboard_frame(&a.device.accessibility().unwrap(), (400.0, 800.0)).is_none());
    }

    #[test]
    fn screenshot_element_returns_a_cropped_png() {
        let a = Agent::new(Fake::new(login()));
        *lock(&a.device.frame) = Some(Frame {
            width: 400,
            height: 800,
            source_width: 400,
            source_height: 800,
            seq: 1,
            data: vec![255; 400 * 800 * 4],
        });
        let r = a.steps(&[Step::ScreenshotElement {
            target: "Email".into(),
        }]);
        assert!(r.ok, "{:?}", r.failed);
        assert_eq!(r.screenshots.len(), 1);
        assert_eq!(&r.screenshots[0].png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn alert_dismiss_refuses_unknown_buttons() {
        let screen = node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![node(
                "AXAlert",
                "",
                [20.0, 300.0, 360.0, 180.0],
                vec![
                    node(
                        "AXStaticText",
                        "Unrecognized prompt",
                        [40.0, 320.0, 320.0, 30.0],
                        vec![],
                    ),
                    node(
                        "AXButton",
                        "Maybe later",
                        [40.0, 400.0, 320.0, 44.0],
                        vec![],
                    ),
                ],
            )],
        );
        let a = Agent::new(Fake::new(screen));
        let r = a.steps(&[Step::Alert {
            op: "dismiss".into(),
            ms: None,
        }]);
        assert!(!r.ok);
        assert_eq!(r.failed.unwrap().code, "not-found");
        assert!(
            lock(&a.device.taps).is_empty(),
            "unknown alert buttons must never be tapped"
        );
    }

    #[test]
    fn a_hit_tested_prompt_reads_without_the_status_bar() {
        // What sdax returns when a system prompt covers an app's keyboard: SpringBoard's
        // status bar, plus the prompt's container found by hit-testing.
        let screen = node(
            "AXApplication",
            " ",
            [0.0, 0.0, 400.0, 800.0],
            vec![
                node("AXStaticText", "13:38", [30.0, 14.0, 60.0, 20.0], vec![]),
                node(
                    "AXGenericElement",
                    "",
                    [40.0, 300.0, 320.0, 260.0],
                    vec![
                        node(
                            "AXStaticText",
                            "Allow “Safari” to use your location?",
                            [60.0, 320.0, 280.0, 40.0],
                            vec![],
                        ),
                        node(
                            "AXStaticText",
                            "Websites you visit may request your location.",
                            [60.0, 360.0, 280.0, 40.0],
                            vec![],
                        ),
                        node("AXButton", "Allow Once", [60.0, 410.0, 280.0, 44.0], vec![]),
                        node(
                            "AXButton",
                            "Allow While Using App",
                            [60.0, 460.0, 280.0, 44.0],
                            vec![],
                        ),
                        node(
                            "AXButton",
                            "Don’t Allow",
                            [60.0, 510.0, 280.0, 44.0],
                            vec![],
                        ),
                    ],
                ),
            ],
        );
        let info = alert_info(&screen).expect("a prompt");
        assert_eq!(
            info.title.as_deref(),
            Some("Allow “Safari” to use your location?")
        );
        assert_eq!(
            info.message.as_deref(),
            Some("Websites you visit may request your location.")
        );
        assert_eq!(info.buttons.len(), 3);
        let a = Agent::new(Fake::new(screen));
        let r = a.steps(&[Step::Alert {
            op: "dismiss".into(),
            ms: None,
        }]);
        assert!(r.ok, "{:?}", r.failed);
        assert_eq!(
            lock(&a.device.taps).clone(),
            [(0.5, 532.0 / 800.0)],
            "Don’t Allow"
        );
    }

    #[test]
    fn scroll_drags_the_content_the_other_way() {
        let a = Agent::new(Fake::new(login()));
        let r = a.steps(
            &serde_json::from_str::<Vec<Step>>(
                r#"[{"action":"scroll","direction":"down"},
                {"action":"scroll","direction":"left","target":"Log in"}]"#,
            )
            .unwrap(),
        );
        assert!(r.ok, "{:?}", r.failed);
        let swipes = lock(&a.device.swipes).clone();
        // Down: up through the middle of the screen (y 20%..80% region, half of it).
        assert_eq!(swipes[0], ((0.5, 0.65), (0.5, 0.35)));
        // Left inside "Log in" (x 20..380, y 240..290): rightwards across half its width.
        assert_eq!(
            swipes[1],
            (
                (110.0 / 400.0, 265.0 / 800.0),
                (290.0 / 400.0, 265.0 / 800.0)
            )
        );
        let r = a.steps(&[Step::Scroll {
            direction: "sideways".into(),
            target: None,
        }]);
        assert!(
            r.failed
                .unwrap()
                .error
                .starts_with("invalid: unknown direction")
        );
    }

    #[test]
    fn drag_resolves_both_ends_and_holds_at_the_start() {
        let a = Agent::new(Fake::new(login()));
        a.snapshot(&quick()).unwrap();
        let r = a.steps(
            &serde_json::from_str::<Vec<Step>>(
                r#"[{"action":"drag","from":"Email","to":"Log in"},
                {"action":"drag","from":"e1","to":"C4","ms":1200}]"#,
            )
            .unwrap(),
        );
        assert!(r.ok, "{:?}", r.failed);
        let drags = lock(&a.device.drags).clone();
        assert_eq!(
            drags,
            [
                (
                    ((0.5, 122.0 / 800.0), (0.5, 265.0 / 800.0)),
                    Duration::from_millis(800),
                    Duration::from_millis(500),
                    Duration::ZERO
                ),
                (
                    (
                        (0.5, 122.0 / 800.0),
                        (400.0 * 3.5 / 6.0 / 400.0, 800.0 * 2.5 / 12.0 / 800.0)
                    ),
                    Duration::from_millis(800),
                    Duration::from_millis(1200),
                    Duration::ZERO
                )
            ]
        );
        // A covered start is refused; nothing drags.
        let a2 = Agent::new(Fake::new(settings()));
        a2.snapshot(&quick()).unwrap();
        let r = a2.steps(&[Step::Drag {
            from: "e3".into(),
            to: "General".into(),
            source_hold_ms: None,
            move_ms: None,
            destination_hold_ms: None,
        }]);
        assert!(r.failed.unwrap().error.starts_with("covered: e3"));
        assert!(lock(&a2.device.drags).is_empty());
    }

    #[test]
    fn pinch_and_rotate_use_two_fingers_around_the_resolved_center() {
        let a = Agent::new(Fake::new(settings()));
        a.snapshot(&quick()).unwrap();
        let r = a.steps(
            &serde_json::from_str::<Vec<Step>>(
                r#"[{"action":"pinch","scale":2.0},
                {"action":"rotate","target":"General","degrees":90}]"#,
            )
            .unwrap(),
        );
        assert!(r.ok, "{:?}", r.failed);
        let gestures = lock(&a.device.touch2).clone();
        let phases = |from: usize| {
            gestures[from..]
                .iter()
                .position(|g| g.0 == simdriver::Phase::Up)
                .unwrap()
                + from
        };
        let pinch_up = phases(0);
        let rotate_up = phases(pinch_up + 1);
        assert_eq!(rotate_up, gestures.len() - 1);
        for (down, up) in [(0, pinch_up), (pinch_up + 1, rotate_up)] {
            assert_eq!(gestures[down].0, simdriver::Phase::Down);
            assert!(up - down > 3, "a stroke of several moves, not one jump");
            assert!(
                gestures[down + 1..up]
                    .iter()
                    .all(|g| g.0 == simdriver::Phase::Move)
            );
        }
        // The pinch opens 50 pt → 100 pt either side of the centre.
        assert_eq!(gestures[0].1, (0.375, 0.5));
        assert_eq!(gestures[0].2, (0.625, 0.5));
        assert_eq!(gestures[pinch_up].1, (0.25, 0.5));
        assert_eq!(gestures[pinch_up].2, (0.75, 0.5));
        // The rotation ends a quarter turn round: fingers stacked vertically.
        let (a1, a2) = (gestures[rotate_up].1, gestures[rotate_up].2);
        assert!((a1.0 - a2.0).abs() < 1e-9 && a1.1 < a2.1, "{a1:?} {a2:?}");
    }

    #[test]
    fn long_press_holds_the_resolved_target() {
        let a = Agent::new(Fake::new(settings()));
        a.snapshot(&quick()).unwrap();
        let r = a.steps(
            &serde_json::from_str::<Vec<Step>>(
                r#"[{"action":"longPress","target":"General"},
                {"action":"longPress","target":"Search","ms":300},
                {"action":"longPress","target":"C4","ms":1500}]"#,
            )
            .unwrap(),
        );
        assert!(r.ok, "{:?}", r.failed);
        let held = lock(&a.device.long_presses).clone();
        assert_eq!(
            held,
            [
                (0.5, 225.0 / 800.0, Duration::from_millis(800)),
                (0.5, 762.5 / 800.0, Duration::from_millis(300)),
                (
                    400.0 * 3.5 / 6.0 / 400.0,
                    800.0 * 2.5 / 12.0 / 800.0,
                    Duration::from_millis(1500)
                )
            ]
        );
        assert!(lock(&a.device.taps).is_empty(), "long presses never tap");
        // Covered and unknown targets refuse without touching.
        let r = a.steps(&[Step::LongPress {
            target: "e3".into(),
            ms: Some(10),
        }]);
        assert_eq!(
            r.failed.unwrap().error,
            "covered: e3 \"Passcode\" is covered by another element"
        );
    }

    #[test]
    fn double_tap_uses_a_validated_element_or_grid_cell() {
        let a = Agent::new(Fake::new(settings()));
        a.snapshot(&quick()).unwrap();
        let r = a.steps(&serde_json::from_str::<Vec<Step>>(
            r#"[{"action":"doubleTap","target":"General"},{"action":"doubleTap","target":"C4"}]"#,
        ).unwrap());
        assert!(r.ok, "{:?}", r.failed);
        let got = lock(&a.device.double_taps).clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (0.5, 225.0 / 800.0));
        assert!((got[1].0 - 3.5 / 6.0).abs() < f64::EPSILON);
        assert!((got[1].1 - 2.5 / 12.0).abs() < f64::EPSILON);
        assert!(lock(&a.device.taps).is_empty());
    }

    #[test]
    fn steps_stop_at_the_first_failure_and_show_the_screen() {
        let a = Agent::new(Fake::new(login()));
        let steps = vec![
            Step::Tap {
                target: "Log in".into(),
            },
            Step::Tap {
                target: "Sign up".into(),
            },
            Step::Tap {
                target: "Log in".into(),
            },
        ];
        let r = a.steps(&steps);
        assert!(!r.ok);
        assert_eq!(r.completed, 1);
        let f = r.failed.unwrap();
        assert_eq!(f.index, 1);
        assert_eq!(
            f.error,
            "not-found: nothing on screen is called \"Sign up\""
        );
        assert!(r.snapshot.unwrap().text.contains("e3 button \"Log in\""));
        assert_eq!(
            lock(&a.device.taps).len(),
            1,
            "nothing after the failure ran"
        );

        let r = a.steps(&[
            Step::Key {
                name: "nope".into(),
            },
            Step::Press {
                button: "home".into(),
            },
        ]);
        assert_eq!(r.failed.unwrap().error, "invalid: unknown key \"nope\"");
        let r = a.steps(&[Step::WaitFor {
            text: "never".into(),
            timeout_ms: Some(200),
        }]);
        assert!(
            r.failed
                .unwrap()
                .error
                .starts_with("not-found: \"never\" did not appear within 0.2 s")
        );
    }

    #[test]
    fn duplicate_labels_are_ambiguous_until_a_ref_picks_one() {
        let dupes = node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![
                node("AXButton", "Edit", [20.0, 200.0, 360.0, 50.0], vec![]),
                node("AXButton", "Edit", [20.0, 300.0, 360.0, 50.0], vec![]),
            ],
        );
        let a = Agent::new(Fake::new(dupes));
        a.snapshot(&quick()).unwrap();
        match a.tap("Edit", None) {
            Err(AgentError::Ambiguous(m)) => {
                assert!(m.contains("\"Edit\" matches 2 elements"), "{m}")
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
        assert!(lock(&a.device.taps).is_empty(), "nothing tapped");
        // A ref disambiguates.
        let t = a.tap("e2", None).unwrap();
        assert_eq!(t.at, [200.0, 325.0]);
        // Exact still beats partial; reachable still beats covered.
        let varied = node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![
                node("AXButton", "Edit photo", [20.0, 200.0, 360.0, 50.0], vec![]),
                node("AXButton", "Edit", [20.0, 300.0, 360.0, 50.0], vec![]),
            ],
        );
        let b = Agent::new(Fake::new(varied));
        assert_eq!(b.tap("Edit", None).unwrap().at, [200.0, 325.0]);
    }
}
