//! Print what an agent sees on a booted simulator, without touching it:
//! `cargo run -p simdriver-agent --example snapshot -- [udid] [out.png]`.
//! With `SIMDRIVER_AX_RAW=1`, also prints the raw accessibility tree as JSON.
use simdriver::{DeviceSelector, Driver, Options};
use simdriver_agent::{Agent, SnapshotOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let device = args.next().map_or(DeviceSelector::FirstBootedOrIphone, DeviceSelector::Udid);
    let out = args.next().unwrap_or_else(|| "snapshot.png".into());
    let driver = Driver::start(Options { device, ..Options::default() }, |_| {}).expect("start");
    if std::env::var_os("SIMDRIVER_AX_RAW").is_some() {
        let root = driver.accessibility().expect("accessibility");
        println!("{}", serde_json::to_string_pretty(&root).unwrap());
    }
    let agent = Agent::new(driver);
    let snap = agent.snapshot(&SnapshotOptions::default()).expect("snapshot");
    println!("{}", snap.text);
    if let Some(png) = &snap.image {
        std::fs::write(&out, png).expect("write image");
        eprintln!("annotated image: {out}");
    }
    agent.device().stop();
}
