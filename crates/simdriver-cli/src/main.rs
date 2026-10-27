//! The simdriver command line: see the simulator's screen, act on it, and set it up —
//! one process per call. The agent layer's refs (`e4`) carry between calls through a
//! small state file per device, so `snapshot` then `tap e4` work as separate commands.
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use simdriver::DeviceSelector;
use simdriver::{self as sd, Device};
use simdriver_agent::{Agent, SnapshotOptions, Step};

const USAGE: &str = "Drive an iOS simulator from the command line.

Usage: simdriver <command> [arguments]

Devices:
  devices                                  List simulators, booted first.
  boot <udid> | shutdown <udid>
  apps                                     List installed apps.

The screen, through the agent layer:
  agent snapshot [--settle-ms N] [--grid] [--cols N] [--image <path>]
  agent tap <target> [--expect <text>]
  agent steps <json | @file>

Apps and simulator state:
  launch <bundle> | terminate <bundle> | open-url <url>
  clipboard get | clipboard set <text>
  set permission <grant|revoke|reset> <service> <bundle>
  set content-size <size> | set appearance <light|dark>
  set location <lat> <lon> | set status-bar <override|clear>
  notify <bundle> <aps-json>
  reset app-data <bundle> | reset keychain

A target is a ref from the last snapshot (e4), a label on screen (\"Log in\"),
or a grid cell (C4). The last snapshot is remembered per device between calls.

Options:
  --udid <udid>   Which simulator (default: the only booted one).
  --json          Machine-readable output.
  --fresh         agent: ignore the remembered snapshot.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(message)) => {
            if !message.is_empty() {
                eprintln!("simdriver: {message}");
            }
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Err(CliError::Fail(message)) => {
            eprintln!("simdriver: {message}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    Fail(String),
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Fail(message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        CliError::Fail(message.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        CliError::Fail(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        CliError::Fail(error.to_string())
    }
}

impl From<sd::Error> for CliError {
    fn from(error: sd::Error) -> Self {
        CliError::Fail(error.to_string())
    }
}

/// Flags that take a value; everything else `--flag`-shaped is boolean.
const VALUE_FLAGS: [&str; 5] = ["--udid", "--expect", "--image", "--cols", "--settle-ms"];

/// Positional words and named flags, with unknown flags rejected.
struct Args {
    words: Vec<String>,
    flags: Vec<(String, String)>,
}

fn parse_args(args: &[String], allowed: &[&str]) -> Result<Args, CliError> {
    let mut words = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--help" || arg == "-h" {
            return Err(CliError::Usage(String::new()));
        }
        if !arg.starts_with("--") {
            words.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--json" || arg == "--fresh" {
            flags.push((arg.clone(), String::new()));
            i += 1;
            continue;
        }
        if !allowed.contains(&arg.as_str()) {
            return Err(CliError::Usage(format!("unknown option {arg}")));
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            let value = args
                .get(i + 1)
                .ok_or_else(|| CliError::Usage(format!("{arg} needs a value")))?;
            flags.push((arg.clone(), value.clone()));
            i += 2;
        } else {
            flags.push((arg.clone(), String::new()));
            i += 1;
        }
    }
    Ok(Args { words, flags })
}

impl Args {
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
    fn bool(&self, name: &str) -> bool {
        self.flags.iter().any(|(key, _)| key == name)
    }
    fn word(&self, index: usize, what: &str) -> Result<String, CliError> {
        self.words
            .get(index)
            .cloned()
            .ok_or_else(|| CliError::Usage(format!("missing {what}")))
    }
}

fn run(args: &[String]) -> Result<(), CliError> {
    let command = args.first().ok_or_else(|| CliError::Usage(String::new()))?;
    let rest = &args[1..];
    match command.as_str() {
        "devices" => {
            let args = parse_args(rest, &[])?;
            let devices = sd::list_devices()?;
            if args.bool("--json") {
                let list: Vec<_> = devices
                    .iter()
                    .map(|d| serde_json::json!({"udid": d.udid, "name": d.name, "state": d.state, "deviceType": d.device_type, "runtime": d.runtime}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for device in devices {
                    println!("{:<10} {:<38} {}", device.state, device.udid, device.name);
                }
            }
            Ok(())
        }
        "boot" | "shutdown" => {
            let args = parse_args(rest, &[])?;
            let udid = args.word(0, "a device udid")?;
            if command == "boot" {
                sd::boot(&udid)?
            } else {
                sd::shutdown(&udid)?
            }
            println!("{command} {udid}");
            Ok(())
        }
        "apps" => {
            let args = parse_args(rest, &["--udid"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            let apps = sd::list_apps(&udid)?;
            if args.bool("--json") {
                let list: Vec<_> = apps
                    .iter()
                    .map(|a| serde_json::json!({"bundleId": a.bundle_id, "name": a.name, "appType": a.app_type}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for app in apps {
                    println!("{:<40} {:<12} {}", app.bundle_id, app.app_type, app.name);
                }
            }
            Ok(())
        }
        "agent" => agent_command(rest),
        "launch" | "terminate" | "open-url" => {
            let args = parse_args(rest, &["--udid"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            let what = args.word(0, "an argument")?;
            match command.as_str() {
                "launch" => sd::launch(&udid, &what)?,
                "terminate" => sd::terminate(&udid, &what)?,
                _ => sd::open_url(&udid, &what)?,
            }
            println!("{command} {what}");
            Ok(())
        }
        "clipboard" => {
            let args = parse_args(rest, &["--udid"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            match args.word(0, "get or set")?.as_str() {
                "get" => {
                    let text = sd::clipboard_read(&udid)?;
                    println!("{text}");
                    Ok(())
                }
                "set" => {
                    let text = args.word(1, "the text")?;
                    sd::clipboard_write(&udid, &text)?;
                    println!("clipboard set");
                    Ok(())
                }
                _ => Err(CliError::Usage(
                    "clipboard get, or clipboard set <text>".into(),
                )),
            }
        }
        "set" => set_command(rest),
        "notify" => {
            let args = parse_args(rest, &["--udid"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            let bundle = args.word(0, "a bundle id")?;
            let payload = args.word(1, "the APNs JSON payload")?;
            let value: serde_json::Value = serde_json::from_str(&payload)
                .map_err(|e| format!("the payload is not JSON: {e}"))?;
            if !value.is_object() {
                return Err("the payload must be a JSON object".into());
            }
            let path = state_dir()?.join(format!("push-{}.json", std::process::id()));
            std::fs::write(&path, value.to_string())?;
            let result = sd::push_notification(&udid, &bundle, &path.to_string_lossy());
            let _ = std::fs::remove_file(&path);
            result?;
            println!("notified {bundle}");
            Ok(())
        }
        "reset" => {
            let args = parse_args(rest, &["--udid"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            match args.word(0, "app-data or keychain")?.as_str() {
                "app-data" => {
                    let bundle = args.word(1, "a bundle id")?;
                    sd::clear_app_data(&udid, &bundle)?;
                    println!("erased {bundle}'s data");
                    Ok(())
                }
                "keychain" => {
                    sd::reset_keychain(&udid)?;
                    println!("reset the keychain");
                    Ok(())
                }
                _ => Err(CliError::Usage(
                    "reset app-data <bundle>, or reset keychain".into(),
                )),
            }
        }
        _ => Err(CliError::Usage(format!("unknown command {command}"))),
    }
}

fn set_command(rest: &[String]) -> Result<(), CliError> {
    let args = parse_args(rest, &["--udid"])?;
    let udid = pick_booted(args.flag("--udid"))?;
    match args.word(0, "what to set")?.as_str() {
        "permission" => {
            let op = args.word(1, "grant, revoke or reset")?;
            let service = args.word(2, "a service")?;
            let bundle = args.word(3, "a bundle id")?;
            match op.as_str() {
                "grant" => sd::grant_permission(&udid, &service, &bundle)?,
                "revoke" => sd::revoke_permission(&udid, &service, &bundle)?,
                "reset" => sd::reset_permission(&udid, &service, &bundle)?,
                _ => {
                    return Err(CliError::Usage(
                        "permission takes grant, revoke or reset".into(),
                    ));
                }
            }
            println!("{op} {service} for {bundle}");
            Ok(())
        }
        "content-size" => {
            let size = args.word(1, "a size")?;
            sd::set_content_size(&udid, &size)?;
            println!("content size {size}");
            Ok(())
        }
        "appearance" => {
            let which = args.word(1, "light or dark")?;
            let appearance = match which.as_str() {
                "light" => sd::Appearance::Light,
                "dark" => sd::Appearance::Dark,
                _ => return Err(CliError::Usage("appearance takes light or dark".into())),
            };
            sd::set_appearance(&udid, appearance)?;
            println!("appearance {which}");
            Ok(())
        }
        "location" => {
            let lat: f64 = args
                .word(1, "a latitude")?
                .parse()
                .map_err(|_| "latitude must be a number")?;
            let lon: f64 = args
                .word(2, "a longitude")?
                .parse()
                .map_err(|_| "longitude must be a number")?;
            sd::set_location(&udid, lat, lon)?;
            println!("location {lat}, {lon}");
            Ok(())
        }
        "status-bar" => {
            match args.word(1, "override or clear")?.as_str() {
                "override" => sd::status_bar_override(&udid)?,
                "clear" => sd::status_bar_clear(&udid)?,
                _ => return Err(CliError::Usage("status-bar takes override or clear".into())),
            }
            println!("status bar {}", args.word(1, "")?);
            Ok(())
        }
        _ => Err(CliError::Usage(
            "set permission|content-size|appearance|location|status-bar".into(),
        )),
    }
}

fn agent_command(rest: &[String]) -> Result<(), CliError> {
    let action = rest
        .first()
        .ok_or_else(|| CliError::Usage("agent snapshot|tap|steps".into()))?;
    let rest = &rest[1..];
    match action.as_str() {
        "snapshot" => {
            let args = parse_args(rest, &["--udid", "--settle-ms", "--cols", "--image"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            let driver = start_driver(&udid)?;
            let agent = Agent::new(driver);
            if !args.bool("--fresh") {
                restore_state(&agent, &udid);
            }
            let mut opts = SnapshotOptions {
                image: args.flag("--image").is_some(),
                ..SnapshotOptions::default()
            };
            if let Some(ms) = args.flag("--settle-ms") {
                opts.settle = Duration::from_millis(
                    ms.parse()
                        .map_err(|_| "settle-ms must be a number of milliseconds")?,
                );
            }
            if let Some(cols) = args.flag("--cols") {
                opts.cols = cols.parse().map_err(|_| "cols must be a number")?;
            }
            opts.grid = args.bool("--grid");
            let snapshot = agent.snapshot(&opts).map_err(|e| e.to_string())?;
            save_state(&agent, &udid);
            if let Some(path) = args.flag("--image") {
                if let Some(png) = &snapshot.image {
                    std::fs::write(path, png)?;
                }
            }
            if args.bool("--json") {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                println!("{}", snapshot.text);
                if let Some(path) = args.flag("--image") {
                    if snapshot.image.is_some() {
                        println!("image: {path}");
                    } else {
                        println!("image: none (no frame was available)");
                    }
                }
            }
            agent.device().stop();
            Ok(())
        }
        "tap" => {
            let args = parse_args(rest, &["--udid", "--expect"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            let target = args.word(0, "a target: e4, a label, or a grid cell")?;
            let driver = start_driver(&udid)?;
            let agent = Agent::new(driver);
            if !args.bool("--fresh") {
                restore_state(&agent, &udid);
            }
            let tapped = agent
                .tap(&target, args.flag("--expect"))
                .map_err(|e| e.to_string())?;
            save_state(&agent, &udid);
            if args.bool("--json") {
                println!("{}", serde_json::to_string(&tapped)?);
            } else {
                println!(
                    "tapped {} \"{}\" at ({}, {})",
                    tapped.target, tapped.name, tapped.at[0], tapped.at[1]
                );
            }
            agent.device().stop();
            Ok(())
        }
        "steps" => {
            let args = parse_args(rest, &["--udid"])?;
            let udid = pick_booted(args.flag("--udid"))?;
            let input = args.word(0, "a JSON array of steps, or @file")?;
            let json = if let Some(file) = input.strip_prefix('@') {
                std::fs::read_to_string(file).map_err(|e| format!("could not read {file}: {e}"))?
            } else {
                input
            };
            let steps: Vec<Step> =
                serde_json::from_str(&json).map_err(|e| format!("the steps are not valid: {e}"))?;
            let driver = start_driver(&udid)?;
            let agent = Agent::new(driver);
            if !args.bool("--fresh") {
                restore_state(&agent, &udid);
            }
            let report = agent.steps(&steps);
            save_state(&agent, &udid);
            let dir = state_dir()?;
            let mut value = serde_json::to_value(&report)?;
            if let Some(shots) = value.get_mut("screenshots").and_then(|v| v.as_array_mut()) {
                for (index, (shot, taken)) in shots.iter_mut().zip(&report.screenshots).enumerate()
                {
                    let path = dir.join(format!(
                        "shot-{}-{index}-{}.png",
                        udid_short(&udid),
                        taken.target.replace(['/', ' ', '"'], "_")
                    ));
                    std::fs::write(&path, &taken.png)?;
                    shot["png"] = serde_json::Value::String(path.to_string_lossy().into());
                }
            }
            let ok = report.ok;
            println!("{}", serde_json::to_string_pretty(&value)?);
            agent.device().stop();
            if ok {
                Ok(())
            } else {
                Err(CliError::Fail(format!(
                    "step {} failed: {}",
                    report.failed.as_ref().map(|f| f.index).unwrap_or(0),
                    report
                        .failed
                        .as_ref()
                        .map(|f| f.error.clone())
                        .unwrap_or_default()
                )))
            }
        }
        _ => Err(CliError::Usage("agent snapshot|tap|steps".into())),
    }
}

/// The udid to use: the one asked for, or the single booted device — with the devices
/// listed when it is ambiguous, so the next command can be copied.
fn pick_booted(udid: Option<&str>) -> Result<String, CliError> {
    if let Some(udid) = udid {
        if udid.is_empty() {
            return Err(CliError::Usage("--udid needs a value".into()));
        }
        return Ok(udid.to_string());
    }
    let booted: Vec<Device> = sd::list_devices()?
        .into_iter()
        .filter(|device| device.state == "Booted")
        .collect();
    match booted.as_slice() {
        [one] => Ok(one.udid.clone()),
        [] => Err("no simulator is booted; boot one with: simdriver boot <udid>".into()),
        many => {
            let list = many
                .iter()
                .map(|device| format!("  {}  {}", device.udid, device.name))
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!("more than one simulator is booted; pass --udid:\n{list}").into())
        }
    }
}

fn start_driver(udid: &str) -> Result<sd::Driver, CliError> {
    let options = sd::Options {
        device: DeviceSelector::Udid(udid.to_string()),
        ..sd::Options::default()
    };
    Ok(sd::Driver::start(options, |_| {})?)
}

fn state_dir() -> Result<PathBuf, CliError> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or("could not find a cache directory (no HOME)")?;
    let dir = base.join("simdriver");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn state_file(udid: &str) -> Result<PathBuf, CliError> {
    Ok(state_dir()?.join(format!("agent-{}.json", udid_short(udid))))
}

fn udid_short(udid: &str) -> String {
    udid.chars().take(8).collect()
}

fn restore_state(agent: &Agent<sd::Driver>, udid: &str) {
    let Ok(path) = state_file(udid) else { return };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    match serde_json::from_str(&text) {
        Ok(state) => agent.restore(state),
        Err(_) => {} // a state from another version: start fresh rather than fail
    }
}

fn save_state(agent: &Agent<sd::Driver>, udid: &str) {
    let Ok(path) = state_file(udid) else { return };
    let Ok(json) = serde_json::to_string(&agent.state()) else {
        return;
    };
    let _ = std::fs::write(path, json);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_words_split() {
        let args = parse_args(
            &[
                "tap".into(),
                "e4".into(),
                "--expect".into(),
                "Log in".into(),
                "--json".into(),
            ],
            &["--expect"],
        )
        .unwrap();
        assert_eq!(args.words, ["tap", "e4"]);
        assert_eq!(args.flag("--expect"), Some("Log in"));
        assert!(args.bool("--json"));
    }

    #[test]
    fn unknown_flag_refused() {
        assert!(parse_args(&["--nope".into()], &["--udid"]).is_err());
    }

    #[test]
    fn value_flag_must_apply() {
        assert!(parse_args(&["devices".into(), "--expect".into(), "x".into()], &[]).is_err());
    }
}
