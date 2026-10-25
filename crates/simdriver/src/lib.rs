use std::path::Path;

/// Directory containing the helper binaries (`sdframe`, `sdhid`) built by `build.rs`.
pub fn baked_helper_dir() -> &'static Path {
    Path::new(env!("SIMDRIVER_BAKED_HELPER_DIR"))
}

mod devices;
mod error;
mod protocol;
mod shm;

pub use devices::{boot, list_devices, shutdown, Device, DeviceSelector};
pub use error::Error;
pub use protocol::Phase;
pub use shm::Frame;

#[doc(hidden)]
pub mod __internal {
    pub use crate::protocol::*;
    pub use crate::shm::*;
}
