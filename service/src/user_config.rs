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
use std::collections::HashMap;
use std::path::PathBuf;

use crate::airplay::hap_pairing::PairingCredentials;

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Unix seconds until which the donation prompt stays hidden.
    /// `None` = never shown yet (show it). Set 7 days out by "Remind me
    /// later", and far out once the user has followed the donate link —
    /// we can't know whether they actually donated, so the honest
    /// assumption is that anyone who clicked through shouldn't be asked
    /// again for a long while.
    #[serde(default)]
    pub donation_prompt_hidden_until: Option<u64>,
    /// Whether to auto-reconnect to `last_speaker_id` on launch.
    /// `true` (default) preserves the prior behaviour. `false` lets
    /// a user keep their saved speaker remembered (so the GUI knows
    /// what to highlight, the Forget button has something to clear)
    /// without auto-binding at startup.
    #[serde(default = "default_auto_reconnect")]
    pub auto_reconnect_on_launch: bool,
    /// AirPlay 2 stream-mode experiment switch: `true` skips buffered
    /// (type 103) and uses the low-latency realtime stream (type 96)
    /// even on receivers that advertise buffered support. Realtime is
    /// architecturally ~250 ms vs buffered's 1-2 s, but some receivers
    /// (current Sonos fw) appear to only actually *play* buffered.
    /// Edit config.json by hand to flip; no GUI yet.
    #[serde(default)]
    pub prefer_realtime_airplay: bool,
    /// Auto-reconnect when a live session drops mid-stream (speaker
    /// rebooted, Wi-Fi blip, receiver reaped the session). One retry per
    /// drop, 5 s after detection — OwnTone's field-proven policy (its 5 s
    /// spacing also respects the Sonos half-open-session hold). The dead
    /// session is torn down either way so the UI never shows a zombie
    /// "streaming" state.
    #[serde(default = "default_auto_reconnect")]
    pub auto_reconnect_on_drop: bool,
    /// RAOP `et=4` MFi-encryption experiment switch. iTunes encrypts
    /// audio to et=4 receivers with a key wrapped via the auth-setup
    /// ECDH secret; our wrap is a best-grounded guess (no open-source
    /// reference exists) whose failure can wedge the receiver for tens
    /// of seconds, so the attempt is opt-in. When enabled the session
    /// tries MFi first and falls back to plaintext/RSA on failure.
    #[serde(default)]
    pub airplay_mfi_encryption: bool,
    /// Per-device AirPlay passwords for `pw=true` receivers, keyed by the
    /// device's stable id (`airplay:<mac>`). Stored so the user only
    /// enters it once. Plain-text in the config file (same trust level as
    /// the rest of the file); RTSP Digest never sends it in the clear.
    #[serde(default)]
    pub airplay_passwords: HashMap<String, String>,
    /// Per-device HomeKit **persistent** pairing credentials, keyed by the
    /// device's stable id (`airplay:<mac>`). Stored after a one-time PIN
    /// pair-setup with an AP2 receiver that refuses transient pairing (an
    /// Apple TV with access control / "require device verification"), so
    /// later connects skip straight to pair-verify. Holds our controller
    /// Ed25519 seed + the accessory's long-term public key — same trust
    /// level as the rest of the file; the seed is a per-device identity,
    /// not a reusable secret elsewhere.
    #[serde(default)]
    pub airplay_pairings: HashMap<String, PairingCredentials>,
    /// Persistent per-install HomeKit controller identity (pairing id +
    /// Ed25519 seed, hex), minted by the first PIN ceremony and reused
    /// for every later pair-setup: HAP accessories key stored pairings on
    /// the controller id, so a stable identity makes re-pairing REPLACE
    /// the record instead of consuming another of the accessory's finite
    /// pairing slots.
    #[serde(default)]
    pub airplay_controller_id: Option<String>,
    #[serde(default)]
    pub airplay_controller_seed_hex: Option<String>,
    /// Catch-all preserving config keys this binary doesn't know about
    /// (a newer version's settings) across load→save round-trips. Without
    /// it, running an older build once silently strips them — which for
    /// `airplay_pairings`-class data costs the user an on-screen PIN
    /// ceremony per device to recreate. (Protects downgrades from
    /// versions AFTER this one; releases before it still strip.)
    #[serde(flatten)]
    pub unknown_keys: serde_json::Map<String, serde_json::Value>,
    /// Forward Windows' "now playing" (title/artist/album from the System
    /// Media Transport Controls) to the speaker as track metadata, so it
    /// shows on the speaker's display / app. **Off by default** — it's a
    /// nicety, it reads whatever app currently has media focus, and the
    /// RAOP metadata path is best-effort (a receiver that ignores it is
    /// harmless). RAOP only for now (Sonos-class); no AP2 metadata yet.
    #[serde(default)]
    pub forward_now_playing: bool,
    /// Debug escape hatch: send the uncompressed-ALAC escape frames
    /// instead of real compressed ALAC. Every field-proven sender
    /// (iTunes, OwnTone, AirConnect) sends compressed; this exists only
    /// to A/B against receivers that misbehave with the encoder.
    /// Edit config.json by hand to flip; no GUI.
    #[serde(default)]
    pub airplay_uncompressed_alac: bool,
    /// AirPlay sender-side buffer, in milliseconds: how far behind the RTP
    /// write head our sync packets place "now", i.e. how much audio the
    /// receiver holds before playing it. This is the bulk of the delay
    /// heard on AirPlay, so lowering it lowers the delay in step, at the
    /// cost of dropout margin (any hiccup longer than the buffer is a
    /// dropout). 2000 = what iTunes sends, proven on every receiver
    /// class. A modern Apple TV accepts almost anything; AirPort Express
    /// and most AirPlay speakers need ~100-350 ms. The receiver's own
    /// advertised `Audio-Latency` is always honoured as a floor, so a
    /// speaker that reports its minimum is never driven below it.
    /// Applies to RAOP and the AirPlay 2 realtime stream; below 1000 ms
    /// an AirPlay 2 receiver is driven realtime automatically, since the
    /// buffered stream holds seconds regardless of what we ask. Clamped
    /// to [`AIRPLAY_LATENCY_MS_MIN`]..=[`AIRPLAY_LATENCY_MS_MAX`] on read.
    #[serde(default = "default_airplay_latency_ms")]
    pub airplay_latency_ms: u32,
}

