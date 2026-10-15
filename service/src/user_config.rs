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
//!
//! Secrets (AirPlay passwords, pairing credentials, the controller seed;
//! Sendspin identity keys and pairing PSKs) are sealed with DPAPI into a
//! separate `protected` section of the file and plain in memory; see
//! `secret_store`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::airplay::hap_pairing::PairingCredentials;
use crate::secret_store::{self, LockedNode, PlatformCodec, SecretCodec};
use crate::discovery_net::SavedAdapter;
use crate::manual_speakers::ManualSpeaker;
use crate::equalizer::EqSettings;

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
    /// enters it once. Stored sealed in the file's `protected` section
    /// (plain in memory); RTSP Digest never sends it in the clear.
    #[serde(default)]
    pub airplay_passwords: HashMap<String, String>,
    /// Per-device HomeKit **persistent** pairing credentials, keyed by the
    /// device's stable id (`airplay:<mac>`). Stored after a one-time PIN
    /// pair-setup with an AP2 receiver that refuses transient pairing (an
    /// Apple TV with access control / "require device verification"), so
    /// later connects skip straight to pair-verify. Holds our controller
    /// Ed25519 seed + the accessory's long-term public key; each entry is
    /// stored sealed in the file's `protected` section.
    #[serde(default)]
    pub airplay_pairings: HashMap<String, PairingCredentials>,
    /// Persistent per-install HomeKit controller identity (pairing id +
    /// Ed25519 seed, hex), minted by the first PIN ceremony and reused
    /// for every later pair-setup: HAP accessories key stored pairings on
    /// the controller id, so a stable identity makes re-pairing REPLACE
    /// the record instead of consuming another of the accessory's finite
    /// pairing slots. The seed is stored sealed in the file's `protected`
    /// section.
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
    /// Stored secrets this account couldn't unseal (file copied from
    /// another machine or user). Kept out of the live settings, written
    /// back unchanged on every save so they're never lost.
    #[serde(skip)]
    pub locked_secrets: Vec<LockedNode>,
    /// `config.json` exists but couldn't be read, or couldn't be moved
    /// aside when it was unusable: these are defaults, not its contents,
    /// so saves leave the file alone for the rest of the run.
    #[serde(skip)]
    keep_file: bool,
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
    /// Privacy mode: only serve `/stream.raw` (the system-audio stream)
    /// to the speaker we're currently streaming to. Without it, anyone
    /// on the LAN who knows the URL can listen to everything the PC
    /// plays. **Off by default** — it can break grouped Sonos playback
    /// (the group's coordinator, which may be a *different* unit than
    /// the selected one, is what fetches the stream) and any other
    /// setup where the fetching IP differs from the selected speaker's.
    #[serde(default)]
    pub privacy_mode: bool,
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
    /// HomePods (single, `model AudioAccessory*`) normally stream with the
    /// low-latency realtime profile at [`Self::airplay_homepod_latency_ms`].
    /// `true` returns single HomePods to the earlier buffered AirPlay 2
    /// stream (seconds of delay, governed by `airplay_latency_ms` and
    /// `prefer_realtime_airplay`). Stereo pairs always use realtime.
    #[serde(default)]
    pub airplay_homepod_buffered: bool,
    /// Playout delay requested from HomePods on the realtime profile, in
    /// ms (the stream's pinned `latencyMin`/`latencyMax`). Clamped to
    /// [`HOMEPOD_LATENCY_MS_MIN`]..=[`HOMEPOD_LATENCY_MS_MAX`] on read.
    #[serde(default = "default_homepod_latency_ms")]
    pub airplay_homepod_latency_ms: u32,
    /// Once-a-day check for a newer release on GitHub (Advanced toggle).
    /// One unauthenticated GET of the releases API; nothing is downloaded
    /// or installed. On by default. See `update_check`.
    #[serde(default = "default_true")]
    pub check_for_updates: bool,
    /// Unix seconds of the last completed update check, any outcome —
    /// paces the automatic check across launches.
    #[serde(default)]
    pub update_last_check_unix: Option<u64>,
    /// Newest release seen by the last successful check: tag + release
    /// page. Cached so the banner is right from the first frame after
    /// launch (the check itself runs 20 s in) and on an offline launch.
    #[serde(default)]
    pub update_latest_tag: Option<String>,
    #[serde(default)]
    pub update_latest_url: Option<String>,
    /// Tag the user chose "Skip this version" on — the banner stays
    /// hidden until a *different* newer release appears.
    #[serde(default)]
    pub update_skipped_tag: Option<String>,
    /// "Later" on the update banner: hidden until this unix time.
    #[serde(default)]
    pub update_banner_hidden_until: Option<u64>,
    /// Sendspin (Music Assistant) identities, pairing records and
    /// settings — see `sendspin::store`. The identities and pairing
    /// records are stored sealed in the file's `protected` section.
    #[serde(default)]
    pub sendspin: crate::sendspin::store::SendspinConfig,
    /// Speakers the user added by address (see `manual_speakers`), in the
    /// order they were added.
    #[serde(default)]
    pub manual_speakers: Vec<ManualSpeaker>,
    /// The one network adapter speaker discovery runs on; `None` = every
    /// adapter (see `discovery_net`).
    #[serde(default)]
    pub discovery_adapter: Option<SavedAdapter>,
    /// Per-speaker overrides keyed by speaker id. Only speakers with a
    /// non-default setting have an entry.
    #[serde(default)]
    pub speaker_settings: HashMap<String, SpeakerSettings>,
    /// Ten-band equalizer applied to everything streamed (all output
    /// types). Off by default; disabling keeps the curve. A malformed
    /// entry falls back to the default rather than failing the file.
    #[serde(default, deserialize_with = "crate::equalizer::deserialize_lenient")]
    pub equalizer: EqSettings,
}

