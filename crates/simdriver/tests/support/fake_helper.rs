//! A stand-in for `sdframe` / `sdhid` in unit tests (built as the `fake_helper` example).
//!
//! Modes:
//!   echo               copy stdin to stdout chunk by chunk (one write per read), exit 0 on EOF.
//!                      Used to feed scripted sdframe lines: whatever the test writes in one
//!                      `send_line` comes back out of the helper as one burst.
//!   hid <dir>          sdhid stand-in: waits for `<dir>/go` to exist, prints
//!                      {"t":"ready","w":393,"h":852}, then appends every stdin line to `<dir>/log`.
//!                      `{"t":"_invalidate"}` prints {"t":"displaced"}; while displaced, a gesture
//!                      start (touch/touch2 down, button, key, text) prints {"t":"reclaimed"}.
//!                      `{"t":"_exit","code":N}` exits with N. Exit 0 on stdin EOF.
//!   lines <code> <l>…  print each argument as a line, then exit with <code>.
//!   stubborn           ignore SIGINT and stdin EOF; sleep until killed.
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("echo") => echo(),
        Some("hid") => hid(PathBuf::from(&args[1])),
        Some("lines") => {
            let code: i32 = args[1].parse().unwrap();
            let mut out = std::io::stdout().lock();
            for l in &args[2..] {
                writeln!(out, "{l}").unwrap();
            }
            out.flush().unwrap();
            std::process::exit(code);
        }
        Some("stubborn") => {
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
            }
            println!("{{\"t\":\"note\",\"detail\":\"stubborn\"}}");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        other => {
            eprintln!("fake_helper: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

fn echo() {
    let mut inp = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 65536];
    loop {
        match inp.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    return;
                }
            }
        }
    }
}

fn hid(dir: PathBuf) {
    while !dir.join("go").exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, r#"{{"t":"ready","w":393,"h":852}}"#).unwrap();
    out.flush().unwrap();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log")).unwrap();
    let mut displaced = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        if line.contains(r#""t":"_invalidate""#) {
            displaced = true;
            writeln!(out, r#"{{"t":"displaced"}}"#).unwrap();
            out.flush().unwrap();
            continue;
        }
        if let Some(rest) = line.strip_prefix(r#"{"t":"_exit","code":"#) {
            std::process::exit(rest.trim_end_matches('}').parse().unwrap());
        }
        let start = line.contains(r#""p":"down""#)
            || [r#""t":"button""#, r#""t":"key""#, r#""t":"text""#].iter().any(|t| line.contains(t));
        if displaced && start {
            displaced = false;
            writeln!(out, r#"{{"t":"reclaimed"}}"#).unwrap();
            out.flush().unwrap();
        }
        writeln!(log, "{line}").unwrap();
        log.flush().unwrap();
    }
}
