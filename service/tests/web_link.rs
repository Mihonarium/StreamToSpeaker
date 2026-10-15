//! `--web-link` end to end: the real binary prints the web UI's link and
//! exits, with the same key every time, and the key goes nowhere but its
//! standard output and the settings that keep it (sealed there on
//! Windows). Each run gets a scratch folder as its settings
//! (`APPDATA`) and log (`LOCALAPPDATA`) folders, so no real config.json is
//! read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FLAGS: &[&str] = &["--advertise-ip", "192.0.2.10", "--port", "5999"];

/// A fresh scratch folder standing in for an account's profile.
fn profile(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sts-web-link-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn web_link(profile: &Path, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stream-to-speaker"))
        .arg("--web-link")
        .args(flags)
        .env("APPDATA", profile.join("Roaming"))
        .env("LOCALAPPDATA", profile.join("Local"))
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("running stream-to-speaker --web-link")
}

/// Standard output, which must be the link alone on one line (scripts take
/// it as it is), and the link's key.
fn link_and_key(out: &Output, prefix: &str) -> (String, String) {
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    let link = stdout.trim_end();
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    let key = link
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix('/'))
        .unwrap_or_else(|| panic!("not a link under {prefix}: {stdout:?}"));
    assert!(key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit()), "{stdout:?}");
    (link.to_string(), key.to_string())
}

/// Every file under `dir`, read as text.
fn files_under(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
            out.push((path, text));
        }
    }
    out
}

#[test]
fn prints_the_same_link_every_time_and_writes_its_key_nowhere_else() {
    let profile = profile("same");
    let first = web_link(&profile, FLAGS);
    let (link, key) = link_and_key(&first, "http://192.0.2.10:5999/r/");
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(stderr.contains("Anyone with this link"), "{stderr}");
    assert!(!stderr.contains(&key), "{stderr}");

    // The key is kept in the settings (sealed, on Windows), and the log
    // folder never sees it.
    let config = profile.join("Roaming").join("StreamToSpeaker").join("config.json");
    let saved = std::fs::read_to_string(&config).expect("config.json written");
    assert!(saved.contains("web_secret"), "{saved}");
    #[cfg(windows)]
    assert!(!saved.contains(&key), "{saved}");
    for (path, text) in files_under(&profile.join("Local")) {
        assert!(!text.contains(&key), "{}: {text}", path.display());
    }

    // Later runs print the same link; other flags change only the address.
    let again = web_link(&profile, FLAGS);
    assert_eq!(link_and_key(&again, "http://192.0.2.10:5999/r/"), (link, key.clone()));
    let elsewhere = web_link(&profile, &["--advertise-ip", "fe80::1", "--port", "8080"]);
    assert_eq!(link_and_key(&elsewhere, "http://[fe80::1]:8080/r/").1, key);
    let _ = std::fs::remove_dir_all(&profile);
}

/// The app running already (it holds the lock a second launch of the app
/// yields to) doesn't stop `--web-link`: that is when the link is wanted.
#[cfg(windows)]
#[test]
fn prints_the_link_while_the_app_is_running() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::CreateMutexA;
    let name = b"Global\\StreamToSpeaker.Singleton\0";
    let held = unsafe { CreateMutexA(std::ptr::null(), 0, name.as_ptr()) };
    assert!(!held.is_null());
    let profile = profile("running");
    let out = web_link(&profile, FLAGS);
    unsafe { CloseHandle(held) };
    link_and_key(&out, "http://192.0.2.10:5999/r/");
    let _ = std::fs::remove_dir_all(&profile);
}
