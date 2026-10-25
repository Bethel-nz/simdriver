use std::path::Path;

/// Directory containing the helper binaries (`sdframe`, `sdhid`) built by `build.rs`.
pub fn baked_helper_dir() -> &'static Path {
    Path::new(env!("SIMDRIVER_BAKED_HELPER_DIR"))
}

mod devices;
mod driver;
mod error;
mod frames;
mod gesture;
mod helper;
mod hid;
mod protocol;
mod shm;
mod stats;

pub use devices::{boot, list_devices, shutdown, Device, DeviceSelector};
pub use driver::{Button, Driver, Event, Modifiers, Options, State};
pub use error::Error;
pub use gesture::ScrollPhase;
pub use protocol::Phase;
pub use shm::Frame;
pub use stats::Stats;

#[doc(hidden)]
pub mod __internal {
    pub use crate::frames::FramePipeline;
    pub use crate::helper::{HELPER_DIR_ENV, Supervised, helper_dir};
    pub use crate::hid::{HidClient, HidEvent, QUEUE_CAP};
    pub use crate::protocol::*;
    pub use crate::shm::*;
}