/// Whether the app connects to a speaker by itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoConnect {
    /// Follow the app-wide setting (reconnect to the last-used speaker at
    /// launch when that is on).
    #[default]
    Default,
    /// Connect when it shows up and nothing else is playing.
    Always,
    /// Never connect without a click, not even at launch.
    Never,
}

/// Settings that apply to one speaker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerSettings {
    /// Left out of the speaker list (and the web API's), unless it's the
    /// one playing. Shown again with "Show hidden".
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub auto_connect: AutoConnect,
    /// AirPlay buffer for this speaker, overriding `airplay_latency_ms`.
    #[serde(default)]
    pub airplay_latency_ms: Option<u32>,
}

impl SpeakerSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Default AirPlay buffer — iTunes' 2 s (88200 samples at 44.1 kHz).
pub const AIRPLAY_LATENCY_MS_DEFAULT: u32 = 2000;
/// Floor for `airplay_latency_ms`. Below ~20 ms the sender's own pacing
/// jitter (8 ms RTP packets fed by 10 ms driver frames) exceeds the
/// receiver's margin, so smaller values only add dropouts.
pub const AIRPLAY_LATENCY_MS_MIN: u32 = 20;
/// Ceiling for `airplay_latency_ms` (3 s — "buffered" territory).
pub const AIRPLAY_LATENCY_MS_MAX: u32 = 3000;

/// Default HomePod realtime delay.
pub const HOMEPOD_LATENCY_MS_DEFAULT: u32 = 200;
/// Floor for `airplay_homepod_latency_ms` (same pacing argument as
/// [`AIRPLAY_LATENCY_MS_MIN`]).
pub const HOMEPOD_LATENCY_MS_MIN: u32 = 20;
/// Ceiling for `airplay_homepod_latency_ms`.
pub const HOMEPOD_LATENCY_MS_MAX: u32 = 2000;

fn default_homepod_latency_ms() -> u32 {
    HOMEPOD_LATENCY_MS_DEFAULT
}

