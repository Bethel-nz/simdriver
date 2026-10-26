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
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Env;
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
  sd::list_devices()
    .map_err(to_napi)
    .map(|ds| {
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
        self.pending.lock().unwrap().fifo.push_back(Ev::State(label));
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
  core: sd::Driver,
  pump: Arc<Pump>,
}

fn phase(s: &str) -> Result<sd::Phase> {
  match s {
    "down" => Ok(sd::Phase::Down),
    "move" => Ok(sd::Phase::Move),
    "up" => Ok(sd::Phase::Up),
    other => Err(Error::new(Status::InvalidArg, format!("unknown phase {other:?}"))),
  }
}

fn scroll_phase(s: &str) -> Result<sd::ScrollPhase> {
  match s {
    "began" => Ok(sd::ScrollPhase::Began),
    "changed" => Ok(sd::ScrollPhase::Changed),
    "ended" => Ok(sd::ScrollPhase::Ended),
    "momentum" => Ok(sd::ScrollPhase::Momentum),
    other => Err(Error::new(Status::InvalidArg, format!("unknown scroll phase {other:?}"))),
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
    other => Err(Error::new(Status::InvalidArg, format!("unknown button {other:?}"))),
  }
}

#[napi]
impl Driver {
  /// `pump` is the JS-side drain loop (index.js wraps the host's onEvent with it).
  /// The call argument is a token; the real events come back through `take_event`.
  #[napi(factory)]
  pub fn start(env: Env, opts: StartOpts, pump: Function<'_, bool, Unknown<'static>>) -> Result<Driver> {
    let tsfn: PumpFn = pump
      .build_threadsafe_function::<bool>()
      .build()
      .map_err(|e| Error::new(Status::GenericFailure, format!("pump function: {e}")))?;
    let pump_inner = Arc::new(Pump {
      pending: Mutex::new(Pending { fifo: VecDeque::new(), frame: None }),
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
    let p = pump_inner.clone();
    let core = sd::Driver::start(core_opts, move |ev| p.on_event(ev)).map_err(to_napi)?;
    let _ = env; // Env kept for signature stability; TSFN creation binds the env itself.
    Ok(Driver { core, pump: pump_inner })
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
    self
      .core
      .pointer(p, x, y, sd::Modifiers { alt: mods.alt, shift: mods.shift, ctrl: mods.ctrl, meta: mods.meta });
    Ok(())
  }

  #[napi]
  pub fn scroll(&self, phase_str: String, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
    let p = scroll_phase(&phase_str)?;
    self.core.scroll(p, x, y, dx, dy);
    Ok(())
  }

  #[napi]
  pub fn touch2(&self, phase_str: String, x1: f64, y1: f64, x2: f64, y2: f64) -> Result<()> {
    let p = phase(&phase_str)?;
    self.core.touch2(p, x1, y1, x2, y2);
    Ok(())
  }

  #[napi]
  pub fn screen_points(&self) -> Option<JsSize> {
    self.core.screen_points().map(|(width, height)| JsSize { width, height })
  }

  #[napi]
  pub fn key(&self, name: String) -> Result<bool> {
    Ok(self.core.key(&name))
  }

  #[napi]
  pub fn text(&self, s: String) {
    self.core.text(&s);
  }

  #[napi]
  pub fn button(&self, name: String, presses: Option<u32>, hold_secs: Option<f64>) -> Result<()> {
    let b = button(&name)?;
    self.core.button(b, presses.unwrap_or(1), hold_secs.unwrap_or(0.0) as f32);
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
