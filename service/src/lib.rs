//! stream-to-speaker-bridge library crate.
//!
//! The binary `stream-to-speaker-bridge` wires these modules together; the library
//! split exists so unit tests can exercise individual pieces (the silence
//! detector in particular) without booting the whole service.

pub mod airplay;
pub mod app;
pub mod audio_loop;
pub mod audio_source;
pub mod silence;
pub mod http_server;
pub mod ssdp;
pub mod upnp;
pub mod gena;
pub mod volume_sync;
pub mod sine_source;
pub mod qpc;
pub mod picker;
pub mod user_config;
pub mod update_check;
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
