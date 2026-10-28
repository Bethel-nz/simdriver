//! simdriver Node addon (napi 3): the plan's Task 11 surface over the Rust `Driver`.
//!
//! Events reach JS through a pump, not one TSFN call per event:
//! - frames park in a single slot — **latest wins**; at most one pump call is in flight,
//!   so a slow JS host never queues stale frames;
//! - states/notes go to a small FIFO — they are rare and are never dropped;
//! - the parked frame is converted to a JS Buffer **on the JS thread** inside
//!   `take_event`, so nothing env-bound crosses threads.
//!
//! `stop()` stops the Rust driver and releases the ThreadsafeFunction, so a stopped
//! driver does not keep the event loop alive.
use napi::Env;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use simdriver_agent as agent;
use simdriver_core as sd;

#[macro_use]
extern crate napi_derive;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn to_napi(e: sd::Error) -> Error {
    Error::new(Status::GenericFailure, e.to_string())
}

#[napi(object)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub state: String,
    pub device_type: String,
    pub runtime: String,
}

#[napi(object)]
pub struct App {
    pub bundle_id: String,
    pub name: String,
    pub app_type: String,
}

/// `Driver.start` off the JS thread: `sd::Driver::start` blocks until live.
pub struct StartTask {
    opts: Option<sd::Options>,
    pump: Arc<Pump>,
}

impl Task for StartTask {
    type Output = sd::Driver;
    type JsValue = Driver;

    fn compute(&mut self) -> Result<sd::Driver> {
        let opts = self
            .opts
            .take()
            .ok_or_else(|| Error::from_reason("start ran twice"))?;
        let p = self.pump.clone();
        sd::Driver::start(opts, move |ev| p.on_event(ev)).map_err(to_napi)
    }

    fn resolve(&mut self, _env: Env, core: sd::Driver) -> Result<Driver> {
        Ok(Driver {
            core: Arc::new(core),
            pump: self.pump.clone(),
        })
    }
}

/// `driver.accessibility()` off the JS thread: a snapshot blocks (1–2 s the first time).
/// Resolves to the tree as JSON; index.js parses it.
pub struct AxTask {
    core: Arc<sd::Driver>,
}

impl Task for AxTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<String> {
        let root = self.core.accessibility().map_err(to_napi)?;
        serde_json::to_string(&root).map_err(|e| Error::from_reason(e.to_string()))
    }

    fn resolve(&mut self, _env: Env, json: String) -> Result<String> {
        Ok(json)
    }
}

/// Any blocking driver call (a `simctl` round trip, a held touch) off the JS thread, so
/// frames keep flowing while it runs.
pub struct Blocking<T: Send + 'static> {
    f: Option<Box<dyn FnOnce() -> std::result::Result<T, sd::Error> + Send>>,
}

impl<T: Send + ToNapiValue + TypeName + 'static> Task for Blocking<T> {
    type Output = T;
    type JsValue = T;

    fn compute(&mut self) -> Result<T> {
        let f = self
            .f
            .take()
            .ok_or_else(|| Error::from_reason("task already ran"))?;
        f().map_err(to_napi)
    }

    fn resolve(&mut self, _env: Env, v: T) -> Result<T> {
        Ok(v)
    }
}

fn blocking<T: Send + ToNapiValue + TypeName + 'static>(
    f: impl FnOnce() -> std::result::Result<T, sd::Error> + Send + 'static,
) -> AsyncTask<Blocking<T>> {
    AsyncTask::new(Blocking {
        f: Some(Box::new(f)),
    })
}

fn ms(v: f64) -> std::time::Duration {
    std::time::Duration::from_secs_f64((v / 1000.0).max(0.0))
}

fn to_app(app: sd::App) -> App {
    App {
        bundle_id: app.bundle_id,
        name: app.name,
        app_type: app.app_type,
    }
}

type CoreAgent = agent::Agent<Arc<sd::Driver>>;

fn agent_err(e: agent::AgentError) -> Error {
    Error::new(Status::GenericFailure, e.to_string())
}

#[napi(object)]
pub struct JsSnapshot {
    /// The snapshot without its image, as JSON (index.js parses it).
    pub json: String,
    pub image: Option<Buffer>,
}

pub struct SnapshotTask {
    agent: Arc<CoreAgent>,
    opts: agent::SnapshotOptions,
}

impl Task for SnapshotTask {
    type Output = agent::Snapshot;
    type JsValue = JsSnapshot;