/// Default AirPlay buffer — iTunes' 2 s (88200 samples at 44.1 kHz).
pub const AIRPLAY_LATENCY_MS_DEFAULT: u32 = 2000;
/// Floor for `airplay_latency_ms`. Below ~20 ms the sender's own pacing
/// jitter (8 ms RTP packets fed by 10 ms driver frames) exceeds the
/// receiver's margin, so smaller values only add dropouts.
pub const AIRPLAY_LATENCY_MS_MIN: u32 = 20;
/// Ceiling for `airplay_latency_ms` (3 s — "buffered" territory).
pub const AIRPLAY_LATENCY_MS_MAX: u32 = 3000;

fn default_airplay_latency_ms() -> u32 {
    AIRPLAY_LATENCY_MS_DEFAULT
}

/// Fresh-install defaults come from the same serde defaults a parsed
/// config uses, so a field's `#[serde(default = ...)]` is its single
/// source of truth. (The previous `#[derive(Default)]` zeroed every
/// field on a first launch — `auto_reconnect_on_drop` documented as
/// default-true came up false, and was then persisted false by the first
/// save. A non-zero default like `airplay_latency_ms` makes that
/// divergence unaffordable.)
impl Default for UserConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("every UserConfig field has a serde default")
    }
}

fn default_auto_reconnect() -> bool {
    true
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
    /// `airplay_latency_ms` clamped to its supported range, so a hand-
    /// edited config (or an older file with the field absent → default)
    /// can't produce a zero or absurd anchor.
    pub fn effective_airplay_latency_ms(&self) -> u32 {
        self.airplay_latency_ms.clamp(AIRPLAY_LATENCY_MS_MIN, AIRPLAY_LATENCY_MS_MAX)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_defaults_match_serde_defaults() {
        // A first launch (no config file) must see the documented
        // defaults, not zeroed fields.
        let c = UserConfig::default();
        assert_eq!(c.airplay_latency_ms, AIRPLAY_LATENCY_MS_DEFAULT);
        assert!(c.auto_reconnect_on_launch);
        assert!(c.auto_reconnect_on_drop);
        assert!(!c.prefer_realtime_airplay);
    }

    #[test]
    fn latency_ms_absent_or_absurd_is_clamped() {
        // Older config file without the field → default.
        let c: UserConfig = serde_json::from_str(r#"{"onboarding_dismissed":true}"#).unwrap();
        assert_eq!(c.effective_airplay_latency_ms(), AIRPLAY_LATENCY_MS_DEFAULT);
        // Hand-edited nonsense is clamped, never zero.
        let c: UserConfig = serde_json::from_str(r#"{"airplay_latency_ms":0}"#).unwrap();
        assert_eq!(c.effective_airplay_latency_ms(), AIRPLAY_LATENCY_MS_MIN);
        let c: UserConfig = serde_json::from_str(r#"{"airplay_latency_ms":999999}"#).unwrap();
        assert_eq!(c.effective_airplay_latency_ms(), AIRPLAY_LATENCY_MS_MAX);
        // In-range values pass through.
        let c: UserConfig = serde_json::from_str(r#"{"airplay_latency_ms":100}"#).unwrap();
        assert_eq!(c.effective_airplay_latency_ms(), 100);
    }
}
