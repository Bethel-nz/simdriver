//! Minimal simdriver host: a winit window that paints the driver's BGRA frames through
//! softbuffer and forwards mouse, trackpad and keys as normalized 0..1 input.
//!
//! BGRA bytes read as a little-endian u32 are 0xAARRGGBB; masking alpha gives
//! softbuffer's 0x00RRGGBB directly. The window is sized to the frame, so window
//! coordinates map straight onto the device screen.
//!
//! Usage: rust-minimal [--udid <udid>] [--seconds N]   (N: print stats each second, exit)
use simdriver::{DeviceSelector, Driver, Event, Frame, Modifiers, Options, Phase, ScrollPhase};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Posted by the driver's frame thread: a new frame is parked, redraw.
struct NewFrame;

struct App {
    driver: Driver,
    latest: Arc<Mutex<Option<Frame>>>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    frame_size: (u32, u32),
    mods: ModifiersState,
    mouse_down: bool,
    cursor: (f64, f64),
    deadline: Option<Instant>,
    next_stats: Instant,
}

impl App {
    /// Window pixels → 0..1 of the device screen (the window is the frame's size).
    fn norm(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let Some(w) = self.window.as_ref() else { return (0.5, 0.5) };
        let s = w.inner_size();
        (x / s.width.max(1) as f64, y / s.height.max(1) as f64) // not clamped: the core clamps
    }

    fn mods(&self) -> Modifiers {
        Modifiers {
            alt: self.mods.alt_key(),
            shift: self.mods.shift_key(),
            ctrl: self.mods.control_key(),
            meta: self.mods.super_key(),
        }
    }

    fn paint(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else { return };
        let Some(frame) = self.latest.lock().unwrap().take() else { return };
        let (w, h) = (frame.width, frame.height);
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else { return };
        if self.frame_size != (w, h) {
            self.frame_size = (w, h);
            let _ = window.request_inner_size(PhysicalSize::new(w, h));
        }
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        for (out, px) in buf.iter_mut().zip(frame.data.chunks_exact(4)) {
            *out = u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & 0x00FF_FFFF;
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler<NewFrame> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("simdriver").with_resizable(false);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface"));
        self.window = Some(window);
    }

    fn user_event(&mut self, _: &ActiveEventLoop, _: NewFrame) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next_stats {
            self.next_stats = now + Duration::from_secs(1);
            let s = self.driver.stats();
            println!(
                "fps {:.1} (active {:.1}, idle {:.1}) · frames {} · last response {:?} ms · {:?}",
                s.fps, s.active_fps, s.idle_fps, s.frames, s.last_response_ms, self.driver.state()
            );
        }
        if self.deadline.is_some_and(|d| now >= d) {
            event_loop.exit();
        }
        // Wake for the next stats line even when no frames arrive.
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_stats));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                if self.mouse_down {
                    let (x, y) = self.norm(self.cursor);
                    self.driver.pointer(Phase::Move, x, y, self.mods());
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.mouse_down = state.is_pressed();
                let (x, y) = self.norm(self.cursor);
                let phase = if self.mouse_down { Phase::Down } else { Phase::Up };
                self.driver.pointer(phase, x, y, self.mods());
            }
            // Trackpad only: a notched wheel (LineDelta) has no phases to lift on.
            WindowEvent::MouseWheel { delta: MouseScrollDelta::PixelDelta(d), phase, .. } => {
                let phase = match phase {
                    TouchPhase::Started => ScrollPhase::Began,
                    TouchPhase::Moved => ScrollPhase::Changed,
                    TouchPhase::Ended | TouchPhase::Cancelled => ScrollPhase::Ended,
                };
                let (x, y) = self.norm(self.cursor);
                let (dx, dy) = self.norm((d.x, d.y)); // macOS deltas already follow the fingers
                self.driver.scroll(phase, x, y, dx, dy);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match &event.logical_key {
                    Key::Named(NamedKey::Space) => self.driver.text(" "),
                    // NamedKey's names (Enter, Escape, ArrowLeft, …) are the DOM spellings
                    // `Driver::key` accepts; unknown ones return false and are dropped.
                    Key::Named(n) => {
                        let _ = self.driver.key(&format!("{n:?}"));
                    }
                    Key::Character(s) if !self.mods.super_key() && !self.mods.control_key() => {
                        self.driver.text(s)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let (mut udid, mut seconds) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--udid" => udid = args.next(),
            "--seconds" => seconds = args.next().and_then(|s| s.parse::<u64>().ok()),
            other => eprintln!("ignoring argument {other:?}"),
        }
    }

    let event_loop = EventLoop::<NewFrame>::with_user_event().build().expect("event loop");
    let proxy: EventLoopProxy<NewFrame> = event_loop.create_proxy();
    let latest: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let parked = latest.clone();
    let opts = Options {
        device: udid.map_or(DeviceSelector::FirstBootedOrIphone, DeviceSelector::Udid),
        ..Options::default()
    };
    // Latest frame wins: the callback parks it and wakes the event loop to draw it.
    let driver = Driver::start(opts, move |ev| match ev {
        Event::Frame(f) => {
            *parked.lock().unwrap() = Some(f);
            let _ = proxy.send_event(NewFrame);
        }
        Event::State(s) => println!("state: {s:?}"),
        Event::Note(n) => println!("note: {n}"),
    })
    .unwrap_or_else(|e| {
        eprintln!("Driver::start: {e} (is a simulator booted?)");
        std::process::exit(1);
    });

    let mut app = App {
        driver,
        latest,
        window: None,
        surface: None,
        frame_size: (0, 0),
        mods: ModifiersState::empty(),
        mouse_down: false,
        cursor: (0.0, 0.0),
        deadline: seconds.map(|s| Instant::now() + Duration::from_secs(s)),
        next_stats: Instant::now() + Duration::from_secs(1),
    };
    event_loop.run_app(&mut app).expect("event loop");
    app.driver.stop();
}