    fn compute(&mut self) -> Result<agent::Snapshot> {
        self.agent.snapshot(&self.opts).map_err(agent_err)
    }

    fn resolve(&mut self, _env: Env, s: agent::Snapshot) -> Result<JsSnapshot> {
        let json = serde_json::to_string(&s).map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(JsSnapshot {
            json,
            image: s.image.map(Buffer::from),
        })
    }
}

pub struct TapTask {
    agent: Arc<CoreAgent>,
    target: String,
    expect: Option<String>,
}

impl Task for TapTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<String> {
        let t = self
            .agent
            .tap(&self.target, self.expect.as_deref())
            .map_err(agent_err)?;
        serde_json::to_string(&t).map_err(|e| Error::from_reason(e.to_string()))
    }

    fn resolve(&mut self, _env: Env, json: String) -> Result<String> {
        Ok(json)
    }
}

/// A steps run: the report as JSON (PNGs skipped) plus a recording's images as Buffers,
/// so a recording's PNGs never travel as JSON number arrays.
#[napi(object)]
pub struct JsStepsResult {
    pub report: String,
    /// How many leading `images` are contact sheets.
    pub sheets: u32,
    /// The record's contact sheets, then its frames in index order (empty without a record).
    pub images: Vec<Buffer>,
}

pub struct StepsTask {
    agent: Arc<CoreAgent>,
    steps: Vec<agent::Step>,
}

impl Task for StepsTask {
    type Output = (String, u32, Vec<Vec<u8>>);
    type JsValue = JsStepsResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let mut r = self.agent.steps(&self.steps);
        let json = serde_json::to_string(&r).map_err(|e| Error::from_reason(e.to_string()))?;
        let mut images = Vec::new();
        let mut sheets = 0;
        if let Some(rec) = r.record.as_mut() {
            sheets = rec.sheets.len() as u32;
            images.append(&mut rec.sheets);
            images.extend(rec.frames.iter_mut().map(|f| std::mem::take(&mut f.png)));
        }
        Ok((json, sheets, images))
    }

    fn resolve(&mut self, _env: Env, (report, sheets, images): Self::Output) -> Result<JsStepsResult> {
        Ok(JsStepsResult {
            report,
            sheets,
            images: images.into_iter().map(Buffer::from).collect(),
        })
    }
}

/// The agent layer over one driver: element refs, annotated screenshots, validated taps,
/// steps. Every call runs off the JS thread.
#[napi(js_name = "Agent")]
pub struct JsAgent {
    inner: Arc<CoreAgent>,
}

#[napi]
impl JsAgent {
    #[napi(constructor)]
    pub fn new(driver: &Driver) -> JsAgent {
        JsAgent {
            inner: Arc::new(agent::Agent::new(driver.core.clone())),
        }
    }

    /// `settleMs`, `image`, `grid`, `cols` (all optional).
    #[napi(ts_return_type = "Promise<JsSnapshot>")]
    pub fn snapshot(
        &self,
        settle_ms: Option<u32>,
        image: Option<bool>,
        grid: Option<bool>,
        cols: Option<u32>,
    ) -> AsyncTask<SnapshotTask> {
        let d = agent::SnapshotOptions::default();
        let opts = agent::SnapshotOptions {
            settle: settle_ms.map_or(d.settle, |ms| std::time::Duration::from_millis(ms.into())),
            image: image.unwrap_or(d.image),
            grid: grid.unwrap_or(d.grid),
            cols: cols.unwrap_or(d.cols),
        };
        AsyncTask::new(SnapshotTask {
            agent: self.inner.clone(),
            opts,
        })
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn tap(&self, target: String, expect: Option<String>) -> AsyncTask<TapTask> {
        AsyncTask::new(TapTask {
            agent: self.inner.clone(),
            target,
            expect,
        })
    }

    /// `steps`: a JSON array of steps (index.js stringifies).
    #[napi]
    pub fn steps(&self, steps: String) -> Result<AsyncTask<StepsTask>> {
        let steps: Vec<agent::Step> = serde_json::from_str(&steps)
            .map_err(|e| Error::new(Status::InvalidArg, format!("steps: {e}")))?;
        Ok(AsyncTask::new(StepsTask {
            agent: self.inner.clone(),
            steps,
        }))
    }
}

/// A named grid over a screen: rows lettered down (A, B, …), columns numbered across.
#[napi(js_name = "Grid")]
pub struct JsGrid {
    inner: sd::Grid,
}

#[napi]
impl JsGrid {
    #[napi(constructor)]
    pub fn new(screen_width: f64, screen_height: f64, cols: Option<u32>) -> JsGrid {
        JsGrid {
            inner: sd::Grid::new(
                (screen_width, screen_height),
                cols.unwrap_or(sd::Grid::DEFAULT_COLS),
            ),
        }
    }

