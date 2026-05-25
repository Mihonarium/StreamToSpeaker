//! Persisted user preferences.
//!
//! Tiny JSON file at `%APPDATA%\StreamToSpeaker\config.json` that
//! survives across launches. Currently holds:
//!   - `last_speaker_id`: the stable id of the speaker the user last
//!     explicitly selected. Used to auto-reconnect on next launch.
//!     `None` on first launch (or after the user clicks "Forget
//!     speaker"), which is what causes the onboarding card to show.
//!   - `onboarding_dismissed`: whether the user clicked "Got it" on
//!     the onboarding card. Persisted so we don't re-show it on
//!     subsequent launches.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub last_speaker_id: Option<String>,
    #[serde(default)]
    pub onboarding_dismissed: bool,
    /// User ticked "Always minimise to tray" in the close-confirm
    /// modal. When true, future window-close events skip the modal
    /// and silently minimise. Inconsistency with onboarding_dismissed
    /// — which IS persisted — was confusing; this brings the close
    /// preference into the same store.
    #[serde(default)]
    pub always_minimise_to_tray: bool,
}

fn config_dir() -> Option<PathBuf> {
    // %APPDATA% on Windows. On non-Windows (tests, lint runs), fall
    // back to $XDG_CONFIG_HOME or $HOME/.config; this binary is
    // gated to Windows in practice but the module compiles cross-
    // platform so unit-test builds don't need a cfg fence.
    if let Ok(p) = std::env::var("APPDATA") {
        Some(PathBuf::from(p).join("StreamToSpeaker"))
    } else if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        Some(PathBuf::from(p).join("stream-to-speaker"))
    } else if let Ok(p) = std::env::var("HOME") {
        Some(PathBuf::from(p).join(".config").join("stream-to-speaker"))
    } else {
        None
    }
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.json"))
}

impl UserConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else { return Self::default(); };
        let Ok(content) = std::fs::read_to_string(&path) else { return Self::default(); };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Best-effort save. Logged-on-failure rather than propagated — a
    /// failed config write should never block UI actions like speaker
    /// selection.
    pub fn save(&self) {
        let Some(path) = config_path() else { return; };
        let Some(dir) = path.parent() else { return; };
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("user_config: mkdir {}: {}", dir.display(), e);
            return;
        }
        let content = match serde_json::to_string_pretty(self) {
            Ok(s) => s,
            Err(e) => { log::warn!("user_config: serialize: {}", e); return; }
        };
        if let Err(e) = std::fs::write(&path, content) {
            log::warn!("user_config: write {}: {}", path.display(), e);
        }
    }
}
