use crate::Error;
use serde::Deserialize;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RUNTIME_PREFIX: &str = "com.apple.CoreSimulator.SimRuntime.";
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub state: String,
    pub device_type: String,
    pub runtime: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelector {
    Udid(String),
    FirstBootedOrIphone,
}

impl DeviceSelector {
    pub fn resolve(&self, devices: &[Device]) -> Result<Device, Error> {
        let found = match self {
            DeviceSelector::Udid(u) => devices.iter().find(|d| &d.udid == u),
            DeviceSelector::FirstBootedOrIphone => devices
                .iter()
                .find(|d| d.state == "Booted")
                .or_else(|| devices.iter().find(|d| d.device_type.contains("iPhone"))),
        };
        found.cloned().ok_or(Error::NoDevice)
    }
}

#[derive(Deserialize)]
struct Raw {
    devices: std::collections::BTreeMap<String, Vec<RawDevice>>,
}

#[derive(Deserialize)]
struct RawDevice {
    udid: String,
    name: String,
    state: String,
    #[serde(rename = "deviceTypeIdentifier", default)]
    device_type: String,
    #[serde(rename = "isAvailable", default = "yes")]
    is_available: bool,
}

fn yes() -> bool {
    true
}

pub(crate) fn parse_devices(json: &str) -> Result<Vec<Device>, Error> {
    let raw: Raw = serde_json::from_str(json)
        .map_err(|e| Error::Simctl(format!("bad simctl device json: {e}")))?;
    let mut out = Vec::new();
    for (runtime, list) in raw.devices {
        let runtime = runtime.strip_prefix(RUNTIME_PREFIX).unwrap_or(&runtime).to_string();
        for d in list.into_iter().filter(|d| d.is_available) {
            out.push(Device {
                udid: d.udid,
                name: d.name,
                state: d.state,
                device_type: d.device_type,
                runtime: runtime.clone(),
            });
        }
    }
    Ok(out)
}

/// Run `xcrun simctl <args>`; returns (success, stderr). Kills the child after `timeout`.
fn run_simctl(args: &[&str], timeout: Duration) -> Result<(bool, String, String), Error> {
    let mut child = Command::new("xcrun")
        .arg("simctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Simctl(format!(
                "simctl {} timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = out_t.join().unwrap_or_default();
    let err = err_t.join().unwrap_or_default();
    Ok((status.success(), out, err))
}

pub fn list_devices() -> Result<Vec<Device>, Error> {
    let (ok, out, err) = run_simctl(&["list", "devices", "-j"], Duration::from_secs(30))?;
    if !ok {
        return Err(Error::Simctl(err.trim().to_string()));
    }
    parse_devices(&out)
}

/// Boot headlessly and wait until ready. Already-booted is success.
pub fn boot(udid: &str) -> Result<(), Error> {
    let (ok, _, err) = run_simctl(&["bootstatus", udid, "-b"], BOOT_TIMEOUT)?;
    if ok || err.contains("already booted") || err.contains("current state: Booted") {
        return Ok(());
    }
    Err(Error::Simctl(format!(
        "bootstatus failed: {}",
        if err.trim().is_empty() { "(no stderr)" } else { err.trim() }
    )))
}

/// Shut down. Already-shutdown is success.
pub fn shutdown(udid: &str) -> Result<(), Error> {
    let (ok, _, err) = run_simctl(&["shutdown", udid], Duration::from_secs(30))?;
    if ok || err.contains("current state: Shutdown") {
        return Ok(());
    }
    Err(Error::Simctl(err.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/simctl_devices.json");

    fn dev(udid: &str, state: &str, ty: &str) -> Device {
        Device {
            udid: udid.into(),
            name: udid.into(),
            state: state.into(),
            device_type: ty.into(),
            runtime: "iOS-27-0".into(),
        }
    }

    #[test]
    fn parses_fixture() {
        let ds = parse_devices(FIXTURE).unwrap();
        assert_eq!(ds.len(), 3);
        let d = ds.iter().find(|d| d.udid == "8BC981D2-7C86-4EFC-944C-AC33FD0262FC").unwrap();
        assert_eq!(d.name, "iPhone 18 Pro");
        assert_eq!(d.state, "Booted");
        assert_eq!(d.runtime, "iOS-27-0");
        assert_eq!(d.device_type, "com.apple.CoreSimulator.SimDeviceType.iPhone-18-Pro");
        assert!(ds.iter().any(|d| d.runtime == "iOS-27-1"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(parse_devices("nope"), Err(Error::Simctl(_))));
    }

    #[test]
    fn resolve_exact_udid() {
        let ds = vec![dev("A", "Shutdown", "iPhone-X"), dev("B", "Booted", "iPad")];
        let d = DeviceSelector::Udid("A".into()).resolve(&ds).unwrap();
        assert_eq!(d.udid, "A");
    }

    #[test]
    fn resolve_unknown_udid() {
        let ds = vec![dev("A", "Shutdown", "iPhone-X")];
        assert!(matches!(DeviceSelector::Udid("Z".into()).resolve(&ds), Err(Error::NoDevice)));
    }

    #[test]
    fn resolve_prefers_booted() {
        let ds = vec![dev("A", "Shutdown", "iPhone-X"), dev("B", "Booted", "iPad")];
        assert_eq!(DeviceSelector::FirstBootedOrIphone.resolve(&ds).unwrap().udid, "B");
    }

    #[test]
    fn resolve_iphone_fallback() {
        let ds = vec![dev("A", "Shutdown", "iPad"), dev("B", "Shutdown", "iPhone-X")];
        assert_eq!(DeviceSelector::FirstBootedOrIphone.resolve(&ds).unwrap().udid, "B");
    }

    #[test]
    fn resolve_empty() {
        assert!(matches!(DeviceSelector::FirstBootedOrIphone.resolve(&[]), Err(Error::NoDevice)));
    }
}