    #[napi(getter)]
    pub fn rows(&self) -> u32 {
        self.inner.rows
    }

    #[napi(getter)]
    pub fn cols(&self) -> u32 {
        self.inner.cols
    }

    /// Name of the cell at `row`, `col` (from 0), e.g. (2, 3) → "C4".
    #[napi]
    pub fn name(&self, row: u32, col: u32) -> String {
        self.inner.name(row, col)
    }

    /// Centre of a named cell in 0..1 of the screen, or null outside the grid.
    #[napi]
    pub fn center(&self, name: String) -> Option<JsPoint> {
        self.inner.center(&name).map(|(x, y)| JsPoint { x, y })
    }

    /// A named cell as {x, y, width, height} in 0..1 of the screen, or null.
    #[napi]
    pub fn rect(&self, name: String) -> Option<JsRect> {
        self.inner.rect(&name).map(|[x, y, width, height]| JsRect {
            x,
            y,
            width,
            height,
        })
    }

    /// Name of the cell containing (x, y) in 0..1 of the screen.
    #[napi]
    pub fn at(&self, x: f64, y: f64) -> String {
        self.inner.at(x, y)
    }
}

#[napi(object)]
pub struct JsPoint {
    pub x: f64,
    pub y: f64,
}

#[napi(object)]
pub struct JsRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[napi(object)]
pub struct JsSize {
    pub width: f64,
    pub height: f64,
}

#[napi(object)]
pub struct JsStats {
    pub fps: f64,
    pub active_fps: f64,
    pub idle_fps: f64,
    pub last_response_ms: Option<f64>,
    pub max_active_gap_ms: f64,
    pub frames: f64,
}

/// One drained event; a frame carries the BGRA bytes (a Buffer, built on this thread).
#[napi(object)]
pub struct JsEvent {
    pub r#type: String,
    pub state: Option<String>,
    pub text: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub source_width: Option<f64>,
    pub source_height: Option<f64>,
    pub seq: Option<f64>,
    pub data: Option<Buffer>,
}

#[napi(object)]
pub struct StartOpts {
    pub udid: Option<String>,
    pub fps: Option<u32>,
    pub scale: Option<u32>,
    pub helper_dir: Option<String>,
}

#[napi(object)]
pub struct JsModifiers {
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub meta: bool,
}

#[napi]
pub fn list_devices() -> Result<Vec<Device>> {
    sd::list_devices().map_err(to_napi).map(|ds| {
        ds.into_iter()
            .map(|d| Device {
                udid: d.udid,
                name: d.name,
                state: d.state,
                device_type: d.device_type,
                runtime: d.runtime,
            })
            .collect()
    })
}

#[napi]
pub fn list_apps(udid: String) -> Result<Vec<App>> {
    sd::list_apps(&udid)
        .map_err(to_napi)
        .map(|apps| apps.into_iter().map(to_app).collect())
}

#[napi]
pub fn boot(udid: String) -> Result<()> {
    sd::boot(&udid).map_err(to_napi)
}

#[napi]
pub fn shutdown(udid: String) -> Result<()> {
    sd::shutdown(&udid).map_err(to_napi)
}

enum Ev {
    State(&'static str),
    Note(String),
}

/// The pump TSFN: CalleeHandled=false (the JS function gets the plain token, not
/// (err, value)), default Weak/MaxQueueSize.
type PumpFn = ThreadsafeFunction<bool, Unknown<'static>, bool, Status, false>;

struct Pending {
    fifo: VecDeque<Ev>,
    frame: Option<sd::Frame>,
}

struct Pump {
    pending: Mutex<Pending>,
    /// One pump call scheduled at a time; cleared (with re-arm check) by `pump_done`.
    in_flight: AtomicBool,
    stopped: AtomicBool,
    tsfn: Mutex<Option<PumpFn>>,
}

impl Pump {
    fn on_event(&self, ev: sd::Event) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        match ev {
            sd::Event::Frame(f) => {
                let mut p = self.pending.lock().unwrap();
                // Latest wins: replace whatever was parked.
                p.frame = Some(f);
            }
            sd::Event::State(s) => {
                let label = match s {
                    sd::State::Starting => "starting",
                    sd::State::Live => "live",
                    sd::State::InputDisplaced => "input-displaced",
                    sd::State::Restarting => "restarting",
                    sd::State::Stopped => "stopped",
                };
                self.pending
                    .lock()
                    .unwrap()
                    .fifo
                    .push_back(Ev::State(label));
            }
            sd::Event::Note(n) => self.pending.lock().unwrap().fifo.push_back(Ev::Note(n)),
        }
        self.schedule();
    }

