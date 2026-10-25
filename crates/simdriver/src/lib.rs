use std::path::Path;

/// Directory containing the helper binaries (`sdframe`, `sdhid`) built by `build.rs`.
pub fn baked_helper_dir() -> &'static Path {
    Path::new(env!("SIMDRIVER_BAKED_HELPER_DIR"))
}
