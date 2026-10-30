//! The helper binaries, compiled into this crate and unpacked on first use. The build's own
//! output directory can't be relied on: `cargo install` deletes it after installing.
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const HELPERS: [(&str, &[u8]); 3] = [
    (
        "sdframe",
        include_bytes!(concat!(env!("OUT_DIR"), "/helpers/sdframe")),
    ),
    (
        "sdhid",
        include_bytes!(concat!(env!("OUT_DIR"), "/helpers/sdhid")),
    ),
    (
        "sdax",
        include_bytes!(concat!(env!("OUT_DIR"), "/helpers/sdax")),
    ),
];

/// Where this build's helpers are unpacked: the user's cache, else the temp dir. If neither
/// can be written the path is still returned, so starting a helper reports it as missing.
pub(crate) fn dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
        let temp = std::env::temp_dir();
        cache
            .iter()
            .chain([&temp])
            .find_map(|base| unpack(&base.join("simdriver/helpers")).ok())
            .unwrap_or_else(|| temp.join("simdriver/helpers").join(build_id()))
    })
}

/// One directory per build, so processes from different versions never share helpers.
fn build_id() -> String {
    let mut hasher = DefaultHasher::new();
    HELPERS.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn unpack(root: &Path) -> io::Result<PathBuf> {
    let dir = root.join(build_id());
    fs::create_dir_all(&dir)?;
    for (name, bytes) in HELPERS {
        let path = dir.join(name);
        if fs::read(&path).is_ok_and(|on_disk| on_disk == bytes) {
            continue;
        }
        // Write a new file then rename: a helper that's running is never overwritten in
        // place, and another process never sees a partial binary.
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&tmp)?;
        file.write_all(bytes)?;
        drop(file);
        fs::rename(&tmp, &path)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn scratch(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("simdriver-embed-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn unpacked_helpers_run() {
        let root = scratch("run");
        let dir = unpack(&root).unwrap();
        for (name, _) in HELPERS {
            let out = Command::new(dir.join(name)).output().unwrap();
            assert_eq!(
                out.status.code(),
                Some(2),
                "{name} with no args prints usage"
            );
        }
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            HELPERS.len(),
            "no temp files left behind"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_damaged_helper_is_replaced() {
        let root = scratch("damaged");
        let dir = unpack(&root).unwrap();
        fs::write(dir.join("sdhid"), b"truncated").unwrap();
        unpack(&root).unwrap();
        let (_, bytes) = HELPERS[1];
        assert_eq!(fs::read(dir.join("sdhid")).unwrap(), bytes);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unwritable_root_is_an_error() {
        assert!(unpack(Path::new("/nonexistent/simdriver")).is_err());
    }
}