    /// Schedule a pump if none is in flight.
    fn schedule(&self) {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return; // one is already queued; it will drain whatever parks before it runs
        }
        let guard = self.tsfn.lock().unwrap();
        let Some(tsfn) = guard.as_ref() else {
            // Released between the check and here: un-arm, nothing more will be delivered.
            self.in_flight.store(false, Ordering::SeqCst);
            return;
        };
        let _ = tsfn.call(true, ThreadsafeFunctionCallMode::NonBlocking);
    }

    /// JS side finished draining: re-arm, and if something parked meanwhile, pump again.
    fn pump_done(&self) {
        self.in_flight.store(false, Ordering::SeqCst);
        let has = {
            let p = self.pending.lock().unwrap();
            p.frame.is_some() || !p.fifo.is_empty()
        };
        if has && !self.stopped.load(Ordering::SeqCst) {
            self.schedule();
        }
    }
}

#[napi]
pub struct Driver {
    core: Arc<sd::Driver>,
    pump: Arc<Pump>,
}

fn phase(s: &str) -> Result<sd::Phase> {
    match s {
        "down" => Ok(sd::Phase::Down),
        "move" => Ok(sd::Phase::Move),
        "up" => Ok(sd::Phase::Up),
        other => Err(Error::new(
            Status::InvalidArg,
            format!("unknown phase {other:?}"),
        )),
    }
}

fn scroll_phase(s: &str) -> Result<sd::ScrollPhase> {
    match s {
        "began" => Ok(sd::ScrollPhase::Began),
        "changed" => Ok(sd::ScrollPhase::Changed),
        "ended" => Ok(sd::ScrollPhase::Ended),
        "momentum" => Ok(sd::ScrollPhase::Momentum),
        other => Err(Error::new(
            Status::InvalidArg,
            format!("unknown scroll phase {other:?}"),
        )),
    }
}

fn button(s: &str) -> Result<sd::Button> {
    match s {
        "home" => Ok(sd::Button::Home),
        "lock" => Ok(sd::Button::Lock),
        "side" => Ok(sd::Button::SideButton),
        "siri" => Ok(sd::Button::Siri),
        "volume-up" => Ok(sd::Button::VolumeUp),
        "volume-down" => Ok(sd::Button::VolumeDown),
        other => Err(Error::new(
            Status::InvalidArg,
            format!("unknown button {other:?}"),
        )),
    }
}

#[napi]
impl Driver {
    /// `pump` is the JS-side drain loop (index.js wraps the host's onEvent with it).
    /// The call argument is a token; the real events come back through `take_event`.
    /// Resolves once the driver is live (or after the bounded wait); the wait runs on the
    /// libuv threadpool, so the JS thread stays free meanwhile.
    #[napi(ts_return_type = "Promise<Driver>")]
    pub fn start(
        opts: StartOpts,
        pump: Function<'_, bool, Unknown<'static>>,
    ) -> Result<AsyncTask<StartTask>> {
        let tsfn: PumpFn = pump
            .build_threadsafe_function::<bool>()
            .build()
            .map_err(|e| Error::new(Status::GenericFailure, format!("pump function: {e}")))?;
        let pump_inner = Arc::new(Pump {
            pending: Mutex::new(Pending {
                fifo: VecDeque::new(),
                frame: None,
            }),
            in_flight: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            tsfn: Mutex::new(Some(tsfn)),
        });
        let selector = match opts.udid {
            Some(u) => sd::DeviceSelector::Udid(u),
            None => sd::DeviceSelector::FirstBootedOrIphone,
        };
        let core_opts = sd::Options {
            device: selector,
            fps: opts.fps.unwrap_or(60),
            scale: opts.scale.unwrap_or(2),
            helper_dir: opts.helper_dir.map(std::path::PathBuf::from),
        };
        Ok(AsyncTask::new(StartTask {
            opts: Some(core_opts),
            pump: pump_inner,
        }))
    }

