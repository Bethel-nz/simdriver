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
mod steps;

pub use marks::{Element, Visibility};
pub use steps::{Step, StepFailure};

use serde::Serialize;
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
    /// A tap at (x, y) in 0..1 of the screen.
    fn tap(&self, x: f64, y: f64);
    /// One finger from `from` to `to` (0..1 of the screen) over `duration`.
    fn swipe(&self, from: (f64, f64), to: (f64, f64), duration: Duration);
    fn text(&self, s: &str);
    fn key(&self, name: &str) -> bool;
    fn button(&self, b: Button);
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
    fn tap(&self, x: f64, y: f64) {
        Driver::tap(self, x, y)
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
    fn tap(&self, x: f64, y: f64) {
        (**self).tap(x, y)
    }
    fn swipe(&self, from: (f64, f64), to: (f64, f64), duration: Duration) {
        (**self).swipe(from, to, duration)
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
    /// No snapshot yet, or input not connected.
    NotReady(String),
    /// A malformed step (unknown key or button).
    Invalid(String),
    Device(simdriver::Error),
}

impl AgentError {
    /// A stable short code: `stale`, `covered`, `not-found`, `not-ready`, `invalid`, `device`.
    pub fn code(&self) -> &'static str {
        match self {
            AgentError::Stale(_) => "stale",
            AgentError::Covered(_) => "covered",
            AgentError::NotFound(_) => "not-found",
            AgentError::NotReady(_) => "not-ready",
            AgentError::Invalid(_) => "invalid",
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
            | AgentError::NotReady(m)
            | AgentError::Invalid(m) => write!(f, "{}: {m}", self.code()),
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
        SnapshotOptions { settle: SETTLE_DEFAULT, image: true, grid: false, cols: Grid::DEFAULT_COLS }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Elements e1…eN are the targets (grid cells work too).
    Elements,
    /// No accessible elements: grid cells are the targets.
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GridSize {
    pub rows: u32,
    pub cols: u32,
}

#[derive(Debug, Clone, Serialize)]
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
    /// The screen after the run (or after the failure), without an image.
    pub snapshot: Option<Snapshot>,
}

pub struct Agent<D: Device> {
    device: D,
    last: Mutex<Option<Snapshot>>,
    next_id: AtomicU64,
    refs: Mutex<Refs>,
    /// The screen as it was when the agent last acted, until accessibility shows a change.
    acted: Mutex<Option<Mark>>,
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
    let (w, h) = (f.width as usize, f.height as usize);
    let (cols, rows) = PRINT;
    let mut out = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        for c in 0..cols {
            let (mut sum, mut n) = ([0u64; 3], 0u64);
            for y in (r * h / rows..(r + 1) * h / rows).step_by(4) {
                for x in (c * w / cols..(c + 1) * w / cols).step_by(4) {
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
#[derive(Default)]
struct Refs {
    count: u64,
    issued: HashMap<String, (String, u64)>,
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
    els.iter().map(|e| (e.role.clone(), e.label.clone(), e.frame.map(|v| v.round() as i64))).collect()
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
        }
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    /// The latest snapshot, if any.
    pub fn last_snapshot(&self) -> Option<Snapshot> {
        lock(&self.last).clone()
    }

    fn screen(&self) -> Result<(f64, f64), AgentError> {
        self.device.screen_points().ok_or_else(|| AgentError::NotReady("input is not connected yet".into()))
    }

    /// Read until the element list stops changing (or `settle` runs out).
    fn settled(&self, screen: (f64, f64), settle: Duration) -> Result<(AxNode, Vec<Element>, usize, bool), AgentError> {
        let t0 = Instant::now();
        let mut prev: Option<(Signature, Instant)> = None;
        loop {
            let root = self.device.accessibility()?;
            let (els, off) = marks::elements(&root, screen);
            let sig = signature(&els);
            let now = Instant::now();
            // Mid-transition (an app launching or closing), the frontmost element is an
            // unnamed SpringBoard with only the status bar: never a screen to settle on.
            let between = els.is_empty() && root.label.as_deref().is_none_or(|l| l.trim().is_empty());
            let lagging = self.tree_lags(&root, &els);
            let since = match prev {
                Some((p, since)) if p == sig && !between && !lagging => since,
                _ => now,
            };
            let need = if els.is_empty() { EMPTY_SETTLE } else { Duration::ZERO };
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
        let Some(mark) = acted.as_ref() else { return false };
        if tree_key(root, els) != mark.tree || mark.at.elapsed() > LAG_MAX {
            *acted = None; // caught up, or never will
            return false;
        }
        if mark.switches {
            return true;
        }
        let Some(before) = mark.print.as_ref() else { return false };
        self.device.capture(Duration::from_millis(200)).is_some_and(|f| changed(before, &fingerprint(&f)) > LAG_CHANGE)
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
        let print = self.device.capture(Duration::from_millis(200)).map(|f| fingerprint(&f));
        // Home on the home screen (SpringBoard's root is unnamed) switches nothing.
        let switches = switches && root.label.as_deref().is_some_and(|l| !l.trim().is_empty());
        *lock(&self.acted) = Some(Mark { tree: tree_key(&root, &els), print, at: Instant::now(), switches });
    }

    /// Wait for the screen to settle, list its elements (or grid), and draw them.
    pub fn snapshot(&self, opts: &SnapshotOptions) -> Result<Snapshot, AgentError> {
        let screen = self.screen()?;
        let (_, mut elements, offscreen, settled) = self.settled(screen, opts.settle)?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.assign_refs(&mut elements, id);
        let grid = Grid::new(screen, opts.cols);
        let mode = if opts.grid || elements.is_empty() { Mode::Grid } else { Mode::Elements };
        let image = if opts.image {
            self.device.capture(CAPTURE_WAIT).map(|f| match mode {
                Mode::Elements => draw::elements(&f, screen, &elements),
                Mode::Grid => draw::grid(&f, &grid),
            })
        } else {
            None
        };
        let size = GridSize { rows: grid.rows, cols: grid.cols };
        let text = describe(id, mode, screen, &elements, offscreen, &grid, settled);
        let snap = Snapshot { id, mode, screen: [screen.0, screen.1], elements, offscreen, grid: size, settled, text, image };
        *lock(&self.last) = Some(Snapshot { image: None, ..snap.clone() });
        Ok(snap)
    }

    /// Elements that were in the last snapshot keep their refs (the nearest same-named
    /// one, for repeats like two "Edit" buttons); new ones get the next unused number.
    fn assign_refs(&self, els: &mut [Element], snapshot: u64) {
        let prev = lock(&self.last).as_ref().map(|s| s.elements.clone()).unwrap_or_default();
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
            refs.issued.insert(e.reference.clone(), (e.name().to_string(), snapshot));
        }
    }

    /// Tap an element ref from the last snapshot (`e4`) or a grid cell (`C4`). A ref is
    /// re-read just before the touch: refused if it vanished or moved, is covered, or —
    /// with `expect` — its name no longer contains that text.
    pub fn tap(&self, target: &str, expect: Option<&str>) -> Result<Tapped, AgentError> {
        let screen = self.screen()?;
        if is_ref(target) {
            return self.tap_ref(&target.to_ascii_lowercase(), expect, screen);
        }
        let cols = lock(&self.last).as_ref().map_or(Grid::DEFAULT_COLS, |s| s.grid.cols);
        match Grid::new(screen, cols).center(target) {
            Some((x, y)) => {
                self.mark(None, screen, false);
                self.device.tap(x, y);
                Ok(Tapped { target: target.to_string(), name: String::new(), at: [x * screen.0, y * screen.1] })
            }
            None => Err(AgentError::NotFound(format!(
                "{target:?} is neither an element ref (e4) nor a grid cell (C4)"
            ))),
        }
    }

    fn tap_ref(&self, target: &str, expect: Option<&str>, screen: (f64, f64)) -> Result<Tapped, AgentError> {
        let last = lock(&self.last)
            .clone()
            .ok_or_else(|| AgentError::NotReady("no snapshot yet: take one, then use its refs".into()))?;
        let Some(el) = last.elements.iter().find(|e| e.reference == target) else {
            return Err(match lock(&self.refs).issued.get(target) {
                Some((name, seen)) => AgentError::Stale(format!(
                    "{target} {name:?} (snapshot {seen}) is not on the current screen (snapshot {}); use the current refs",
                    last.id
                )),
                None => AgentError::NotFound(format!("{target} was never issued; take a snapshot and use its refs")),
            });
        };
        let root = self.device.accessibility()?;
        let (fresh, _) = marks::elements(&root, screen);
        let now = fresh
            .iter()
            .filter(|f| f.same_identity(el))
            .min_by(|a, b| a.moved_by(el).total_cmp(&b.moved_by(el)))
            .ok_or_else(|| {
                AgentError::Stale(format!("{target} {:?} is no longer on screen; take a new snapshot", el.name()))
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
        self.touch(target, &now, root, screen)
    }

    fn touch(&self, target: &str, el: &Element, root: AxNode, screen: (f64, f64)) -> Result<Tapped, AgentError> {
        let p = el.tap.ok_or_else(|| {
            AgentError::Covered(format!("{target} {:?} is covered by another element", el.name()))
        })?;
        self.mark(Some(root), screen, false);
        self.device.tap(p[0] / screen.0, p[1] / screen.1);
        Ok(Tapped { target: target.to_string(), name: el.name().to_string(), at: p })
    }

    /// Tap a ref, a label on the current (settled) screen, or a grid cell.
    fn act_on(&self, target: &str, text_input: bool) -> Result<Tapped, AgentError> {
        if is_ref(target) {
            return self.tap(target, None);
        }
        let screen = self.screen()?;
        let (root, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
        match self.find(&els, target, text_input) {
            Some(el) => self.touch(target, &el.clone(), root, screen),
            None if Grid::new(screen, Grid::DEFAULT_COLS).center(target).is_some() => self.tap(target, None),
            None => Err(AgentError::NotFound(format!("nothing on screen is called {target:?}"))),
        }
    }

    /// The element a label, identifier or value names: exact over partial, reachable
    /// first, text fields first for `fill`; of equal matches the first in tree order.
    fn find<'a>(&self, els: &'a [Element], target: &str, text_input: bool) -> Option<&'a Element> {
        let want = target.to_lowercase();
        let rank = |e: &Element| {
            let names = [e.label.as_deref(), e.identifier.as_deref(), e.value.as_deref()];
            let exact = names.iter().flatten().any(|n| n.to_lowercase() == want);
            let partial = names.iter().flatten().any(|n| n.to_lowercase().contains(&want));
            let score = if exact { 2 } else if partial { 1 } else { 0 };
            (score, e.tap.is_some(), !text_input || e.takes_text())
        };
        els.iter().rev().filter(|e| rank(e).0 > 0).max_by_key(|e| rank(e))
    }

    fn run(&self, step: &Step) -> Result<(), AgentError> {
        match step {
            Step::Tap { target } => self.act_on(target, false).map(drop),
            Step::Fill { target, value } => {
                self.act_on(target, true)?;
                thread::sleep(FOCUS_WAIT);
                self.device.text(value);
                Ok(())
            }
            Step::Scroll { direction, target } => {
                let screen = self.screen()?;
                // The region to drag in, in points: the target's visible part, else the
                // middle of the screen (clear of the system edge zones).
                let region = match target {
                    // A ref names an element of the last snapshot (fresh reads number afresh).
                    Some(t) if is_ref(t) => lock(&self.last)
                        .as_ref()
                        .and_then(|s| s.elements.iter().find(|e| e.reference.eq_ignore_ascii_case(t)).map(|e| e.frame))
                        .ok_or_else(|| AgentError::NotFound(format!("{t} is not in the last snapshot")))?,
                    Some(t) => {
                        let (_, els, _, _) = self.settled(screen, SnapshotOptions::default().settle)?;
                        let el = self.find(&els, t, false).ok_or_else(|| {
                            AgentError::NotFound(format!("nothing on screen is called {t:?}"))
                        })?;
                        el.frame
                    }
                    None => [screen.0 * 0.1, screen.1 * 0.2, screen.0 * 0.8, screen.1 * 0.6],
                };
                let (cx, cy) = (region[0] + region[2] / 2.0, region[1] + region[3] / 2.0);
                let (dx, dy) = (region[2] * SCROLL_SHARE / 2.0, region[3] * SCROLL_SHARE / 2.0);
                // Revealing what is below drags the content up, and so on.
                let ((x0, y0), (x1, y1)) = match direction.as_str() {
                    "down" => ((cx, cy + dy), (cx, cy - dy)),
                    "up" => ((cx, cy - dy), (cx, cy + dy)),
                    "right" => ((cx + dx, cy), (cx - dx, cy)),
                    "left" => ((cx - dx, cy), (cx + dx, cy)),
                    other => return Err(AgentError::Invalid(format!("unknown direction {other:?} (up, down, left, right)"))),
                };
                self.mark(None, screen, false);
                let n = |x: f64, y: f64| (x / screen.0, y / screen.1);
                self.device.swipe(n(x0, y0), n(x1, y1), SCROLL_TIME);
                Ok(())
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
                let b = button(name).ok_or_else(|| AgentError::Invalid(format!("unknown button {name:?}")))?;
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
        let mut failed = None;
        let mut completed = 0;
        for (index, step) in steps.iter().enumerate() {
            match self.run(step) {
                Ok(()) => completed += 1,
                Err(e) => {
                    failed = Some(StepFailure { index, step: step.clone(), error: e.to_string() });
                    break;
                }
            }
        }
        let snapshot = self.snapshot(&SnapshotOptions { image: false, ..SnapshotOptions::default() }).ok();
        StepsReport { ok: failed.is_none(), completed, failed, snapshot }
    }
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"))
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
            out.push_str(&format!("snapshot {id} · {} elements · {w}×{h} pt{unsettled}\n", els.len()));
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
            out.push_str(&format!("snapshot {id} · grid {}×{} · {w}×{h} pt{unsettled}\n", grid.cols, grid.rows));
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

    type OnTap = Box<dyn Fn(&Fake, f64, f64) + Send + Sync>;
    type OnButton = Box<dyn Fn(&Fake) + Send + Sync>;
    type Swipe = ((f64, f64), (f64, f64));

    /// A scripted device: `screen` is what accessibility reads; taps and typing are logged,
    /// and `on_tap` can change the screen (navigation).
    struct Fake {
        screen: Mutex<AxNode>,
        taps: Mutex<Vec<(f64, f64)>>,
        typed: Mutex<Vec<String>>,
        swipes: Mutex<Vec<Swipe>>,
        reads: AtomicU64,
        on_tap: Mutex<Option<OnTap>>,
        on_button: Mutex<Option<OnButton>>,
        frame: Mutex<Option<Frame>>,
    }

    impl Fake {
        fn new(screen: AxNode) -> Fake {
            Fake {
                screen: Mutex::new(screen),
                taps: Mutex::default(),
                typed: Mutex::default(),
                swipes: Mutex::default(),
                reads: AtomicU64::new(0),
                on_tap: Mutex::new(None),
                on_button: Mutex::new(None),
                frame: Mutex::new(None),
            }
        }
        fn set(&self, screen: AxNode) {
            *lock(&self.screen) = screen;
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
        fn tap(&self, x: f64, y: f64) {
            lock(&self.taps).push((x, y));
            if let Some(f) = lock(&self.on_tap).as_ref() {
                f(self, x, y);
            }
        }
        fn swipe(&self, from: (f64, f64), to: (f64, f64), _: Duration) {
            lock(&self.swipes).push((from, to));
        }
        fn text(&self, s: &str) {
            lock(&self.typed).push(s.to_string());
        }
        fn key(&self, name: &str) -> bool {
            name == "enter"
        }
        fn button(&self, _: Button) {
            if let Some(f) = lock(&self.on_button).as_ref() {
                f(self);
            }
        }
    }

    fn solid(v: u8) -> Frame {
        Frame { width: 400, height: 800, source_width: 400, source_height: 800, seq: 2, data: vec![v; 400 * 800 * 4] }
    }

    fn quick() -> SnapshotOptions {
        SnapshotOptions { settle: Duration::from_millis(300), ..SnapshotOptions::default() }
    }

    #[test]
    fn snapshot_lists_refs_as_text_and_draws_them() {
        let fake = Fake::new(settings());
        *lock(&fake.frame) = Some(solid(0));
        let a = Agent::new(fake);
        let s = a.snapshot(&quick()).unwrap();
        assert_eq!((s.id, s.mode, s.settled, s.offscreen), (1, Mode::Elements, true, 1));
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
        assert!(a.last_snapshot().unwrap().image.is_none(), "the stored copy drops the image");
        assert_eq!(a.snapshot(&quick()).unwrap().id, 2);
    }

    #[test]
    fn an_empty_screen_becomes_a_grid_once_it_stays_empty() {
        let a = Agent::new(Fake::new(node("AXApplication", "Maps", [0.0, 0.0, 400.0, 800.0], vec![])));
        let t0 = Instant::now();
        let s = a.snapshot(&SnapshotOptions { image: false, ..SnapshotOptions::default() }).unwrap();
        assert!(t0.elapsed() >= EMPTY_SETTLE, "an empty screen waits {EMPTY_SETTLE:?} to be sure");
        assert_eq!((s.mode, s.settled), (Mode::Grid, true));
        assert_eq!(s.grid, GridSize { rows: 12, cols: 6 });
        assert!(s.text.contains("no accessible elements on screen; target grid cells: rows A–M top to bottom (no E)"), "{}", s.text);
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
            vec![node("AXStaticText", "08:34", [56.0, 21.0, 52.0, 22.0], vec![])],
        );
        let a = Agent::new(Fake::new(launching));
        let fake = &a.device;
        let s = thread::scope(|sc| {
            sc.spawn(|| {
                thread::sleep(Duration::from_millis(1500)); // longer than EMPTY_SETTLE
                fake.set(settings());
            });
            a.snapshot(&SnapshotOptions { image: false, ..SnapshotOptions::default() }).unwrap()
        });
        assert_eq!((s.mode, s.settled), (Mode::Elements, true), "{}", s.text);
    }

    #[test]
    fn refs_stay_with_their_elements_and_are_never_reused() {
        let a = Agent::new(Fake::new(login()));
        let s1 = a.snapshot(&quick()).unwrap();
        let refs = |s: &Snapshot| s.elements.iter().map(|e| (e.reference.clone(), e.name().to_string())).collect::<Vec<_>>();
        assert_eq!(refs(&s1), [("e1".into(), "Email".into()), ("e2".into(), "Password".into()), ("e3".into(), "Log in".into())]);

        // A banner appears above the form and pushes it down: the form keeps its refs, the
        // banner gets the next number.
        let mut t = login();
        for c in &mut t.children {
            c.frame[1] += 60.0;
        }
        t.children.insert(0, node("AXButton", "Dismiss", [20.0, 40.0, 360.0, 40.0], vec![]));
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
            Err(AgentError::Stale(m)) => assert!(m.contains("\"Log in\" (snapshot 2) is not on the current screen (snapshot 3)"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(a.tap("e42", None), Err(AgentError::NotFound(_))));
        assert!(lock(&a.device.taps).is_empty());
    }

    /// Run `steps` while, once `trigger` holds, the tree switches to `then` after 600 ms;
    /// returns how long the run took.
    fn lagged(a: &Agent<Fake>, steps: &[Step], trigger: impl Fn(&Fake) -> bool + Sync, then: AxNode) -> Duration {
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
        let took = lagged(&a, &[Step::Press { button: "home".into() }], pressed, welcome());
        assert!(took >= Duration::from_millis(600), "settled on the stale tree after {took:?}");
        assert!(lock(&a.acted).is_none(), "the mark clears once the tree catches up");

        // A tap that changes the screen a lot (an app opening): same, from the pixels.
        a.device.set(login());
        *lock(&a.device.frame) = Some(solid(30));
        *lock(&a.device.on_tap) = Some(Box::new(|f: &Fake, _, _| *lock(&f.frame) = Some(solid(200))));
        let took = lagged(
            &a,
            &[Step::Tap { target: "Log in".into() }],
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
        assert!(a.steps(&[Step::Tap { target: "Log in".into() }]).ok);
        assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
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
        assert!(matches!(a.tap("e1", None), Err(AgentError::NotReady(_))), "no snapshot yet");
        a.snapshot(&quick()).unwrap();

        let t = a.tap("E1", Some("general")).unwrap();
        assert_eq!(t.target, "e1", "refs ignore case");
        assert_eq!((t.name.as_str(), t.at), ("General", [200.0, 225.0]));
        assert_eq!(lock(&a.device.taps).last(), Some(&(0.5, 225.0 / 800.0)));

        assert!(matches!(a.tap("e3", None), Err(AgentError::Covered(m)) if m.contains("Passcode")));
        assert!(matches!(a.tap("e1", Some("Wi-Fi")), Err(AgentError::Stale(m)) if m.contains("\"General\"")));
        assert!(matches!(a.tap("e99", None), Err(AgentError::NotFound(_))));
        assert!(matches!(a.tap("hello", None), Err(AgentError::NotFound(_))));

        // A small shift is fine; a large one is refused.
        let mut t = settings();
        t.children[1].children[0].frame[1] = 205.0;
        a.device.set(t.clone());
        assert!(a.tap("e1", None).is_ok());
        t.children[1].children[0].frame[1] = 260.0;
        a.device.set(t.clone());
        assert!(matches!(a.tap("e1", None), Err(AgentError::Stale(m)) if m.contains("moved 60 pt")));
        // Gone.
        t.children[1].children.remove(0);
        a.device.set(t);
        assert!(matches!(a.tap("e1", None), Err(AgentError::Stale(m)) if m.contains("no longer on screen")));
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
                node("AXSecureTextField", "Password", [20.0, 160.0, 360.0, 44.0], vec![]),
                node("AXButton", "Log in", [20.0, 240.0, 360.0, 50.0], vec![]),
            ],
        )
    }

    fn welcome() -> AxNode {
        node(
            "AXApplication",
            "App",
            [0.0, 0.0, 400.0, 800.0],
            vec![node("AXStaticText", "Welcome back, Ada", [20.0, 100.0, 360.0, 30.0], vec![])],
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
                {"action":"fill","target":"password","value":"hunter2"},
                {"action":"tap","target":"Log in"},
                {"action":"waitFor","text":"welcome back","timeoutMs":1000}]"#,
        )
        .unwrap();
        let r = a.steps(&steps);
        assert!(r.ok, "{:?}", r.failed);
        assert_eq!(r.completed, 4);
        assert_eq!(*lock(&a.device.typed), ["ada@example.com", "hunter2"]);
        // "Email" matched the text field, not the static label above it.
        assert_eq!(lock(&a.device.taps)[0], (0.5, 122.0 / 800.0));
        let s = r.snapshot.unwrap();
        assert_eq!(s.mode, Mode::Grid, "the welcome screen has nothing to tap");
    }

    #[test]
    fn scroll_drags_the_content_the_other_way() {
        let a = Agent::new(Fake::new(login()));
        let r = a.steps(&serde_json::from_str::<Vec<Step>>(
            r#"[{"action":"scroll","direction":"down"},
                {"action":"scroll","direction":"left","target":"Log in"}]"#,
        )
        .unwrap());
        assert!(r.ok, "{:?}", r.failed);
        let swipes = lock(&a.device.swipes).clone();
        // Down: up through the middle of the screen (y 20%..80% region, half of it).
        assert_eq!(swipes[0], ((0.5, 0.65), (0.5, 0.35)));
        // Left inside "Log in" (x 20..380, y 240..290): rightwards across half its width.
        assert_eq!(swipes[1], ((110.0 / 400.0, 265.0 / 800.0), (290.0 / 400.0, 265.0 / 800.0)));
        let r = a.steps(&[Step::Scroll { direction: "sideways".into(), target: None }]);
        assert!(r.failed.unwrap().error.starts_with("invalid: unknown direction"));
    }

    #[test]
    fn steps_stop_at_the_first_failure_and_show_the_screen() {
        let a = Agent::new(Fake::new(login()));
        let steps = vec![
            Step::Tap { target: "Log in".into() },
            Step::Tap { target: "Sign up".into() },
            Step::Tap { target: "Log in".into() },
        ];
        let r = a.steps(&steps);
        assert!(!r.ok);
        assert_eq!(r.completed, 1);
        let f = r.failed.unwrap();
        assert_eq!(f.index, 1);
        assert_eq!(f.error, "not-found: nothing on screen is called \"Sign up\"");
        assert!(r.snapshot.unwrap().text.contains("e3 button \"Log in\""));
        assert_eq!(lock(&a.device.taps).len(), 1, "nothing after the failure ran");

        let r = a.steps(&[Step::Key { name: "nope".into() }, Step::Press { button: "home".into() }]);
        assert_eq!(r.failed.unwrap().error, "invalid: unknown key \"nope\"");
        let r = a.steps(&[Step::WaitFor { text: "never".into(), timeout_ms: Some(200) }]);
        assert!(r.failed.unwrap().error.starts_with("not-found: \"never\" did not appear within 0.2 s"));
    }
}
