//! `--list-speakers` end to end: the real binary searches, prints and
//! exits by itself. The search is kept to this PC (loopback), with a
//! scratch folder as its settings (`APPDATA`) and log (`LOCALAPPDATA`)
//! folders, so it neither reaches other devices nor reads a real
//! config.json.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn list_speakers_searches_prints_and_exits() {
    let profile = std::env::temp_dir().join(format!("sts-list-speakers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    std::fs::create_dir_all(&profile).unwrap();
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_stream-to-speaker"))
        .args(["--list-speakers", "--advertise-ip", "127.0.0.1", "--no-airplay", "--no-sendspin"])
        .env("APPDATA", profile.join("Roaming"))
        .env("LOCALAPPDATA", profile.join("Local"))
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("running stream-to-speaker --list-speakers");
    let _ = std::fs::remove_dir_all(&profile);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Either speakers (something on this PC answered), one line each, or
    // the advice for when there are none.
    assert!(
        stdout.lines().all(|l| l.starts_with("  [")) && (!stdout.is_empty() || stderr.contains("No speakers found")),
        "stdout: {stdout:?}\nstderr: {stderr:?}"
    );
    // Info-level logging stays out of a command whose output is the list.
    assert!(!stderr.contains(" INFO "), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(60), "took {:?}", started.elapsed());
}
