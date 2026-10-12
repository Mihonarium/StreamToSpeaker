//! stream-to-speaker-bridge library crate.
//!
//! The binary `stream-to-speaker-bridge` wires these modules together; the library
//! split exists so unit tests can exercise individual pieces (the silence
//! detector in particular) without booting the whole service.

pub mod airplay;
pub mod sendspin;
pub mod sendspin_app;
pub mod app;
pub mod audio_loop;
pub mod audio_source;
pub mod silence;
pub mod http_server;
pub mod sonos;
pub mod ssdp;
pub mod upnp;
pub mod gena;
pub mod volume_sync;
pub mod sine_source;
pub mod qpc;
pub mod picker;
pub mod user_config;
pub mod update_check;
pub mod stream_gate;
pub mod gui_layout;
pub mod now_playing;
#[cfg(windows)]
pub mod wasapi_source;

#[cfg(windows)]
pub mod ioctl_source;

#[cfg(windows)]
pub mod gui;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod endpoint_name;

/// Whether `url` is something we're willing to hand to the shell: an
/// absolute http(s) URL made only of letters, digits and `-._~/:%[]@`.
/// That excludes whitespace, quotes, control characters and shell
/// metacharacters (`&`, `|`, `^`, `<`, `>`); nothing we open needs a
/// query string.
pub fn is_openable_url(url: &str) -> bool {
    let rest = if let Some(r) = url.strip_prefix("https://") {
        r
    } else if let Some(r) = url.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    !rest.is_empty()
        && !rest.starts_with('/')
        && rest.len() < 2048
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~/:%[]@".contains(&b))
}

/// Open `url` in the default browser via `ShellExecuteW` — no command
/// interpreter involved. Refuses anything [`is_openable_url`] rejects.
#[cfg(windows)]
pub fn open_url(url: &str) -> std::io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if !is_openable_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to open {:?}", url),
        ));
    }
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let verb = wide("open");
    let file = wide(url);
    // ShellExecute may hand off to shell extensions that need COM; the
    // documented requirement is an apartment-threaded apartment. S_OK /
    // S_FALSE (already initialised as STA) both need a matching
    // CoUninitialize; a thread already in the MTA just proceeds.
    // SAFETY: plain COM initialisation for this thread.
    let hr = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if hr >= 0 {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    // Values above 32 mean success.
    if rc as usize > 32 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("ShellExecuteW failed ({})", rc as usize),
        ))
    }
}

#[cfg(test)]
mod url_tests {
    use super::is_openable_url;

    #[test]
    fn openable_urls() {
        assert!(is_openable_url("https://github.com/Mihonarium/StreamToSpeaker/releases/tag/v0.1.4"));
        assert!(is_openable_url("http://192.168.1.10:5901/"));
        assert!(is_openable_url("http://[fe80::1]:5901/"));
        for bad in [
            "https://github.com/x&calc",
            "https://github.com/x|calc",
            "https://github.com/x^&calc",
            "https://github.com/x\"calc",
            "https://github.com/x calc",
            "file:///C:/Windows/System32/calc.exe",
            "calc.exe",
            "https://",
            "https:///x",
            "javascript:alert(1)",
        ] {
            assert!(!is_openable_url(bad), "{bad}");
        }
    }
}

/// User-facing product name (shown in CLI help, web status page, etc).
/// Internal Rust crate name stays `stream_to_speaker` for code stability.
pub const PRODUCT_NAME: &str = "Stream To Speaker";
/// Short slug used in user agents.
pub const PRODUCT_UA: &str = "stream-to-speaker/0.1";

/// Release version CI bakes in on tag builds (`STS_RELEASE_VERSION` = the
/// git tag minus its `v`, see build.yml). Unset for local/dev builds.
const RELEASE_VERSION_ENV: Option<&str> = option_env!("STS_RELEASE_VERSION");

/// The release this exe was built for, or `None` for a dev build. NB the
/// crate version (`CARGO_PKG_VERSION`) is *not* the release version — the
/// two are versioned independently — so anything user-facing or
/// comparable against GitHub must use this.
pub fn release_version() -> Option<&'static str> {
    RELEASE_VERSION_ENV.filter(|s| !s.trim().is_empty())
}

/// Version to show people: the release tag on release builds, else the
/// crate version (dev builds).
pub fn display_version() -> &'static str {
    release_version().unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Wire format: only L16 PCM, 44.1 kHz, stereo in v1.
pub const WIRE_SAMPLE_RATE: u32 = 44_100;
/// Channels on the wire (stereo).
pub const WIRE_CHANNELS: u16 = 2;
/// Bits per sample on the wire.
pub const WIRE_BITS_PER_SAMPLE: u16 = 16;

/// Bytes per sample frame on the wire (= 4 for L16 stereo).
pub const WIRE_BYTES_PER_FRAME: usize =
    (WIRE_CHANNELS as usize) * (WIRE_BITS_PER_SAMPLE as usize / 8);

/// Per-user log directory (`%LOCALAPPDATA%\StreamToSpeaker`). Created
/// if missing. Returns None when `LOCALAPPDATA` isn't set (non-Windows
/// hosts, lint builds). Used by `main.rs` to seed the file logger and
/// by the GUI Help menu's "Open log folder" item.
pub fn log_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    let dir = std::path::PathBuf::from(base).join("StreamToSpeaker");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}
