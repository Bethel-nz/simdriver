use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    NoDevice,
    NotBooted(String),
    HelperMissing(PathBuf),
    Spawn(String),
    Shm(String),
    Simctl(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDevice => write!(f, "no simulator device found"),
            Error::NotBooted(u) => write!(f, "simulator {u} is not booted"),
            Error::HelperMissing(p) => write!(f, "helper binary missing: {}", p.display()),
            Error::Spawn(s) => write!(f, "failed to spawn helper: {s}"),
            Error::Shm(s) => write!(f, "shared memory error: {s}"),
            Error::Simctl(s) => write!(f, "simctl error: {s}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