    #[napi(getter)]
    pub fn udid(&self) -> String {
        self.core.udid().to_string()
    }

    #[napi(getter)]
    pub fn state(&self) -> String {
        match self.core.state() {
            sd::State::Starting => "starting",
            sd::State::Live => "live",
            sd::State::InputDisplaced => "input-displaced",
            sd::State::Restarting => "restarting",
            sd::State::Stopped => "stopped",
        }
        .to_string()
    }

    /// Drain one parked event (FIFO first, then the latest frame); null when empty.
    /// The Buffer for a frame is created here, on the JS thread.
    #[napi]
    pub fn take_event(&self) -> Result<Option<JsEvent>> {
        let mut p = self.pump.pending.lock().unwrap();
        if let Some(ev) = p.fifo.pop_front() {
            return match ev {
                Ev::State(s) => Ok(Some(JsEvent {
                    r#type: "state".into(),
                    state: Some(s.into()),
                    text: None,
                    width: None,
                    height: None,
                    source_width: None,
                    source_height: None,
                    seq: None,
                    data: None,
                })),
                Ev::Note(n) => Ok(Some(JsEvent {
                    r#type: "note".into(),
                    state: None,
                    text: Some(n),
                    width: None,
                    height: None,
                    source_width: None,
                    source_height: None,
                    seq: None,
                    data: None,
                })),
            };
        }
        Ok(p.frame.take().map(|f| {
            // Buffer::from moves the Vec (no copy); the JS side owns it from here.
            let data: Buffer = Buffer::from(f.data);
            JsEvent {
                r#type: "frame".into(),
                state: None,
                text: None,
                width: Some(f.width.into()),
                height: Some(f.height.into()),
                source_width: Some(f.source_width.into()),
                source_height: Some(f.source_height.into()),
                seq: Some(f.seq as f64),
                data: Some(data),
            }
        }))
    }

    /// The JS pump finished: re-arm and, if newer events parked, pump again immediately.
    #[napi]
    pub fn pump_done(&self) {
        self.pump.pump_done();
    }

    #[napi]
    pub fn pointer(&self, phase_str: String, x: f64, y: f64, mods: JsModifiers) -> Result<()> {
        let p = phase(&phase_str)?;
        self.core.pointer(
            p,
            x,
            y,
            sd::Modifiers {
                alt: mods.alt,
                shift: mods.shift,
                ctrl: mods.ctrl,
                meta: mods.meta,
            },
        );
        Ok(())
    }

    #[napi]
    pub fn scroll(&self, phase_str: String, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        let p = scroll_phase(&phase_str)?;
        self.core.scroll(p, x, y, dx, dy);
        Ok(())
    }

    #[napi]
    pub fn tap(&self, x: f64, y: f64) {
        self.core.tap(x, y);
    }