fn default_true() -> bool {
    true
}

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
    /// Defaults that `save` never writes, so tests can run code that saves
    /// the config without touching the developer's real file.
    #[cfg(test)]
    pub fn unsaved() -> Self {
        Self { keep_file: true, ..Self::default() }
    }

    /// Clear a device's stored password, including a sealed copy this
    /// account can't open (otherwise the next save would keep it).
    pub fn remove_airplay_password(&mut self, id: &str) {
        self.airplay_passwords.remove(id);
        secret_store::forget(
            &mut self.locked_secrets,
            &["airplay_passwords".to_string(), id.to_string()],
        );
    }

    /// Remove a device's stored pairing, including a sealed copy this
    /// account can't open. Returns whether a usable one was removed.
    pub fn remove_airplay_pairing(&mut self, id: &str) -> bool {
        secret_store::forget(
            &mut self.locked_secrets,
            &["airplay_pairings".to_string(), id.to_string()],
        );
        self.airplay_pairings.remove(id).is_some()
    }

    /// `airplay_latency_ms` clamped to its supported range, so a hand-
    /// edited config (or an older file with the field absent → default)
    /// can't produce a zero or absurd anchor.
    pub fn effective_airplay_latency_ms(&self) -> u32 {
        self.airplay_latency_ms.clamp(AIRPLAY_LATENCY_MS_MIN, AIRPLAY_LATENCY_MS_MAX)
    }

    /// `airplay_homepod_latency_ms` clamped to its supported range.
    pub fn effective_homepod_latency_ms(&self) -> u32 {
        self.airplay_homepod_latency_ms.clamp(HOMEPOD_LATENCY_MS_MIN, HOMEPOD_LATENCY_MS_MAX)
    }

    /// The AirPlay buffer for speaker `id`: its own override if set, else
    /// the app-wide value; clamped either way.
    pub fn airplay_latency_ms_for(&self, id: &str) -> u32 {
        match self.speaker_settings.get(id).and_then(|s| s.airplay_latency_ms) {
            Some(ms) => ms.clamp(AIRPLAY_LATENCY_MS_MIN, AIRPLAY_LATENCY_MS_MAX),
            None => self.effective_airplay_latency_ms(),
        }
    }

    /// The HomePod delay for speaker `id`: its own AirPlay buffer override
    /// if set, else the app-wide HomePod delay; clamped either way.
    pub fn homepod_latency_ms_for(&self, id: &str) -> u32 {
        match self.speaker_settings.get(id).and_then(|s| s.airplay_latency_ms) {
            Some(ms) => ms.clamp(HOMEPOD_LATENCY_MS_MIN, HOMEPOD_LATENCY_MS_MAX),
            None => self.effective_homepod_latency_ms(),
        }
    }

    pub fn load() -> Self {
        let Some(path) = config_path() else { return Self::default(); };
        Self::load_from(&path)
    }

    /// Load from `path`. A missing file is a fresh install. A file that
    /// exists but isn't valid JSON (or isn't text in an encoding
    /// `decode_config_text` reads) is **quarantined** — renamed to
    /// `config.json.corrupt-<unix time>` beside the original — and the
    /// defaults are used. The old behaviour was `unwrap_or_default()`,
    /// which silently reset everything and then let the next save
    /// overwrite the broken file; `airplay_pairings` alone costs a PIN
    /// ceremony per Apple TV to recreate, so the data is worth keeping
    /// for manual recovery. A file that can't be read at all (or moved
    /// aside) is never overwritten by this run's saves.
    pub fn load_from(path: &std::path::Path) -> Self {
        Self::load_from_with(path, &PlatformCodec)
    }

    /// [`load_from`](Self::load_from) with an explicit secret protector.
    /// Unseals protected values; if the file still had plain secrets
    /// (written before protection existed, or by an older version since)
    /// and the protector can seal, saves right away so they don't stay
    /// readable on disk. A file in another encoding than the UTF-8 saves
    /// write is rewritten right away too: older versions read only that.
    pub fn load_from_with(path: &std::path::Path, codec: &dyn SecretCodec) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::error!(
                    "user_config: can't read {}: {} — using defaults; the file is left as is \
                     and settings changed in this run are not saved",
                    path.display(),
                    e
                );
                return Self { keep_file: true, ..Self::default() };
            }
        };
        let parsed = decode_config_text(&bytes, decode_ansi).and_then(|(json, encoding)| {
            serde_json::from_str::<serde_json::Value>(&json)
                .and_then(|mut tree| {
                    let report = secret_store::unseal(&mut tree, codec);
                    serde_json::from_value::<UserConfig>(tree).map(|c| (c, report, encoding))
                })
                .map_err(|e| format!("not valid JSON: {e}"))
        });
        match parsed {
            Ok((mut c, report, encoding)) => {
                c.locked_secrets = report.locked;
                if let Some(encoding) = encoding {
                    log::info!("user_config: rewriting {} ({encoding}) as UTF-8", path.display());
                    c.save_to_with(path, codec);
                } else if report.needs_save && codec.can_protect() {
                    log::info!(
                        "user_config: protecting stored secrets written unprotected by an \
                         earlier version"
                    );
                    c.save_to_with(path, codec);
                }
                c
            }
            Err(e) => {
                let aside = quarantine_path(path);
                match std::fs::rename(path, &aside) {
                    Ok(()) => log::error!(
                        "user_config: can't use {} ({}); moved it to {} and starting with \
                         defaults",
                        path.display(),
                        e,
                        aside.display()
                    ),
                    Err(re) => {
                        log::error!(
                            "user_config: can't use {} ({}) and could not move it aside ({}); \
                             starting with defaults — the file is left as is and settings \
                             changed in this run are not saved",
                            path.display(),
                            e,
                            re
                        );
                        return Self { keep_file: true, ..Self::default() };
                    }
                }
                Self::default()
            }
        }
    }

    /// Best-effort save. Logged-on-failure rather than propagated — a
    /// failed config write should never block UI actions like speaker
    /// selection.
    pub fn save(&self) {
        let Some(path) = config_path() else { return; };
        self.save_to(&path);
    }

    /// Atomic replace: serialise to a sibling temp file, then rename it
    /// over the real one. A crash or power cut mid-write can then never
    /// leave a truncated `config.json` (which `load_from` would have to
    /// quarantine). `rename` replaces an existing file on both Windows
    /// and Unix.
    pub fn save_to(&self, path: &std::path::Path) {
        self.save_to_with(path, &PlatformCodec)
    }

    /// [`save_to`](Self::save_to) with an explicit secret protector.
    pub fn save_to_with(&self, path: &std::path::Path, codec: &dyn SecretCodec) {
        if self.keep_file {
            log::warn!(
                "user_config: not saving over {}: it couldn't be read at startup",
                path.display()
            );
            return;
        }
        let Some(dir) = path.parent() else { return; };
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("user_config: mkdir {}: {}", dir.display(), e);
            return;
        }
        let content = match serde_json::to_value(self).and_then(|mut tree| {
            secret_store::seal(&mut tree, &self.locked_secrets, codec);
            serde_json::to_string_pretty(&tree)
        }) {
            Ok(s) => s,
            Err(e) => { log::warn!("user_config: serialize: {}", e); return; }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, content) {
            log::warn!("user_config: write {}: {}", tmp.display(), e);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            log::warn!("user_config: replace {}: {}", path.display(), e);
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The text of a config file, and the name of its encoding when it isn't
/// the UTF-8 without a byte-order mark that saves write. A file edited by
/// hand may also be UTF-8 with one (Windows PowerShell 5 `Set-Content
/// -Encoding UTF8`), UTF-16 with one (its `Out-File` and `>`), or in the
/// system's ANSI code page (its `Set-Content`), which `ansi` decodes.
fn decode_config_text(
    bytes: &[u8],
    ansi: fn(&[u8]) -> Option<String>,
) -> Result<(String, Option<&'static str>), String> {
    fn utf16(b: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, String> {
        if b.len() % 2 != 0 {
            return Err("odd length for UTF-16".into());
        }
        let units: Vec<u16> = b.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
        String::from_utf16(&units).map_err(|e| format!("bad UTF-16: {e}"))
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes).map(|t| (t, Some("UTF-16LE")));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes).map(|t| (t, Some("UTF-16BE")));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return String::from_utf8(rest.to_vec())
            .map(|t| (t, Some("UTF-8 with a byte-order mark")))
            .map_err(|e| format!("not UTF-8: {e}"));
    }
    match String::from_utf8(bytes.to_vec()) {
        Ok(t) => Ok((t, None)),
        Err(e) => match ansi(bytes) {
            Some(t) => Ok((t, Some("ANSI code page"))),
            None => Err(format!("not UTF-8: {e}")),
        },
    }
}

