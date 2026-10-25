use std::path::Path;

/// Directory containing the helper binaries (`sdframe`, `sdhid`) built by `build.rs`.
pub fn baked_helper_dir() -> &'static Path {
    Path::new(env!("SIMDRIVER_BAKED_HELPER_DIR"))
}

mod devices;
mod error;
// Consumed by the driver (Task 9); unused until then.
#[allow(dead_code)]
mod gesture;
mod protocol;
mod shm;
// Consumed by the driver (Task 9); unused until then.
#[allow(dead_code)]
mod stats;

pub use devices::{boot, list_devices, shutdown, Device, DeviceSelector};
pub use error::Error;
pub use gesture::ScrollPhase;
pub use protocol::Phase;
pub use shm::Frame;
pub use stats::Stats;

#[doc(hidden)]
pub mod __internal {
    pub use crate::protocol::*;
    pub use crate::shm::*;
}