    /// Two taps 100 ms apart. Resolves after the second.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn double_tap(&self, x: f64, y: f64) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || {
            core.double_tap(x, y);
            Ok(())
        })
    }

    /// Press at (x, y), hold for `hold_ms` (default 800), then lift — iOS's long press.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn long_press(&self, x: f64, y: f64, hold_ms: Option<f64>) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || {
            core.long_press(x, y, ms(hold_ms.unwrap_or(800.0)));
            Ok(())
        })
    }

    /// Drag between two points, each 0..1 in the box: down, hold `hold_ms` (default 800),
    /// move over `move_ms` (default 500), hold `destination_hold_ms` (default 0), lift.
    #[napi(ts_return_type = "Promise<void>")]
    #[allow(clippy::too_many_arguments)] // one napi arg per JS parameter, by design
    pub fn drag(
        &self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        hold_ms: Option<f64>,
        move_ms: Option<f64>,
        destination_hold_ms: Option<f64>,
    ) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        let timing = sd::DragTiming {
            source_hold: ms(hold_ms.unwrap_or(800.0)),
            movement: ms(move_ms.unwrap_or(500.0)),
            destination_hold: ms(destination_hold_ms.unwrap_or(0.0)),
        };
        blocking(move || {
            core.drag_with_holds((x1, y1), (x2, y2), timing);
            Ok(())
        })
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn accessibility(&self) -> AsyncTask<AxTask> {
        AsyncTask::new(AxTask {
            core: self.core.clone(),
        })
    }

    #[napi]
    pub fn touch2(&self, phase_str: String, x1: f64, y1: f64, x2: f64, y2: f64) -> Result<()> {
        let p = phase(&phase_str)?;
        self.core.touch2(p, x1, y1, x2, y2);
        Ok(())
    }

    #[napi]
    pub fn screen_points(&self) -> Option<JsSize> {
        self.core
            .screen_points()
            .map(|(width, height)| JsSize { width, height })
    }

    #[napi]
    pub fn key(&self, name: String) -> Result<bool> {
        Ok(self.core.key(&name))
    }

    #[napi]
    pub fn key_combo(&self, keys: Vec<String>) -> bool {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.core.key_combo(&keys)
    }

    #[napi(ts_return_type = "Promise<string>")]
    pub fn clipboard_read(&self) -> AsyncTask<Blocking<String>> {
        let core = self.core.clone();
        blocking(move || core.clipboard_read())
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn clipboard_write(&self, text: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.clipboard_write(&text))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn open_url(&self, url: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.open_url(&url))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn launch(&self, bundle_id: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.launch(&bundle_id))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn terminate(&self, bundle_id: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.terminate(&bundle_id))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_location(&self, lat: f64, lon: f64) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.set_location(lat, lon))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_appearance(&self, appearance: String) -> Result<AsyncTask<Blocking<()>>> {
        let appearance = match appearance.to_ascii_lowercase().as_str() {
            "light" => sd::Appearance::Light,
            "dark" => sd::Appearance::Dark,
            _ => return Err(Error::from_reason("appearance must be light or dark")),
        };
        let core = self.core.clone();
        Ok(blocking(move || core.set_appearance(appearance)))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn grant_permission(&self, service: String, bundle_id: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.grant_permission(&service, &bundle_id))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn revoke_permission(&self, service: String, bundle_id: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.revoke_permission(&service, &bundle_id))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn reset_permission(&self, service: String, bundle_id: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.reset_permission(&service, &bundle_id))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_content_size(&self, size: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.set_content_size(&size))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn status_bar_override(&self) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.status_bar_override())
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn status_bar_clear(&self) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.status_bar_clear())
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn push_notification(
        &self,
        bundle_id: String,
        payload_path: String,
    ) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.push_notification(&bundle_id, &payload_path))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn reset_keychain(&self) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.reset_keychain())
    }

    #[napi(ts_return_type = "Promise<Array<App>>")]
    pub fn apps(&self) -> AsyncTask<Blocking<Vec<App>>> {
        let core = self.core.clone();
        blocking(move || {
            core.apps()
                .map(|apps| apps.into_iter().map(to_app).collect())
        })
    }

    /// Terminates the app and deletes its data container's contents. Destructive.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn clear_app_data(&self, bundle_id: String) -> AsyncTask<Blocking<()>> {
        let core = self.core.clone();
        blocking(move || core.clear_app_data(&bundle_id))
    }

    #[napi]
    pub fn text(&self, s: String) {
        self.core.text(&s);
    }

    #[napi]
    pub fn button(&self, name: String, presses: Option<u32>, hold_secs: Option<f64>) -> Result<()> {
        let b = button(&name)?;
        self.core
            .button(b, presses.unwrap_or(1), hold_secs.unwrap_or(0.0) as f32);
        Ok(())
    }

    #[napi]
    pub fn stats(&self) -> JsStats {
        let s = self.core.stats();
        JsStats {
            fps: f64::from(s.fps),
            active_fps: f64::from(s.active_fps),
            idle_fps: f64::from(s.idle_fps),
            last_response_ms: s.last_response_ms.map(f64::from),
            max_active_gap_ms: f64::from(s.max_active_gap_ms),
            frames: s.frames as f64,
        }
    }

    /// Stop the driver. Idempotent; after it returns, no further events reach JS and the
    /// event loop is not kept alive by this driver.
    #[napi]
    pub fn stop(&self) {
        self.pump.stopped.store(true, Ordering::SeqCst);
        self.core.stop();
        // Release the TSFN so the process/test runner can exit.
        if let Some(tsfn) = self.pump.tsfn.lock().unwrap().take() {
            drop(tsfn);
        }
        self.pump.pending.lock().unwrap().fifo.clear();
        self.pump.pending.lock().unwrap().frame = None;
    }
}