/// Text in the system's ANSI code page; `None` when the bytes aren't
/// valid in it (or off Windows).
#[cfg(windows)]
fn decode_ansi(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MB_ERR_INVALID_CHARS};
    let len = i32::try_from(bytes.len()).ok().filter(|n| *n > 0)?;
    // SAFETY: the input pointer and length describe `bytes`; the second
    // call writes at most `n` units into a buffer of that size.
    let n = unsafe {
        MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes.as_ptr(), len, std::ptr::null_mut(), 0)
    };
    if n <= 0 {
        return None;
    }
    let mut wide = vec![0u16; n as usize];
    let written = unsafe {
        MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes.as_ptr(), len, wide.as_mut_ptr(), n)
    };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    String::from_utf16(&wide).ok()
}

#[cfg(not(windows))]
fn decode_ansi(_: &[u8]) -> Option<String> {
    None
}

/// `config.json` → `config.json.corrupt-<unix seconds>` in the same dir.
fn quarantine_path(path: &std::path::Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.json".into());
    path.with_file_name(format!("{name}.corrupt-{secs}"))
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
        assert!(c.check_for_updates);
        assert_eq!(c.update_last_check_unix, None);
        assert!(!c.prefer_realtime_airplay);
        assert!(!c.airplay_homepod_buffered);
        assert_eq!(c.effective_homepod_latency_ms(), HOMEPOD_LATENCY_MS_DEFAULT);
    }

    fn temp_config_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sts-user-config-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_temp_file() {
        let dir = temp_config_dir("roundtrip");
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.last_speaker_id = Some("uuid:RINCON_TEST".into());
        c.airplay_latency_ms = 250;
        c.save_to(&path);
        assert!(path.exists(), "config written");
        assert!(!dir.join("config.json.tmp").exists(), "temp file renamed away");
        let back = UserConfig::load_from(&path);
        assert_eq!(back.last_speaker_id.as_deref(), Some("uuid:RINCON_TEST"));
        assert_eq!(back.airplay_latency_ms, 250);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_quarantined_not_overwritten() {
        let dir = temp_config_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ \"airplay_pairings\": { truncated").unwrap();
        let c = UserConfig::load_from(&path);
        // Defaults, and the broken file is preserved under a new name.
        assert_eq!(c.airplay_latency_ms, AIRPLAY_LATENCY_MS_DEFAULT);
        assert!(!path.exists(), "corrupt file moved aside");
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "exactly one quarantined copy");
        let body = std::fs::read_to_string(kept[0].path()).unwrap();
        assert!(body.contains("airplay_pairings"), "original bytes preserved");
        // A save afterwards writes a fresh valid file next to it.
        c.save_to(&path);
        assert!(UserConfig::load_from(&path).airplay_latency_ms == AIRPLAY_LATENCY_MS_DEFAULT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leading_byte_order_mark_is_accepted_and_not_written() {
        let dir = temp_config_dir("bom");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        // What Windows PowerShell 5 `Set-Content -Encoding UTF8` writes.
        std::fs::write(&path, "\u{feff}{\"last_speaker_id\":\"uuid:RINCON_BOM\",\"airplay_latency_ms\":300}")
            .unwrap();
        let c = UserConfig::load_from(&path);
        assert_eq!(c.last_speaker_id.as_deref(), Some("uuid:RINCON_BOM"));
        assert_eq!(c.airplay_latency_ms, 300);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert_eq!(names, ["config.json"], "nothing quarantined");
        // Rewritten without the mark at once, as every version reads it.
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"{"), "saved without a BOM");
        let back: UserConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.last_speaker_id.as_deref(), Some("uuid:RINCON_BOM"));
        // Only one: a second mark is not a config file this app wrote.
        std::fs::write(&path, "\u{feff}\u{feff}{}").unwrap();
        UserConfig::load_from(&path);
        assert!(!path.exists(), "quarantined");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf16_files_load_and_undecodable_ones_are_quarantined() {
        let dir = temp_config_dir("utf16");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let json = "{\"last_speaker_id\":\"Küche\",\"airplay_latency_ms\":400}";
        // What Windows PowerShell 5 `Out-File` and `>` write, and the
        // big-endian form.
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain(json.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain(json.encode_utf16().flat_map(u16::to_be_bytes)).collect();
        for bytes in [le, be] {
            std::fs::write(&path, &bytes).unwrap();
            let c = UserConfig::load_from(&path);
            assert_eq!(c.last_speaker_id.as_deref(), Some("Küche"));
            assert_eq!(c.airplay_latency_ms, 400);
            // Rewritten as UTF-8 at once, as every version reads it.
            let back: UserConfig = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(back.last_speaker_id.as_deref(), Some("Küche"));
            assert_eq!(back.airplay_latency_ms, 400);
        }
        // Odd-length UTF-16, unpaired surrogates, and (off Windows, where
        // there is no ANSI code page to try) Windows-1252: moved aside,
        // never overwritten.
        let ansi = b"{\"last_speaker_id\":\"K\xFCche\"}".to_vec();
        let mut bad = vec![vec![0xFF, 0xFE, b'{'], vec![0xFF, 0xFE, 0x00, 0xD8, b'{', 0]];
        if !cfg!(windows) {
            bad.push(ansi);
        }
        for bytes in bad {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            let c = UserConfig::load_from(&path);
            assert_eq!(c.last_speaker_id, None);
            assert!(!path.exists(), "quarantined");
            let kept: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
            assert_eq!(kept.len(), 1);
            assert_eq!(std::fs::read(kept[0].path()).unwrap(), bytes);
            c.save_to(&path);
            assert_eq!(std::fs::read(kept[0].path()).unwrap(), bytes, "a save doesn't touch it");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_in_the_ansi_code_page_is_decoded_with_it() {
        fn latin1(b: &[u8]) -> Option<String> {
            Some(b.iter().map(|&c| c as char).collect())
        }
        fn none(_: &[u8]) -> Option<String> {
            None
        }
        let ansi = b"{\"source_name\":\"K\xFCche PC\"}";
        let (text, encoding) = decode_config_text(ansi, latin1).unwrap();
        assert_eq!(text, "{\"source_name\":\"Küche PC\"}");
        assert_eq!(encoding, Some("ANSI code page"));
        assert!(decode_config_text(ansi, none).is_err());
        // UTF-8 is never second-guessed, and a UTF-8 mark rules out ANSI.
        let utf8 = "{\"source_name\":\"Küche\"}";
        assert_eq!(decode_config_text(utf8.as_bytes(), latin1).unwrap(), (utf8.to_string(), None));
        let marked: Vec<u8> = b"\xEF\xBB\xBF".iter().chain(ansi).copied().collect();
        assert!(decode_config_text(&marked, latin1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_read_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_config_dir("unreadable");
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.last_speaker_id = Some("kept".into());
        c.airplay_passwords.insert("airplay:aa".into(), "hunter2".into());
        c.save_to(&path);
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Permissions don't apply (running as root): nothing to test.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut loaded = UserConfig::load_from(&path);
        assert_eq!(loaded.last_speaker_id, None, "defaults");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Readable again later in the run: saves still leave it alone.
        loaded.airplay_latency_ms = 777;
        loaded.save_to(&path);
        loaded.clone().save_to(&path);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert_eq!(names, ["config.json"]);
        // The next launch reads it as it was.
        let next = UserConfig::load_from(&path);
        assert_eq!(next.last_speaker_id.as_deref(), Some("kept"));
        assert_eq!(next.airplay_passwords["airplay:aa"], "hunter2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_a_fresh_install() {
        let dir = temp_config_dir("missing");
        let c = UserConfig::load_from(&dir.join("config.json"));
        assert!(c.auto_reconnect_on_drop);
        assert!(!dir.exists(), "loading must not create anything");
    }

    #[test]
    fn secrets_are_sealed_on_disk_and_plain_files_migrate() {
        use crate::secret_store::tests::FakeCodec;
        use crate::secret_store::{PROTECTED_KEY, PROTECTED_PREFIX};
        let dir = temp_config_dir("secrets");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let seed = "ab".repeat(32);
        let ltpk = "cd".repeat(32);
        // A file from before protection: plain secrets.
        std::fs::write(
            &path,
            format!(
                r#"{{"airplay_passwords":{{"airplay:aa":"hunter2","airplay:bb":"pw-b"}},
                    "airplay_controller_id":"ctl","airplay_controller_seed_hex":"{seed}",
                    "airplay_pairings":{{"airplay:cc":{{"controller_id":"ctl",
                    "controller_seed_hex":"{seed}","accessory_id":"acc",
                    "accessory_ltpk_hex":"{ltpk}"}}}},"future_setting":7}}"#
            ),
        )
        .unwrap();
        let codec = FakeCodec(0x42);
        let c = UserConfig::load_from_with(&path, &codec);
        assert_eq!(c.airplay_passwords["airplay:aa"], "hunter2");
        assert_eq!(c.airplay_pairings["airplay:cc"].controller_seed_hex, seed);
        // Loading rewrote the file sealed; unknown keys survive.
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains("hunter2") && !disk.contains(&seed), "{disk}");
        assert!(disk.contains(PROTECTED_PREFIX) && disk.contains("future_setting"));
        // And it reads back to the same values.
        let back = UserConfig::load_from_with(&path, &codec);
        assert_eq!(back.airplay_passwords, c.airplay_passwords);
        assert_eq!(back.airplay_pairings, c.airplay_pairings);
        assert_eq!(back.airplay_controller_seed_hex.as_deref(), Some(seed.as_str()));
        assert!(back.unknown_keys.get(PROTECTED_KEY).is_none(), "section consumed on load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn older_versions_see_no_secrets_and_their_changes_win_on_return() {
        use crate::secret_store::tests::FakeCodec;
        use crate::secret_store::PROTECTED_KEY;
        let dir = temp_config_dir("downgrade");
        let path = dir.join("config.json");
        let codec = FakeCodec(0x42);
        let creds = |ctl: &str, seed: &str| PairingCredentials {
            controller_id: ctl.into(),
            controller_seed_hex: seed.repeat(32),
            accessory_id: "acc".into(),
            accessory_ltpk_hex: "cd".repeat(32),
        };
        let mut c = UserConfig::default();
        c.airplay_controller_id = Some("ctl-1".into());
        c.airplay_controller_seed_hex = Some("11".repeat(32));
        c.airplay_passwords.insert("airplay:aa".into(), "hunter2".into());
        c.airplay_pairings.insert("airplay:cc".into(), creds("ctl-1", "11"));
        c.airplay_pairings.insert("airplay:dd".into(), creds("ctl-1", "11"));
        c.save_to_with(&path, &codec);

        // An older version parses the file without unsealing: no pairing,
        // no seed, no password, and `protected` rides along unknown.
        let disk = std::fs::read_to_string(&path).unwrap();
        let mut old: UserConfig = serde_json::from_str(&disk).unwrap();
        assert!(old.airplay_pairings.is_empty() && old.airplay_passwords.is_empty());
        assert_eq!(old.airplay_controller_seed_hex, None);
        assert!(old.unknown_keys.contains_key(PROTECTED_KEY));
        // It mints a new identity, re-pairs one device and saves plainly.
        old.airplay_controller_id = Some("ctl-2".into());
        old.airplay_controller_seed_hex = Some("22".repeat(32));
        old.airplay_pairings.insert("airplay:cc".into(), creds("ctl-2", "22"));
        std::fs::write(&path, serde_json::to_string_pretty(&old).unwrap()).unwrap();

        // Back on this version: the older version's values win, everything
        // it didn't touch comes back from the sealed copies.
        let back = UserConfig::load_from_with(&path, &codec);
        assert_eq!(back.airplay_controller_id.as_deref(), Some("ctl-2"));
        assert_eq!(back.airplay_controller_seed_hex, Some("22".repeat(32)));
        assert_eq!(back.airplay_pairings["airplay:cc"], creds("ctl-2", "22"));
        assert_eq!(back.airplay_pairings["airplay:dd"], creds("ctl-1", "11"));
        assert_eq!(back.airplay_passwords["airplay:aa"], "hunter2");
        // ...and the file was resealed at once.
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains(&"22".repeat(32)), "{disk}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_secrets_survive_saves_but_not_deliberate_removal() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("locked");
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.airplay_passwords.insert("airplay:aa".into(), "hunter2".into());
        c.airplay_passwords.insert("airplay:bb".into(), "pw-b".into());
        c.save_to_with(&path, &FakeCodec(1));

        // Another user / machine can't open them, but saving keeps them.
        let mut other = UserConfig::load_from_with(&path, &FakeCodec(2));
        assert!(other.airplay_passwords.is_empty());
        other.last_speaker_id = Some("x".into());
        // The user clears bb's password there: that one must stay gone.
        other.remove_airplay_password("airplay:bb");
        other.save_to_with(&path, &FakeCodec(2));

        let again = UserConfig::load_from_with(&path, &FakeCodec(1));
        assert_eq!(again.airplay_passwords.get("airplay:aa").map(String::as_str), Some("hunter2"));
        assert!(!again.airplay_passwords.contains_key("airplay:bb"));
        assert_eq!(again.last_speaker_id.as_deref(), Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::sendspin::keys::{b64url, psk_id};
    use crate::sendspin::store::{PlayerPairing, SendspinConfig, ServerPairing};

    /// Sendspin with both identities, one server and one player paired.
    fn paired_sendspin() -> SendspinConfig {
        let mut s = SendspinConfig { enabled: true, source_enabled: true, ..Default::default() };
        s.ensure_client_key();
        s.ensure_pairing_psk();
        s.ensure_server_key();
        s.store_server_pairing(server_pairing("srv-a", 0xA1));
        s.player_pairings.insert(
            "player-a".into(),
            PlayerPairing {
                psk: b64url(&[0xB1; 32]),
                name: Some("Kitchen".into()),
                instance: Some("kitchen".into()),
            },
        );
        s
    }

    fn server_pairing(id: &str, psk: u8) -> ServerPairing {
        ServerPairing {
            server_id: id.into(),
            psk: b64url(&[psk; 32]),
            server_name: Some("Music Assistant".into()),
            last_used_unix: 10,
        }
    }

    fn sendspin_secrets(s: &SendspinConfig) -> Vec<String> {
        let mut v: Vec<String> =
            [&s.client_key, &s.pairing_psk, &s.server_key].iter().filter_map(|k| (*k).clone()).collect();
        v.extend(s.server_pairings.iter().map(|r| r.psk.clone()));
        v.extend(s.player_pairings.values().map(|p| p.psk.clone()));
        v
    }

    fn assert_same_client(a: &SendspinConfig, b: &SendspinConfig) {
        assert_eq!(a.client_key, b.client_key);
        assert_eq!(a.pairing_psk, b.pairing_psk);
        assert_eq!(a.server_pairings, b.server_pairings);
    }

    fn assert_same_server(a: &SendspinConfig, b: &SendspinConfig) {
        assert_eq!(a.server_key, b.server_key);
        assert_eq!(a.player_pairings, b.player_pairings);
    }

    /// Load, change and save `path` the way a version without secret
    /// groups (0.1.10) does: the same secret table, no groups.
    fn as_older_version(
        path: &std::path::Path,
        codec: &dyn SecretCodec,
        f: impl FnOnce(&mut UserConfig),
    ) -> UserConfig {
        let mut tree: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let report = secret_store::unseal_with(&mut tree, codec, &[]);
        let mut c: UserConfig = serde_json::from_value(tree).unwrap();
        c.locked_secrets = report.locked;
        f(&mut c);
        let mut tree = serde_json::to_value(&c).unwrap();
        secret_store::seal_with(&mut tree, &c.locked_secrets, codec, &[]);
        std::fs::write(path, serde_json::to_string_pretty(&tree).unwrap()).unwrap();
        c
    }

    fn protected_section(path: &std::path::Path) -> serde_json::Value {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        v[crate::secret_store::PROTECTED_KEY].clone()
    }

    #[test]
    fn sendspin_secrets_are_sealed_on_disk_and_plain_files_migrate() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("sendspin-secrets");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.last_speaker_id = Some("x".into());
        c.sendspin = paired_sendspin();
        // A file from before protection covered Sendspin: plain fields.
        std::fs::write(&path, serde_json::to_string_pretty(&c).unwrap()).unwrap();
        let codec = FakeCodec(0x42);
        let loaded = UserConfig::load_from_with(&path, &codec);
        assert_same_client(&loaded.sendspin, &c.sendspin);
        assert_same_server(&loaded.sendspin, &c.sendspin);
        // Loading rewrote the file sealed.
        let disk = std::fs::read_to_string(&path).unwrap();
        for s in sendspin_secrets(&c.sendspin) {
            assert!(!disk.contains(&s), "{s} in {disk}");
        }
        let p = protected_section(&path);
        assert!(p["/sendspin/client_identity"].is_string() && p["/sendspin/server_identity"].is_string());
        // And it reads back to the same values, settings included.
        let back = UserConfig::load_from_with(&path, &codec);
        assert_same_client(&back.sendspin, &c.sendspin);
        assert_same_server(&back.sendspin, &c.sendspin);
        assert!(back.sendspin.enabled && back.sendspin.source_enabled);
        assert_eq!(back.last_speaker_id.as_deref(), Some("x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_identity_minted_by_an_older_version_gives_way_to_the_sealed_one() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("sendspin-downgrade");
        let path = dir.join("config.json");
        let codec = FakeCodec(0x42);
        let mut c = UserConfig::default();
        c.sendspin = paired_sendspin();
        c.save_to_with(&path, &codec);
        let sealed = protected_section(&path);

        // The older version sees no Sendspin identity or pairing, mints
        // its own when the source starts and a speaker connects, and saves
        // them in the plain fields.
        let old = as_older_version(&path, &codec, |old| {
            assert!(old.sendspin.enabled && old.sendspin.source_enabled);
            assert!(old.sendspin.client_key.is_none() && old.sendspin.server_key.is_none());
            assert!(old.sendspin.server_pairings.is_empty() && old.sendspin.player_pairings.is_empty());
            old.sendspin.ensure_client_key();
            old.sendspin.ensure_pairing_psk();
            old.sendspin.ensure_server_key();
        });
        // Nothing stored was dropped or changed.
        assert_eq!(protected_section(&path), sealed);

        // Back on this version: nothing was paired with the minted
        // identities, so the originals and their pairings return, and the
        // minted keys leave the file.
        let back = UserConfig::load_from_with(&path, &codec);
        assert_same_client(&back.sendspin, &c.sendspin);
        assert_same_server(&back.sendspin, &c.sendspin);
        let disk = std::fs::read_to_string(&path).unwrap();
        for s in sendspin_secrets(&old.sendspin) {
            assert!(!disk.contains(&s), "{s} in {disk}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sendspin_pairings_made_by_an_older_version_win_on_return() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("sendspin-repaired");
        let path = dir.join("config.json");
        let codec = FakeCodec(0x42);
        let mut c = UserConfig::default();
        c.sendspin = paired_sendspin();
        c.save_to_with(&path, &codec);
        let sealed = protected_section(&path);

        // The older version pairs Music Assistant again with its new client
        // identity; its server identity stays unpaired.
        let old = as_older_version(&path, &codec, |old| {
            old.sendspin.ensure_client_key();
            old.sendspin.ensure_pairing_psk();
            old.sendspin.ensure_server_key();
            old.sendspin.store_server_pairing(server_pairing("srv-b", 0xA2));
        });

        let back = UserConfig::load_from_with(&path, &codec);
        assert_same_client(&back.sendspin, &old.sendspin);
        assert_same_server(&back.sendspin, &c.sendspin);
        let disk = std::fs::read_to_string(&path).unwrap();
        for s in sendspin_secrets(&old.sendspin) {
            assert!(!disk.contains(&s), "resealed at once: {s} in {disk}");
        }
        // The replaced client group is gone; the server group stays.
        let mut again = UserConfig::load_from_with(&path, &codec);
        assert_same_client(&again.sendspin, &old.sendspin);
        assert!(again.locked_secrets.is_empty());
        again.airplay_latency_ms += 1;
        again.save_to_with(&path, &codec);
        let p = protected_section(&path);
        assert_eq!(p.as_object().unwrap().len(), 2);
        assert_ne!(p["/sendspin/client_identity"], sealed["/sendspin/client_identity"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_sendspin_identities_survive_until_paired_again() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("sendspin-locked");
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.sendspin = paired_sendspin();
        c.save_to_with(&path, &FakeCodec(1));
        let sealed = protected_section(&path);

        // Another user / machine can't open them, and starts the source
        // with an identity of its own.
        let mut other = UserConfig::load_from_with(&path, &FakeCodec(2));
        assert_eq!(other.locked_secrets.len(), 2);
        assert!(other.sendspin.client_key.is_none() && other.sendspin.server_pairings.is_empty());
        other.sendspin.ensure_client_key();
        other.sendspin.ensure_pairing_psk();
        other.save_to_with(&path, &FakeCodec(2));
        let again = UserConfig::load_from_with(&path, &FakeCodec(1));
        assert_same_client(&again.sendspin, &c.sendspin);
        assert_same_server(&again.sendspin, &c.sendspin);

        // A server pairs with the new identity: it replaces the stored
        // client group; the server group is still kept.
        other.sendspin.store_server_pairing(server_pairing("srv-c", 0xA3));
        other.save_to_with(&path, &FakeCodec(2));
        let there = UserConfig::load_from_with(&path, &FakeCodec(2));
        assert_same_client(&there.sendspin, &other.sendspin);
        assert_eq!(there.sendspin.server_key, None);
        let here = UserConfig::load_from_with(&path, &FakeCodec(1));
        assert_eq!(here.sendspin.client_key, None);
        assert_same_server(&here.sendspin, &c.sendspin);
        // One group per role is stored: the new client group and the
        // server group kept byte for byte.
        let p = protected_section(&path);
        assert_eq!(p.as_object().unwrap().len(), 2);
        assert_eq!(p["/sendspin/server_identity"], sealed["/sendspin/server_identity"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_identity_minted_while_the_stored_one_is_locked_lasts_across_launches() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("sendspin-pending");
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.sendspin = paired_sendspin();
        c.save_to_with(&path, &FakeCodec(1));
        let sealed = protected_section(&path);

        // On another PC, an older version mints an unpaired identity in the
        // plain fields next to the groups it can't see.
        let old = as_older_version(&path, &FakeCodec(2), |old| {
            old.sendspin.ensure_client_key();
            old.sendspin.ensure_pairing_psk();
            old.sendspin.ensure_server_key();
        });
        // This version can't open the groups either: it keeps that identity,
        // and every later launch gets the same one, pairing token included.
        let first = UserConfig::load_from_with(&path, &FakeCodec(2));
        assert_eq!(first.sendspin.client_key, old.sendspin.client_key);
        let disk = std::fs::read_to_string(&path).unwrap();
        for s in sendspin_secrets(&old.sendspin) {
            assert!(!disk.contains(&s), "{s} in {disk}");
        }
        for _ in 0..2 {
            let next = UserConfig::load_from_with(&path, &FakeCodec(2));
            assert_eq!(next.sendspin.client_key, old.sendspin.client_key);
            assert_eq!(next.sendspin.pairing_psk, old.sendspin.pairing_psk);
            assert_eq!(next.sendspin.server_key, old.sendspin.server_key);
            next.save_to_with(&path, &FakeCodec(2));
        }
        // The same holds for one this version mints itself.
        let fresh = temp_config_dir("sendspin-pending-fresh").join("config.json");
        c.save_to_with(&fresh, &FakeCodec(1));
        let mut there = UserConfig::load_from_with(&fresh, &FakeCodec(2));
        there.sendspin.ensure_client_key();
        there.sendspin.ensure_pairing_psk();
        there.save_to_with(&fresh, &FakeCodec(2));
        let again = UserConfig::load_from_with(&fresh, &FakeCodec(2));
        assert_eq!(again.sendspin.client_key, there.sendspin.client_key);
        assert_eq!(again.sendspin.pairing_psk, there.sendspin.pairing_psk);

        // The stored groups were never touched, and win where they open.
        let p = protected_section(&path);
        assert_eq!(p["/sendspin/client_identity"], sealed["/sendspin/client_identity"]);
        assert_eq!(p["/sendspin/server_identity"], sealed["/sendspin/server_identity"]);
        let home = UserConfig::load_from_with(&path, &FakeCodec(1));
        assert_same_client(&home.sendspin, &c.sendspin);
        assert_same_server(&home.sendspin, &c.sendspin);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(fresh.parent().unwrap());
    }

    #[test]
    fn removed_sendspin_pairings_stay_removed() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("sendspin-removed");
        let path = dir.join("config.json");
        let codec = FakeCodec(0x42);
        let mut c = UserConfig::default();
        c.sendspin = paired_sendspin();
        c.save_to_with(&path, &codec);

        let mut c = UserConfig::load_from_with(&path, &codec);
        // Music Assistant revokes its pairing; the kitchen speaker is
        // paired again with a new identity, which replaces the old record.
        assert!(c.sendspin.remove_server_pairing_by_psk_id(&psk_id(&[0xA1; 32])));
        c.sendspin.player_pairings.insert(
            "player-b".into(),
            PlayerPairing { psk: b64url(&[0xB2; 32]), name: None, instance: None },
        );
        assert!(c.sendspin.pin_player("player-b", "kitchen"));
        c.save_to_with(&path, &codec);

        let back = UserConfig::load_from_with(&path, &codec);
        assert!(back.sendspin.server_pairings.is_empty());
        assert_eq!(back.sendspin.client_key, c.sendspin.client_key, "the identity stays");
        let ids: Vec<&String> = back.sendspin.player_pairings.keys().collect();
        assert_eq!(ids, ["player-b"]);
        assert!(back.locked_secrets.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_speaker_latency_overrides_and_clamps() {
        let mut c = UserConfig::default();
        c.airplay_latency_ms = 500;
        assert_eq!(c.airplay_latency_ms_for("airplay:AA"), 500);
        c.speaker_settings.insert(
            "airplay:AA".into(),
            SpeakerSettings { airplay_latency_ms: Some(150), ..Default::default() },
        );
        assert_eq!(c.airplay_latency_ms_for("airplay:AA"), 150);
        assert_eq!(c.airplay_latency_ms_for("airplay:BB"), 500);
        c.speaker_settings.get_mut("airplay:AA").unwrap().airplay_latency_ms = Some(1);
        assert_eq!(c.airplay_latency_ms_for("airplay:AA"), AIRPLAY_LATENCY_MS_MIN);
        // Round-trips through JSON; an old config without the map loads.
        let back: UserConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.speaker_settings, c.speaker_settings);
        let old: UserConfig = serde_json::from_str("{}").unwrap();
        assert!(old.speaker_settings.is_empty());
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

    #[test]
    fn equalizer_defaults_round_trips_and_tolerates_garbage() {
        assert!(!UserConfig::default().equalizer.enabled);
        let mut c = UserConfig::default();
        c.equalizer.enabled = true;
        c.equalizer.bands_db[2] = 4.5;
        let back: UserConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.equalizer, c.equalizer);
        // A broken equalizer entry must not take the rest of the file with it.
        let c: UserConfig = serde_json::from_str(
            r#"{"last_speaker_id":"x","equalizer":{"bands_db":[1,2,3]}}"#,
        )
        .unwrap();
        assert_eq!(c.last_speaker_id.as_deref(), Some("x"));
        assert_eq!(c.equalizer, EqSettings::default());
        // Out-of-range values are clamped onto the grid.
        let c: UserConfig =
            serde_json::from_str(r#"{"equalizer":{"enabled":true,"preamp_db":40.2}}"#).unwrap();
        assert!(c.equalizer.enabled);
        assert_eq!(c.equalizer.preamp_db, 0.0);
    }
}
