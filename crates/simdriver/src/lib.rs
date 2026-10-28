use std::path::Path;

/// Directory containing the helper binaries (`sdframe`, `sdhid`, `sdax`) built by `build.rs`.
pub fn baked_helper_dir() -> &'static Path {
    Path::new(env!("SIMDRIVER_BAKED_HELPER_DIR"))
}

mod ax;
mod devices;
mod driver;
mod error;
// Public only so cbindgen can scan it (include/simdriver.h); not part of the Rust API.
#[doc(hidden)]
pub mod ffi;
mod frames;
mod gesture;
mod grid;
mod helper;
mod hid;
mod protocol;
mod shm;
mod stats;
mod tap;

pub use ax::AxNode;
pub use devices::{
    App, Appearance, Device, DeviceSelector, boot, clear_app_data, clipboard_read, clipboard_write,
    grant_permission, launch, list_apps, list_devices, open_url, push_notification, reset_keychain,
    reset_permission, revoke_permission, set_appearance, set_content_size, set_location, shutdown,
    status_bar_clear, status_bar_override, terminate,
};
pub use driver::{Button, DragTiming, Driver, Event, Modifiers, Options, State};
pub use error::Error;
pub use gesture::ScrollPhase;
pub use grid::Grid;
pub use protocol::Phase;
pub use shm::Frame;
pub use stats::Stats;
pub use tap::{FrameTap, TapFeeder, TapFrame, TapHealth, TapRecv};

#[doc(hidden)]
pub mod __internal {
    pub use crate::frames::FramePipeline;
    pub use crate::helper::{HELPER_DIR_ENV, Supervised, helper_dir};
    pub use crate::hid::{HidClient, HidEvent, QUEUE_CAP};
    pub use crate::protocol::*;
    pub use crate::shm::*;
}
