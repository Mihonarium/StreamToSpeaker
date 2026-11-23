//! Persisted user preferences: `config.json` in the roaming profile
//! (`%APPDATA%\StreamToSpeaker`, see `folders`), loaded at start and saved
//! after each change.
//!
//! - A save can't lose the settings, whatever moment a crash or a power
//!   cut hits, and leaves a mirror in `config.json.bak` for a file found
//!   damaged later (see `settings_file`).
//! - A load reads each value on its own (see `lenient_json`): one that
//!   can't be read (a hand edit, a value a later version wrote) takes its
//!   default, or its list or map entry is left out, and the rest loads. A
//!   copy of the file as it was is kept beside it, and
//!   [`UserConfig::load_notice`] says what happened. Only a file that
//!   isn't a JSON object at all is set aside, after the backup was tried.
//!   Keys this version doesn't know are kept across saves.
//!
//! Secrets (AirPlay passwords, pairing credentials, the controller seed;
//! Sendspin identity keys and pairing PSKs; the web UI's key) are sealed
//! with DPAPI into a separate `protected` section of the file and plain in
//! memory; see `secret_store`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::airplay::hap_pairing::PairingCredentials;
use crate::secret_store::{self, LockedNode, PlatformCodec, SecretCodec};
use crate::lenient_json::{self, KeyPath};
use crate::settings_file::{self, Disk, FileOps, WriteError};
use crate::discovery_net::SavedAdapter;
use crate::manual_speakers::ManualSpeaker;
use crate::stream_profile::StreamProfile;
use crate::equalizer::EqSettings;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub last_speaker_id: Option<String>,
    /// The person chose This PC last: at the next start nothing goes back
    /// to `last_speaker_id` (which still names Windows' device).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chose_this_pc: bool,
    /// The app closed to be updated: when it starts again it goes back to
    /// the speaker whatever played meanwhile.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed_for_update: bool,
    /// The output This PC means (Windows' id for it): where the sound goes
    /// back to, and what the person picked under This PC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pc_output: Option<String>,
    /// The list of This PC's outputs is open in the panel.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pc_outputs_open: bool,
    /// The level this PC sends to Music Assistant (0–100); full when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ma_level: Option<u8>,
    /// The version that ran last, to say once what an update brought.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_version_seen: Option<String>,
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
    /// The web UI's secret, 32 hex digits (see `web_access`): the key in
    /// its link (`/r/<key>/`) and the JSON API's bearer token. Made the
    /// first time the web UI is turned on or `--web-link` runs, replaced
    /// by "Make a new link". Stored sealed in the file's `protected`
    /// section. While it is `None`, saves keep the key the file holds:
    /// `--web-link` may store one while the app runs.
    #[serde(default)]
    pub web_secret: Option<String>,
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
    /// Where saves go.
    #[serde(skip)]
    saves: Saves,
    /// What loading found that the person should be told once.
    #[serde(skip)]
    load_notice: LoadNotice,
    /// Whether saves reach the file (shared with every clone).
    #[serde(skip)]
    save_health: SaveHealth,
    /// The order saves of these settings were prepared in (shared with
    /// every clone), so the newest state wins.
    #[serde(skip)]
    write_order: WriteOrder,
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
    /// to this PC and the speaker we're currently streaming to (with the
    /// other members of its Sonos group, as grouped when it was picked).
    /// Without it, anyone on the LAN who knows the URL can listen to
    /// everything the PC plays while streaming is enabled, whatever kind
    /// of speaker is playing. **Off by default** — anything else that
    /// fetches the stream is refused: a speaker grouped after the pick, a
    /// player on another PC, any setup where the fetching IP differs from
    /// the selected speaker's.
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
    /// Used for every AirPlay speaker whose delay hasn't been changed with
    /// the delay controls; one that has keeps its own (see `delay`).
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
    /// Like `airplay_latency_ms`, for HomePods without a delay of their
    /// own.
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
    #[serde(default)]
    pub equalizer: EqSettings,
    /// About → *Show Lab (experimental settings)*: the Lab page shows in
    /// the Settings window.
    #[serde(default)]
    pub show_lab: bool,
    /// Lab: Sonos and other UPnP speakers apply a delay change at once,
    /// with a short skip, instead of gradually (see `delay::catch_up`).
    #[serde(default)]
    pub delay_at_once: bool,
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeakerSettings {
    /// Left out of the speaker list (and the web API's), unless it's the
    /// one playing. Shown again with "Show hidden".
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub auto_connect: AutoConnect,
    /// The AirPlay 1 opener that worked for this speaker when it wasn't
    /// the usual one; tried first next time, forgotten when it fails. A
    /// value this version doesn't know reads as nothing remembered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub airplay_opener: Option<crate::airplay::session::Opener>,
    /// AirPlay buffer for this speaker, overriding `airplay_latency_ms`.
    /// It gives the speaker its delay until its delay is changed with the
    /// delay controls, which then keep it in the fields below (see
    /// `delay::upgrade`).
    #[serde(default)]
    pub airplay_latency_ms: Option<u32>,
    /// UPnP: the stream profile this speaker last played; the next
    /// connect starts there (see `stream_profile`).
    #[serde(default)]
    pub upnp_stream_profile: Option<StreamProfile>,
    /// What the person removed from this speaker's delay (negative) or
    /// added to it (positive), in ms: on Sonos and other UPnP speakers,
    /// Sendspin speakers and HomePod pairs, and on an AirPlay speaker or
    /// single HomePod while it is on Short delay (see `delay`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_change_ms: i32,
    /// The same for an AirPlay speaker or single HomePod on Steady: each
    /// delay choice keeps its own.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub steady_delay_change_ms: i32,
    /// Short delay or Steady, for an AirPlay speaker or single HomePod.
    /// On any AirPlay speaker (a HomePod pair included, which has no
    /// choice) it also says that the delay is the speaker's own; `None`
    /// plays what the app-wide AirPlay settings give it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_choice: Option<crate::delay::DelayChoice>,
    /// The level last used with this speaker (0–100): what a speaker
    /// whose level can't be read starts at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// *Show song titles on Kitchen*; `None` follows `forward_now_playing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_titles: Option<bool>,
    /// The speaker's tone; `None` plays the app-wide equalizer, which is
    /// what every speaker had before tone was kept per speaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<EqSettings>,
    /// When it last played from this PC (seconds since 1970): of two ways
    /// to one speaker, the one used last is listed first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<u64>,
    /// *Never louder than*: the speaker's level never goes above this
    /// (0–100); no limit when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_limit: Option<u8>,
    /// *Show as*: the name every surface of this app uses for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_as: Option<String>,
    /// This speaker's settings that a newer version wrote and this one
    /// doesn't know, kept as they are across saves.
    #[serde(flatten)]
    pub unknown_keys: serde_json::Map<String, serde_json::Value>,
}

impl SpeakerSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_zero(n: &i32) -> bool {
    *n == 0
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

/// Where [`UserConfig`]'s saves go.
#[derive(Clone, Debug, Default)]
enum Saves {
    /// To `config.json`.
    #[default]
    ToFile,
    /// Nowhere, and saving fails: `config.json` exists but couldn't be
    /// read, or couldn't be moved aside when it was unusable. These are
    /// defaults, not its contents, so saves leave the file alone for the
    /// rest of the run.
    KeepFile,
    /// Nowhere, and saving succeeds: settings for tests of app actions,
    /// which must never write or read the developer's own file.
    #[cfg(test)]
    InMemory,
    /// To this file instead of `config.json`, and read back from it:
    /// settings for tests of app actions that need a file of their own.
    #[cfg(test)]
    AtPath(PathBuf),
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

fn config_path() -> Option<PathBuf> {
    crate::folders::settings_dir().map(|d| d.join("config.json"))
}

impl UserConfig {
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

    /// Defaults whose saves succeed without touching the disk, for tests
    /// of app actions that save (which would otherwise write the
    /// developer's own file).
    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self { saves: Saves::InMemory, ..Self::default() }
    }

    /// Defaults whose saves fail, as they do all run when `config.json`
    /// couldn't be read at startup; nothing touches the disk.
    #[cfg(test)]
    pub(crate) fn unsaveable() -> Self {
        Self::not_read("a test".into())
    }

    /// Defaults that save nothing for the rest of the run, because the
    /// file couldn't be read (`why`): saving over it would destroy the
    /// settings it holds.
    fn not_read(why: String) -> Self {
        Self {
            saves: Saves::KeepFile,
            save_health: SaveHealth::with(Some(SaveProblem::NotRead { why, damaged: false, on_backup: false })),
            ..Self::default()
        }
    }

    /// Whether saves reach `config.json`, shared by every copy of these
    /// settings.
    pub fn save_health(&self) -> SaveHealth {
        self.save_health.clone()
    }

    /// Defaults saved to `path` and read back from it, for tests of app
    /// actions that need a settings file of their own.
    #[cfg(test)]
    pub(crate) fn at_path(path: PathBuf) -> Self {
        Self { saves: Saves::AtPath(path), ..Self::default() }
    }

    /// Load from `path`. A missing file is a fresh install. A file that
    /// exists but isn't usable (not text in an encoding
    /// `decode_config_text` reads, not JSON, not a JSON object: empty or
    /// zero-filled after a power cut, say) gives way to the backup the
    /// previous save left (`config.json.bak`, see `settings_file`), and is
    /// **moved aside** to `config.json.corrupt-<unix time>`; the backup's
    /// settings are saved as `config.json` at once. With no usable backup
    /// either, the defaults are used. A file that can't be read at all (or
    /// moved aside), and an unreadable backup, are never overwritten by
    /// this run's saves. What the person should hear about is in
    /// [`load_notice`](Self::load_notice).
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
        Self::load_with_ops(path, codec, &Disk)
    }

    /// [`load_from_with`](Self::load_from_with) through `fs`.
    pub(crate) fn load_with_ops(path: &std::path::Path, codec: &dyn SecretCodec, fs: &dyn FileOps) -> Self {
        settings_file::remove_stale_temps(fs, path);
        /// What `config.json` turned out to be, when it can't be used.
        enum Main {
            Missing,
            Damaged,
            Unreadable(String),
        }
        let main = match fs.read(path) {
            Ok(bytes) => match parse_settings(&bytes, codec) {
                Ok(parsed) => return Self::from_parsed(parsed, path, codec, fs, LoadNotice::default(), None),
                Err(why) => {
                    log::error!("user_config: can't use {} ({why})", path.display());
                    Main::Damaged
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Main::Missing,
            Err(e) => {
                log::error!(
                    "user_config: can't read {}: {} — the file is left as is and settings changed \
                     in this run are not saved",
                    path.display(),
                    e
                );
                Main::Unreadable(e.to_string())
            }
        };
        // `config.json` is missing or can't be used: the backup, if it is good.
        let backup_path = settings_file::backup_path(path);
        let backup = match fs.read(&backup_path) {
            Ok(bytes) => match parse_settings(&bytes, codec) {
                Ok(parsed) => Some(parsed),
                Err(why) => {
                    log::warn!("user_config: can't use the backup {} either ({why})", backup_path.display());
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                log::error!(
                    "user_config: can't read the backup {}: {} — using defaults; both files are \
                     left as they are and settings changed in this run are not saved",
                    backup_path.display(),
                    e
                );
                let why = match main {
                    Main::Unreadable(why) => why,
                    _ => e.to_string(),
                };
                return Self::not_read(why);
            }
        };
        // The file can't be replaced this run: the backup's settings if
        // there is a good one, saving nothing over the file either way.
        let keep_file = |why: String, damaged: bool, backup: Option<Parsed>| match backup {
            Some(parsed) => {
                log::warn!(
                    "user_config: using the settings in the backup {} for this run",
                    backup_path.display()
                );
                let problem = SaveProblem::NotRead { why, damaged, on_backup: true };
                Self::from_parsed(parsed, path, codec, fs, LoadNotice::default(), Some(problem))
            }
            None => Self {
                saves: Saves::KeepFile,
                save_health: SaveHealth::with(Some(SaveProblem::NotRead { why, damaged, on_backup: false })),
                ..Self::default()
            },
        };
        let mut notice = LoadNotice::default();
        match main {
            Main::Missing if backup.is_none() => return Self::default(),
            Main::Missing => {}
            Main::Unreadable(why) => return keep_file(why, false, backup),
            Main::Damaged => match settings_file::move_aside(fs, path, "corrupt") {
                Ok(aside) => {
                    log::warn!("user_config: moved {} to {}", path.display(), aside.display());
                    notice.unusable_kept_as = Some(aside);
                }
                Err(re) => {
                    log::error!(
                        "user_config: could not move {} aside ({}); the file is left as is and \
                         settings changed in this run are not saved",
                        path.display(),
                        re
                    );
                    return keep_file(re.to_string(), true, backup);
                }
            },
        }
        match backup {
            Some(parsed) => {
                log::warn!("user_config: loaded the settings from the backup {}", backup_path.display());
                notice.restored_from_backup = true;
                let c = Self::from_parsed(parsed, path, codec, fs, notice, None);
                // So the next start finds them in `config.json` again.
                c.save_to_ops(path, codec, fs);
                c
            }
            None => {
                log::error!("user_config: no usable backup; starting with defaults");
                notice.all_reset = true;
                Self { load_notice: notice, ..Self::default() }
            }
        }
    }

    /// Settings from a file `parse_settings` accepted, saving at once when
    /// the file needs it (see [`load_from_with`](Self::load_from_with)).
    /// `keep`: the file can't be replaced this run, for this reason.
    fn from_parsed(
        parsed: Parsed,
        path: &std::path::Path,
        codec: &dyn SecretCodec,
        fs: &dyn FileOps,
        mut notice: LoadNotice,
        keep: Option<SaveProblem>,
    ) -> Self {
        let Parsed { mut config, report, encoding, reset } = parsed;
        let mut drop_unread = false;
        if let Some((paths, as_found)) = reset {
            // Kept before any save drops the values that couldn't be read;
            // without a copy, nothing is saved over the file.
            match keep_before_reset(fs, path, as_found, &report.locked, codec) {
                Ok(copy) => {
                    notice.reset_kept_as = Some(copy);
                    drop_unread = true;
                }
                Err(why) => {
                    config.saves = Saves::KeepFile;
                    config.save_health = SaveHealth::with(Some(SaveProblem::CopyNotKept { why }));
                }
            }
            for p in &paths {
                if relearned(p) {
                    log::info!(
                        "user_config: {} couldn't be read; the app learns it again",
                        secret_store::pointer(p)
                    );
                } else {
                    log::warn!(
                        "user_config: {} couldn't be read, so it is back to its default",
                        secret_store::pointer(p)
                    );
                }
            }
            notice.reset = paths;
        }
        config.locked_secrets = report.locked;
        config.load_notice = notice;
        if let Some(problem) = keep {
            config.saves = Saves::KeepFile;
            config.save_health = SaveHealth::with(Some(problem));
        }
        if let Some(encoding) = encoding {
            log::info!("user_config: rewriting {} ({encoding}) as UTF-8", path.display());
            config.save_to_ops(path, codec, fs);
        } else if report.needs_save && codec.can_protect() {
            log::info!(
                "user_config: protecting stored secrets written unprotected by an earlier version"
            );
            config.save_to_ops(path, codec, fs);
        } else if drop_unread {
            // The file then holds what the app runs with, and the next start
            // has nothing to report again.
            config.save_to_ops(path, codec, fs);
        }
        config
    }

    /// What loading found that the person should be told once: settings
    /// restored from the backup, or reset.
    pub fn load_notice(&self) -> &LoadNotice {
        &self.load_notice
    }

    /// Best-effort save. Logged-on-failure rather than propagated — a
    /// failed config write should never block UI actions like speaker
    /// selection. An action that must not take effect unsaved uses
    /// [`try_save`](Self::try_save) instead.
    ///
    /// Made while this thread holds the app's settings lock
    /// ([`SettingsCell`]), the save is written when that lock is
    /// released, so no other thread waits for the disk.
    pub fn save(&self) {
        let Some(path) = self.file() else { return; };
        match self.prepare(&path, &PlatformCodec, &Disk) {
            Ok(Some(job)) => {
                if let Some(job) = stage(job) {
                    let _ = job.run(&Disk);
                }
            }
            Ok(None) | Err(SaveFailure::NotRead(_)) => {}
            Err(SaveFailure::Failed { problem, .. }) => self.save_health.set(Some(problem)),
        }
    }

    /// Save, or say why `config.json` wasn't written.
    pub fn try_save(&self) -> Result<(), String> {
        match self.file() {
            Some(path) => self.try_save_to_with(&path, &PlatformCodec),
            None => Err("there's no folder to keep the settings in".into()),
        }
    }

    /// The file these settings are saved to: `config.json`, or a test's own.
    fn file(&self) -> Option<PathBuf> {
        match &self.saves {
            #[cfg(test)]
            Saves::AtPath(path) => Some(path.clone()),
            _ => config_path(),
        }
    }

    /// The web UI's key as `config.json` holds it now, which may be one
    /// `--web-link` stored after these settings were loaded (it can run
    /// while the app does). Read without loading the rest of the file:
    /// nothing is moved aside, saved or logged. `None` for settings that
    /// couldn't be read at startup, which take nothing from the file as
    /// they save nothing over it, and for test settings kept in memory.
    pub fn stored_web_secret(&self) -> Option<String> {
        match &self.saves {
            Saves::ToFile => {}
            #[cfg(test)]
            Saves::AtPath(_) => {}
            _ => return None,
        }
        Self::stored_web_secret_at(&self.file()?, &PlatformCodec)
    }

    /// [`stored_web_secret`](Self::stored_web_secret) for the file at
    /// `path`, opened with `codec`.
    pub fn stored_web_secret_at(path: &std::path::Path, codec: &dyn SecretCodec) -> Option<String> {
        let (tree, _) = settings_tree(&std::fs::read(path).ok()?).ok()?;
        stored_web_secret_in(&tree, codec)
    }

    /// Save to `path`: see `settings_file` for how a save can't lose the
    /// settings, whatever moment a crash or a power cut hits. `rename`
    /// replaces an existing file on both Windows and Unix.
    pub fn save_to(&self, path: &std::path::Path) {
        self.save_to_with(path, &PlatformCodec)
    }

    /// [`save_to`](Self::save_to) with an explicit secret protector.
    pub fn save_to_with(&self, path: &std::path::Path, codec: &dyn SecretCodec) {
        self.save_to_ops(path, codec, &Disk)
    }

    /// Failures are logged once each time they change (see
    /// [`SaveHealth`]).
    fn save_to_ops(&self, path: &std::path::Path, codec: &dyn SecretCodec, fs: &dyn FileOps) {
        let _ = self.try_save_to_ops(path, codec, fs);
    }

    /// [`save_to_with`](Self::save_to_with), returning why the file wasn't
    /// written instead of logging it.
    pub fn try_save_to_with(
        &self,
        path: &std::path::Path,
        codec: &dyn SecretCodec,
    ) -> Result<(), String> {
        self.try_save_to_ops(path, codec, &Disk)
    }

    /// [`try_save_to_with`](Self::try_save_to_with) through `fs`, noting
    /// the outcome in [`save_health`](Self::save_health). Written now,
    /// whatever lock the caller holds.
    pub(crate) fn try_save_to_ops(
        &self,
        path: &std::path::Path,
        codec: &dyn SecretCodec,
        fs: &dyn FileOps,
    ) -> Result<(), String> {
        match self.prepare(path, codec, fs) {
            Ok(Some(job)) => job.run(fs),
            Ok(None) => Ok(()),
            Err(SaveFailure::NotRead(e)) => Err(e),
            Err(SaveFailure::Failed { error, problem }) => {
                self.save_health.set(Some(problem));
                Err(error)
            }
        }
    }

    /// The bytes to write for these settings, sealed, as a [`SaveJob`]
    /// ordered after every save prepared before it; `None` for test
    /// settings kept in memory.
    fn prepare(
        &self,
        path: &std::path::Path,
        codec: &dyn SecretCodec,
        fs: &dyn FileOps,
    ) -> Result<Option<SaveJob>, SaveFailure> {
        let Some(bytes) = self.write_to_ops(path, codec, fs)? else { return Ok(None) };
        let generation = self.write_order.next();
        Ok(Some(SaveJob {
            order: self.write_order.clone(),
            health: self.save_health.clone(),
            path: path.to_path_buf(),
            bytes,
            generation,
        }))
    }

    /// The sealed text to save at `path`; `None` for settings kept in
    /// memory.
    fn write_to_ops(
        &self,
        path: &std::path::Path,
        codec: &dyn SecretCodec,
        fs: &dyn FileOps,
    ) -> Result<Option<Vec<u8>>, SaveFailure> {
        match &self.saves {
            Saves::ToFile => {}
            Saves::KeepFile => {
                return Err(SaveFailure::NotRead(format!(
                    "not saving over {}: it couldn't be read at startup",
                    path.display()
                )));
            }
            #[cfg(test)]
            Saves::InMemory => return Ok(None),
            #[cfg(test)]
            Saves::AtPath(_) => {}
        }
        let unwritable = |e: serde_json::Error| SaveFailure::Failed {
            error: format!("serialize: {e}"),
            problem: SaveProblem::Failed { why: e.to_string(), kind: FailKind::Other },
        };
        let mut tree = serde_json::to_value(self).map_err(unwritable)?;
        if self.web_secret.is_none() {
            // The file may hold a key `--web-link` stored while these
            // settings held none. Writing none over it would break the link
            // it printed; the app takes that key when its web UI is turned
            // on (see `web_access::stored_or_new_secret`).
            let now = fs.read(path).ok().and_then(|b| settings_tree(&b).ok());
            if let Some(key) = now.and_then(|(t, _)| stored_web_secret_in(&t, codec)) {
                if let Some(fields) = tree.as_object_mut() {
                    fields.insert("web_secret".into(), serde_json::Value::String(key));
                }
            }
        }
        secret_store::seal(&mut tree, &self.locked_secrets, codec);
        let content = serde_json::to_string_pretty(&tree).map_err(unwritable)?;
        Ok(Some(content.into_bytes()))
    }
}

/// What loading the settings found that the person should be told once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadNotice {
    /// The settings were read from the backup, because `config.json` was
    /// missing or unusable: they are as they were before the last save.
    pub restored_from_backup: bool,
    /// `config.json` as it was found, when it was unusable and moved aside.
    pub unusable_kept_as: Option<PathBuf>,
    /// Every setting is back to its default: `config.json` was unusable
    /// and there was no usable backup.
    pub all_reset: bool,
    /// Settings that couldn't be read and are back to their defaults, by
    /// key path (an entry of a list or map that couldn't be read is left
    /// out as a whole).
    pub reset: Vec<KeyPath>,
    /// The copy of the file as it was found, kept when `reset` isn't empty.
    pub reset_kept_as: Option<PathBuf>,
}

impl LoadNotice {
    /// Whether there is anything to tell: values the app learns again by
    /// itself ([`RELEARNED`]) are not.
    pub fn is_empty(&self) -> bool {
        !self.restored_from_backup
            && self.unusable_kept_as.is_none()
            && !self.all_reset
            && self.reset.iter().all(|p| relearned(p))
    }

    /// What to tell the person, in a few sentences; `None` when there is
    /// nothing. File names are given without their folder; settings by the
    /// window's words, speakers by the names `name_of` knows for their ids
    /// (the key paths are in the log).
    pub fn describe(&self, name_of: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut out: Vec<String> = Vec::new();
        if self.restored_from_backup {
            out.push(match &self.unusable_kept_as {
                Some(kept) => format!(
                    "Your settings file was damaged, so the app used its backup copy instead. The \
                     damaged file is kept as {} in the settings folder.",
                    name(kept)
                ),
                None => "Your settings file was missing, so the app used its backup copy instead.".into(),
            });
        } else if self.all_reset {
            let kept = self
                .unusable_kept_as
                .as_ref()
                .map(|k| format!(" The damaged file is kept as {} in the settings folder.", name(k)))
                .unwrap_or_default();
            out.push(format!(
                "Your settings file was damaged and there was no usable backup copy, so the app \
                 started with default settings: speakers, pairings and passwords need setting up \
                 again.{kept}"
            ));
        }
        if self.reset.iter().any(|p| !relearned(p)) {
            const SHOWN: usize = 4;
            let mut labels: Vec<String> = Vec::new();
            for p in self.reset.iter().filter(|p| !relearned(p)) {
                let label = reset_label(p, name_of);
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            let more = labels.len().saturating_sub(SHOWN);
            labels.truncate(SHOWN);
            if more > 0 {
                labels.push(format!("and {more} more"));
            }
            let kept = self
                .reset_kept_as
                .as_ref()
                .map(|k| format!(" The file as it was is kept as {} in the settings folder.", name(k)))
                .unwrap_or_default();
            out.push(format!(
                "Some settings couldn't be read and were reset or left out: {}.{kept}",
                labels.join("; ")
            ));
        }
        (!out.is_empty()).then(|| out.join(" "))
    }
}

/// What the reset notice calls each top-level setting, and whether that
/// is the window's own text for it (`true`: a test finds it in the
/// window's code) or a description of something the window doesn't show
/// (`false`).
const SETTING_LABELS: &[(&str, &str, bool)] = &[
    ("last_speaker_id", "The speaker used last", false),
    ("chose_this_pc", "The speaker used last", false),
    ("closed_for_update", "The speaker used last", false),
    ("pc_output", "The output This PC plays on", false),
    ("pc_outputs_open", "This PC's outputs shown in the panel", false),
    ("ma_level", "The level sent to Music Assistant", false),
    ("last_version_seen", "What's new after an update", false),
    ("onboarding_dismissed", "Getting started", true),
    ("always_minimise_to_tray", "Always minimise to tray", true),
    ("donation_prompt_hidden_until", "The donation reminder", false),
    ("auto_reconnect_on_launch", "Auto-connect to this speaker when the app launches", true),
    ("prefer_realtime_airplay", "AirPlay 2 stream mode", true),
    ("auto_reconnect_on_drop", "Reconnect automatically if the connection drops", true),
    ("airplay_mfi_encryption", "AirPlay MFi encryption (experimental)", true),
    ("airplay_passwords", "AirPlay password", true),
    ("airplay_pairings", "Paired with this PC by PIN", true),
    ("airplay_controller_id", "This PC's AirPlay pairing identity", false),
    ("airplay_controller_seed_hex", "This PC's AirPlay pairing identity", false),
    ("web_secret", "Web UI", true),
    ("forward_now_playing", "Show now-playing on the speaker", true),
    ("airplay_uncompressed_alac", "AirPlay uncompressed audio", false),
    ("privacy_mode", "Privacy mode", true),
    ("airplay_latency_ms", "AirPlay buffer (latency)", true),
    ("airplay_homepod_buffered", "Use the buffered stream for single HomePods", true),
    ("airplay_homepod_latency_ms", "HomePod delay", true),
    ("check_for_updates", "Check for updates", true),
    ("update_last_check_unix", "Check for updates", true),
    ("update_latest_tag", "Check for updates", true),
    ("update_latest_url", "Check for updates", true),
    ("update_skipped_tag", "Check for updates", true),
    ("update_banner_hidden_until", "Check for updates", true),
    ("sendspin", "Sendspin and Music Assistant", true),
    ("manual_speakers", "Speakers added by address", true),
    ("discovery_adapter", "Discovery network adapter", true),
    ("speaker_settings", "Each speaker's settings", false),
    ("equalizer", "Equalizer", true),
    ("show_lab", "Show Lab (experimental settings)", true),
    ("delay_at_once", "Apply delay changes on Sonos and UPnP speakers", true),
];

/// The same for a speaker's own settings (`speaker_settings.<id>.<key>`).
const SPEAKER_LABELS: &[(&str, &str, bool)] = &[
    ("hidden", "Hide this speaker from the list", true),
    ("auto_connect", "Connect automatically", true),
    ("airplay_latency_ms", "Own AirPlay buffer for this speaker", true),
    ("delay_change_ms", "What was removed from or added to its delay", false),
    ("steady_delay_change_ms", "What was removed from or added to its delay", false),
    ("delay_choice", "Short delay or Steady", false),
    ("level", "The volume last used with it", false),
    ("song_titles", "Show song titles", false),
    ("tone", "Tone", true),
    ("last_played", "When it last played from this PC", false),
    ("volume_limit", "Never louder than", false),
    ("show_as", "Show as", false),
];

/// Values the app remembers by itself and learns again at the next
/// connect, with nothing the person notices but that connect being a
/// little slower: one that can't be read is left out quietly (the log
/// names it; the reset notice doesn't). Key paths; `*` matches any key.
/// Anything the person chose, or would notice losing (a pairing, a
/// password, the saved speaker), is labelled above instead.
const RELEARNED: &[&[&str]] = &[
    // The stream profile a UPnP speaker last played (see `stream_profile`).
    &["speaker_settings", "*", "upnp_stream_profile"],
    // The AirPlay 1 opener that worked for a speaker when it wasn't the
    // usual one (see `airplay::session::Opener`).
    &["speaker_settings", "*", "airplay_opener"],
];

/// Whether `path` is one of [`RELEARNED`].
fn relearned(path: &[String]) -> bool {
    RELEARNED.iter().any(|p| p.len() == path.len() && p.iter().zip(path).all(|(p, k)| *p == "*" || p == k))
}

/// A speaker's settings as a whole.
const WHOLE_SPEAKER_LABEL: (&str, &str, bool) = ("speaker_settings.<id>", "Settings for this speaker", true);

/// What the reset notice calls a top-level setting; `None` for a key this
/// version doesn't have.
fn setting_label(key: &str) -> Option<&'static str> {
    SETTING_LABELS.iter().find(|(k, _, _)| *k == key).map(|(_, label, _)| *label)
}

/// What the reset notice calls the setting at `path` that was reset or
/// left out: the window's words for it, and for a speaker's own setting,
/// password or pairing the speaker's name (from `name_of`) before them.
fn reset_label(path: &[String], name_of: &dyn Fn(&str) -> Option<String>) -> String {
    let speaker = |id: &str| name_of(id).unwrap_or_else(|| "A speaker".into());
    let key = path.first().map(String::as_str).unwrap_or_default();
    let label_of = |table: &[(&str, &'static str, bool)], k: &str| {
        table.iter().find(|(name, _, _)| *name == k).map(|(_, label, _)| *label)
    };
    match (key, path.get(1), path.get(2).map(String::as_str)) {
        ("speaker_settings", Some(id), field) => {
            let what = field.and_then(|f| label_of(SPEAKER_LABELS, f)).unwrap_or(WHOLE_SPEAKER_LABEL.1);
            format!("{}: {what}", speaker(id))
        }
        ("airplay_passwords" | "airplay_pairings", Some(id), _) => {
            format!("{}: {}", speaker(id), setting_label(key).unwrap_or_default())
        }
        ("manual_speakers", Some(_), _) => setting_label(key).unwrap_or_default().into(),
        _ => setting_label(key).unwrap_or("A setting from a newer version").into(),
    }
}

/// Why saves aren't reaching `config.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveProblem {
    /// The file couldn't be read at start, or was damaged and couldn't be
    /// moved aside (`damaged`), for `why` (the system's words); the app
    /// runs on the backup's settings (`on_backup`) or on defaults, and
    /// saves nothing over the file.
    NotRead { why: String, damaged: bool, on_backup: bool },
    /// The last save failed (`why`, the system's words; `kind`, what the
    /// error shows).
    Failed { why: String, kind: FailKind },
    /// Some values in the file couldn't be read and no copy of the file
    /// could be kept (`why`), so the app saves nothing over it.
    CopyNotKept { why: String },
}

/// What a failed save's error shows about its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// Windows refused (access denied, or another program has the file).
    Refused,
    /// The disk (or the person's quota on it) is full.
    DiskFull,
    /// Anything else: the system's words say what.
    Other,
}

impl SaveProblem {
    /// The problem a save that failed with `e` leaves.
    pub fn failed(e: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        let kind = if settings_file::is_refusal(e) {
            FailKind::Refused
        } else if matches!(e.kind(), ErrorKind::StorageFull | ErrorKind::QuotaExceeded)
            || (cfg!(windows) && matches!(e.raw_os_error(), Some(39 | 112)))
        {
            FailKind::DiskFull
        } else {
            FailKind::Other
        };
        SaveProblem::Failed { why: e.to_string(), kind }
    }

    /// What to tell the person while the problem lasts.
    pub fn describe(&self) -> String {
        match self {
            SaveProblem::NotRead { why, damaged, on_backup } => format!(
                "{}, so the app is using {} and leaves the file alone: changes made now won't be \
                 kept. Quit the app and start it again to retry. Details: {why}",
                if *damaged {
                    "Your settings file is damaged and couldn't be moved aside"
                } else {
                    "Your settings file couldn't be read"
                },
                if *on_backup { "its backup copy" } else { "default settings" },
            ),
            SaveProblem::CopyNotKept { why } => format!(
                "Some settings couldn't be read, and a copy of the settings file couldn't be kept \
                 beside it, so the app leaves the file as it is: changes made now won't be kept. \
                 Details: {why}"
            ),
            SaveProblem::Failed { why, kind } => {
                let lost = "changes made now will be lost when the app closes";
                match kind {
                    FailKind::Refused => format!(
                        "Settings can't be saved, so {lost}. Windows refused to write config.json: \
                         the file or its folder may be read-only, or another program may be holding \
                         the file. The app keeps trying. Details: {why}"
                    ),
                    FailKind::DiskFull => format!(
                        "Settings can't be saved because the disk is full, so {lost}. The app keeps \
                         trying, and saves them once there is space. Details: {why}"
                    ),
                    FailKind::Other => {
                        format!("Settings can't be saved, so {lost}. The app keeps trying. Details: {why}")
                    }
                }
            }
        }
    }
}

/// Whether saves reach `config.json`: the problem since the last save
/// that failed, until one succeeds. Shared by every copy of the settings
/// and by whoever shows it; each change is logged once.
#[derive(Clone, Debug, Default)]
pub struct SaveHealth(std::sync::Arc<std::sync::Mutex<Option<SaveProblem>>>);

impl SaveHealth {
    fn with(problem: Option<SaveProblem>) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(problem)))
    }

    /// The current problem, if saves aren't reaching the file.
    pub fn problem(&self) -> Option<SaveProblem> {
        self.0.lock().map(|p| p.clone()).unwrap_or(None)
    }

    fn set(&self, now: Option<SaveProblem>) {
        let Ok(mut current) = self.0.lock() else { return };
        if *current == now {
            return;
        }
        match &now {
            Some(SaveProblem::Failed { why, .. }) => {
                log::warn!(
                    "user_config: the settings can't be saved ({why}); changes made now will be \
                     lost when the app closes"
                )
            }
            Some(SaveProblem::NotRead { .. } | SaveProblem::CopyNotKept { .. }) => {}
            None => log::info!("user_config: the settings are saved again"),
        }
        *current = now;
    }
}

/// The order of one settings file's saves: each save is numbered when its
/// text is made (under the settings lock, so in the order of the changes),
/// and one whose number is lower than a save already written is not
/// written: the newest state wins. Numbering takes no lock, so a save
/// never waits for another one being written; only the writes take turns.
#[derive(Clone, Debug, Default)]
struct WriteOrder(std::sync::Arc<Order>);

#[derive(Debug, Default)]
struct Order {
    /// The number the last save got.
    numbered: std::sync::atomic::AtomicU64,
    /// Held for each write: the newest save written (or tried), and its
    /// outcome.
    written: std::sync::Mutex<Option<(u64, Result<(), String>)>>,
}

impl WriteOrder {
    fn next(&self) -> u64 {
        self.0.numbered.fetch_add(1, std::sync::atomic::Ordering::SeqCst).saturating_add(1)
    }
}

/// One save's text, to be written to `path`.
struct SaveJob {
    order: WriteOrder,
    health: SaveHealth,
    path: PathBuf,
    bytes: Vec<u8>,
    generation: u64,
}

impl SaveJob {
    /// Write it (see `settings_file`), unless a newer save of the same
    /// settings was written meanwhile, and note the outcome. The writes of
    /// one settings file take turns under their own lock, not the
    /// settings lock.
    fn run(self, fs: &dyn FileOps) -> Result<(), String> {
        let mut written = self.order.0.written.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((newest, outcome)) = &*written {
            if *newest >= self.generation {
                return outcome.clone();
            }
        }
        let path = &self.path;
        let outcome = settings_file::replace(fs, path, &self.bytes).map_err(|e| {
            let (error, e) = match e {
                WriteError::Folder(e) => (format!("mkdir {}: {}", path.parent().unwrap_or(path).display(), e), e),
                WriteError::Write(e) => (format!("write beside {}: {}", path.display(), e), e),
                WriteError::Replace(e) => (format!("replace {}: {}", path.display(), e), e),
            };
            (error, SaveProblem::failed(&e))
        });
        self.health.set(outcome.as_ref().err().map(|(_, problem)| problem.clone()));
        let outcome = outcome.map_err(|(error, _)| error);
        *written = Some((self.generation, outcome.clone()));
        outcome
    }
}

thread_local! {
    /// Saves made on this thread while it holds the settings lock, to be
    /// written once it releases it; `None` while it doesn't hold it.
    static STAGED: std::cell::RefCell<Option<Vec<SaveJob>>> = const { std::cell::RefCell::new(None) };
}

/// Keep `job` for when this thread releases the settings lock; it back
/// when this thread doesn't hold it.
fn stage(job: SaveJob) -> Option<SaveJob> {
    STAGED.with(|staged| match staged.borrow_mut().as_mut() {
        Some(jobs) => {
            jobs.push(job);
            None
        }
        None => Some(job),
    })
}

/// The app's settings behind their lock. A save made while the lock is
/// held ([`UserConfig::save`]) is written when the lock is released,
/// outside it: a thread that only reads a setting (a Music Assistant
/// handshake, the web server) never waits for a flush to the disk, and of
/// two saves the newer state is the one that stays on the disk. The thread
/// that made a save writes it once it has let go of the lock; when two
/// writes overlap, they take turns there, holding nothing else.
pub struct SettingsCell(std::sync::Mutex<UserConfig>);

impl SettingsCell {
    pub fn new(settings: UserConfig) -> Self {
        Self(std::sync::Mutex::new(settings))
    }

    /// The settings, locked (as `Mutex::lock`).
    pub fn lock(&self) -> std::sync::LockResult<SettingsGuard<'_>> {
        let owns = STAGED.with(|staged| {
            let mut staged = staged.borrow_mut();
            let free = staged.is_none();
            if free {
                *staged = Some(Vec::new());
            }
            free
        });
        let wrap = |guard| SettingsGuard { guard: std::mem::ManuallyDrop::new(guard), owns };
        match self.0.lock() {
            Ok(guard) => Ok(wrap(guard)),
            Err(poisoned) => Err(std::sync::PoisonError::new(wrap(poisoned.into_inner()))),
        }
    }
}

/// The locked settings; saves made through it are written when it drops,
/// after the lock is released.
pub struct SettingsGuard<'a> {
    guard: std::mem::ManuallyDrop<std::sync::MutexGuard<'a, UserConfig>>,
    /// Whether this guard writes this thread's staged saves.
    owns: bool,
}

impl std::ops::Deref for SettingsGuard<'_> {
    type Target = UserConfig;
    fn deref(&self) -> &UserConfig {
        &self.guard
    }
}

impl std::ops::DerefMut for SettingsGuard<'_> {
    fn deref_mut(&mut self) -> &mut UserConfig {
        &mut self.guard
    }
}

impl Drop for SettingsGuard<'_> {
    fn drop(&mut self) {
        let staged = if self.owns { STAGED.with(|staged| staged.borrow_mut().take()) } else { None };
        // SAFETY: the guard is dropped once, here, and not used after.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.guard) };
        for job in staged.into_iter().flatten() {
            let _ = job.run(&Disk);
        }
    }
}

/// Why [`UserConfig::write_to_ops`] made no text to write.
#[derive(Debug)]
enum SaveFailure {
    /// The file couldn't be read at start.
    NotRead(String),
    /// Writing failed: the full error, and what to tell the person.
    Failed { error: String, problem: SaveProblem },
}

/// A settings file `parse_settings` accepted.
struct Parsed {
    config: UserConfig,
    report: secret_store::UnsealReport,
    /// The file's encoding, when it isn't the UTF-8 saves write.
    encoding: Option<&'static str>,
    /// Values that couldn't be read (their key paths), and the file's tree
    /// as it was found, secrets unsealed.
    reset: Option<(Vec<KeyPath>, serde_json::Value)>,
}

/// The settings in a file's bytes, secrets unsealed with `codec`; why
/// they aren't usable otherwise. A value that can't be read costs that
/// value only: it takes its default, or its list or map entry is left out
/// (see `lenient_json`); only a file that isn't a JSON object is refused.
fn parse_settings(bytes: &[u8], codec: &dyn SecretCodec) -> Result<Parsed, String> {
    let (mut tree, encoding) = settings_tree(bytes)?;
    let report = secret_store::unseal(&mut tree, codec);
    let (mut config, rejected) = lenient_json::from_value::<UserConfig>(&tree)?;
    config.equalizer = config.equalizer.sanitized();
    let reset = (!rejected.is_empty()).then_some((rejected, tree));
    Ok(Parsed { config, report, encoding, reset })
}

/// Most copies of the settings file kept from before values were reset.
const RESET_COPIES: usize = 3;

/// Keep a copy of a settings file whose values couldn't all be read, as
/// it was found (`as_found`, secrets unsealed; sealed again here with
/// `locked`), beside `path` as `config.json.before-reset-<unix time>`. An
/// identical copy already kept is used instead; the oldest beyond
/// [`RESET_COPIES`] are removed. Its path; why not (logged) when it
/// couldn't be written.
fn keep_before_reset(
    fs: &dyn FileOps,
    path: &std::path::Path,
    mut as_found: serde_json::Value,
    locked: &[LockedNode],
    codec: &dyn SecretCodec,
) -> Result<PathBuf, String> {
    let plain = as_found.clone();
    secret_store::seal(&mut as_found, locked, codec);
    let text = serde_json::to_string_pretty(&as_found).map_err(|e| {
        log::warn!("user_config: couldn't keep a copy of the settings ({e})");
        e.to_string()
    })?;
    // Sealing twice gives different bytes: copies are compared opened.
    let same = |bytes: &[u8]| {
        settings_tree(bytes).is_ok_and(|(mut tree, _)| {
            let report = secret_store::unseal(&mut tree, codec);
            tree == plain && report.locked == locked
        })
    };
    settings_file::keep_copy(fs, path, "before-reset", text.as_bytes(), RESET_COPIES, &same).map_err(|e| {
        log::warn!("user_config: couldn't keep a copy of the settings beside {} ({e})", path.display());
        e.to_string()
    })
}

/// A settings file's JSON tree, which must be an object, as stored
/// (sealed), and its encoding when it isn't UTF-8.
fn settings_tree(bytes: &[u8]) -> Result<(serde_json::Value, Option<&'static str>), String> {
    let (json, encoding) = decode_config_text(bytes, decode_ansi)?;
    let tree: serde_json::Value = serde_json::from_str(&json).map_err(|e| format!("not valid JSON: {e}"))?;
    if !tree.is_object() {
        return Err("not a JSON object".into());
    }
    Ok((tree, encoding))
}

/// The web UI's key in a settings file's tree (see
/// [`UserConfig::stored_web_secret`]).
fn stored_web_secret_in(tree: &serde_json::Value, codec: &dyn SecretCodec) -> Option<String> {
    match secret_store::stored_value(tree, "web_secret", codec)? {
        serde_json::Value::String(key) => Some(key),
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings read from `text` as a load reads them.
    fn from_text(text: &str) -> UserConfig {
        parse_settings(text.as_bytes(), &PlatformCodec).unwrap_or_else(|e| panic!("{text}: {e}")).config
    }

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
        assert_eq!(c.web_secret, None);
    }

    #[test]
    fn the_web_secret_is_sealed_with_the_other_secrets() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("web-secret");
        let path = dir.join("config.json");
        let key = "00112233445566778899aabbccddeeff";
        let mut c = UserConfig::default();
        c.web_secret = Some(key.into());
        c.save_to_with(&path, &FakeCodec(0x42));
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains(key), "{disk}");
        assert!(disk.contains("\"/web_secret\""), "{disk}");
        let back = UserConfig::load_from_with(&path, &FakeCodec(0x42));
        assert_eq!(back.web_secret.as_deref(), Some(key));
        // Another account can't open it, and gets none.
        let other = UserConfig::load_from_with(&path, &FakeCodec(0x43));
        assert_eq!(other.web_secret, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_holding_no_web_key_keep_the_one_the_file_holds() {
        use crate::secret_store::tests::FakeCodec;
        let key = "00112233445566778899aabbccddeeff";
        let own = "ffeeddccbbaa99887766554433221100";
        let codecs: [(&str, &dyn SecretCodec); 2] = [("fake", &FakeCodec(0x42)), ("platform", &PlatformCodec)];
        for (tag, codec) in codecs {
            let dir = temp_config_dir(&format!("keep-web-key-{tag}"));
            let path = dir.join("config.json");
            // The app's settings, loaded before there was a key.
            UserConfig::default().save_to_with(&path, codec);
            let mut app = UserConfig::load_from_with(&path, codec);
            // `--web-link` meanwhile, a process of its own, stores one.
            let mut other = UserConfig::load_from_with(&path, codec);
            other.web_secret = Some(key.into());
            other.save_to_with(&path, codec);
            // The app saves another setting: the key stays in the file.
            app.last_speaker_id = Some("uuid:RINCON_KEEP".into());
            app.try_save_to_with(&path, codec).unwrap();
            assert_eq!(app.web_secret, None, "{tag}");
            let back = UserConfig::load_from_with(&path, codec);
            assert_eq!(back.web_secret.as_deref(), Some(key), "{tag}");
            assert_eq!(back.last_speaker_id.as_deref(), Some("uuid:RINCON_KEEP"), "{tag}");
            if codec.can_protect() {
                let disk = std::fs::read_to_string(&path).unwrap();
                assert!(!disk.contains(key), "{tag}: {disk}");
            }
            // Settings holding a key of their own save theirs.
            app.web_secret = Some(own.into());
            app.try_save_to_with(&path, codec).unwrap();
            assert_eq!(UserConfig::load_from_with(&path, codec).web_secret.as_deref(), Some(own), "{tag}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn reading_the_stored_web_key_changes_nothing() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("stored-web-key");
        let path = dir.join("config.json");
        let key = "00112233445566778899aabbccddeeff";
        let codec = FakeCodec(0x42);
        assert_eq!(UserConfig::stored_web_secret_at(&path, &codec), None);
        let mut c = UserConfig::default();
        c.web_secret = Some(key.into());
        c.save_to_with(&path, &codec);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(UserConfig::stored_web_secret_at(&path, &codec).as_deref(), Some(key));
        // Another account can't open it.
        assert_eq!(UserConfig::stored_web_secret_at(&path, &FakeCodec(0x43)), None);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // A file that isn't JSON stays where it is (loading moves it aside).
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(UserConfig::stored_web_secret_at(&path, &codec), None);
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        // Only the file and its backup: nothing was moved aside.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        // Test settings with a file of their own read it; ones in memory, and
        // ones that couldn't be read at startup, read nothing.
        let own = dir.join("own").join("config.json");
        let mut c = UserConfig::at_path(own.clone());
        assert_eq!(c.stored_web_secret(), None);
        c.web_secret = Some(key.into());
        c.save();
        assert_eq!(UserConfig::at_path(own).stored_web_secret().as_deref(), Some(key));
        assert_eq!(UserConfig::in_memory().stored_web_secret(), None);
        assert_eq!(UserConfig::unsaveable().stored_web_secret(), None);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn try_save_says_why_the_file_was_not_written() {
        let dir = temp_config_dir("try-save");
        let path = dir.join("config.json");
        let mut c = UserConfig::default();
        c.last_speaker_id = Some("uuid:RINCON_TRY".into());
        c.try_save_to_with(&path, &PlatformCodec).unwrap();
        assert_eq!(UserConfig::load_from(&path).last_speaker_id.as_deref(), Some("uuid:RINCON_TRY"));
        // A file that couldn't be read at startup is left as it is.
        let before = std::fs::read(&path).unwrap();
        let err = UserConfig::unsaveable().try_save_to_with(&path, &PlatformCodec).unwrap_err();
        assert!(err.contains("couldn't be read at startup"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Its folder can't be made: a file has the name.
        let err = c.try_save_to_with(&path.join("config.json"), &PlatformCodec).unwrap_err();
        assert!(err.starts_with("mkdir "), "{err}");
        // The temp file can't be written: the folder takes no new files.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let blocked = dir.join("blocked");
            std::fs::create_dir_all(&blocked).unwrap();
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();
            // Permissions don't apply when running as root: nothing to test.
            if std::fs::write(blocked.join("probe"), b"").is_err() {
                let err = c.try_save_to_with(&blocked.join("config.json"), &PlatformCodec).unwrap_err();
                assert!(err.starts_with("write "), "{err}");
            }
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // It can't replace the file, a folder here: the temp file goes.
        let occupied = dir.join("occupied").join("config.json");
        std::fs::create_dir_all(&occupied).unwrap();
        let err = c.try_save_to_with(&occupied, &PlatformCodec).unwrap_err();
        assert!(err.starts_with("replace "), "{err}");
        assert!(!occupied.with_extension("json.tmp").exists());
        // Test settings in memory: saved, and nothing written.
        let untouched = dir.join("in-memory").join("config.json");
        UserConfig::in_memory().try_save_to_with(&untouched, &PlatformCodec).unwrap();
        assert!(!untouched.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Settings told apart by `id`, with a secret that goes with them.
    fn numbered(id: &str) -> UserConfig {
        let mut c = UserConfig::default();
        c.last_speaker_id = Some(id.into());
        c.airplay_passwords.insert("airplay:aa".into(), format!("pw-{id}"));
        c
    }

    #[test]
    fn a_power_cut_at_any_moment_of_a_save_leaves_the_old_or_the_new_settings() {
        use crate::secret_store::tests::FakeCodec;
        use crate::settings_file::tests::PowerCutDisk;
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let codec = FakeCodec(0x42);
        // The disk after two saves.
        let start = || {
            let disk = PowerCutDisk::default();
            numbered("older").try_save_to_ops(&path, &codec, &disk).unwrap();
            numbered("old").try_save_to_ops(&path, &codec, &disk).unwrap();
            disk
        };
        let full = {
            let disk = start();
            disk.cut_after(usize::MAX);
            numbered("new").try_save_to_ops(&path, &codec, &disk).unwrap();
            disk.ops()
        };
        for n in 0..=full {
            let disk = start();
            disk.cut_after(n);
            let _ = numbered("new").try_save_to_ops(&path, &codec, &disk);
            let after = disk.after_power_cut();
            let loaded = UserConfig::load_with_ops(&path, &codec, &after);
            let id = loaded.last_speaker_id.clone();
            assert!(matches!(id.as_deref(), Some("old" | "new")), "power cut after {n} of {full}: {id:?}");
            assert_eq!(loaded.airplay_passwords["airplay:aa"], format!("pw-{}", id.unwrap()), "{n}");
            assert!(loaded.load_notice().is_empty(), "{n}: {:?}", loaded.load_notice());
        }
    }

    #[test]
    fn a_file_zeroed_by_a_power_cut_gives_way_to_the_backup() {
        use crate::secret_store::tests::FakeCodec;
        use crate::settings_file::tests::PowerCutDisk;
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let codec = FakeCodec(0x42);
        let mut disk = PowerCutDisk::default();
        numbered("before").try_save_to_ops(&path, &codec, &disk).unwrap();
        numbered("last").try_save_to_ops(&path, &codec, &disk).unwrap();
        // An earlier version saves the way it did (no flush before the
        // rename), and the power goes before the data is on the disk: the
        // file has its length and nothing but zeros.
        disk.cached_writes = true;
        let text = serde_json::to_string_pretty(&numbered("unflushed")).unwrap();
        let tmp = path.with_extension("json.tmp");
        disk.create_flushed(&tmp, text.as_bytes()).unwrap();
        disk.rename(&tmp, &path).unwrap();
        let after = disk.after_power_cut();
        assert!(after.get(&path).unwrap().iter().all(|&b| b == 0));
        let loaded = UserConfig::load_with_ops(&path, &codec, &after);
        // The backup: the settings this version saved last.
        assert_eq!(loaded.last_speaker_id.as_deref(), Some("last"));
        assert_eq!(loaded.airplay_passwords["airplay:aa"], "pw-last");
        let notice = loaded.load_notice();
        assert!(notice.restored_from_backup && !notice.all_reset);
        // The zeroed file is kept aside, never as the backup, and the
        // backup's settings are in config.json again.
        let aside = notice.unusable_kept_as.clone().unwrap();
        assert!(after.get(&aside).unwrap().iter().all(|&b| b == 0));
        let bak = after.get(&crate::settings_file::backup_path(&path)).unwrap();
        assert!(!bak.iter().all(|&b| b == 0));
        let again = UserConfig::load_with_ops(&path, &codec, &after);
        assert_eq!(again.last_speaker_id.as_deref(), Some("last"));
        assert!(again.load_notice().is_empty(), "told once");
    }

    /// Every file in `dir` with its text.
    fn folder_text(dir: &std::path::Path) -> Vec<(String, String)> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| {
                let text = String::from_utf8_lossy(&std::fs::read(e.path()).unwrap()).into_owned();
                (e.file_name().to_string_lossy().into_owned(), text)
            })
            .collect()
    }

    #[test]
    fn no_file_in_the_folder_keeps_a_secret_an_older_version_left_unsealed() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("plain-secrets");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let seed = "ab".repeat(32);
        // As a version from before sealing wrote it.
        std::fs::write(
            &path,
            format!(
                r#"{{"last_speaker_id":"x","airplay_passwords":{{"airplay:aa":"hunter2"}},
                    "airplay_pairings":{{"airplay:cc":{{"controller_id":"ctl",
                    "controller_seed_hex":"{seed}","accessory_id":"acc",
                    "accessory_ltpk_hex":"{}"}}}}}}"#,
                "cd".repeat(32)
            ),
        )
        .unwrap();
        // Loading seals them and saves at once.
        let c = UserConfig::load_from_with(&path, &FakeCodec(0x42));
        assert_eq!(c.airplay_passwords["airplay:aa"], "hunter2");
        let files = folder_text(&dir);
        assert!(files.iter().any(|(n, _)| n == "config.json.bak"), "{files:?}");
        for (name, text) in &files {
            assert!(!text.contains("hunter2") && !text.contains(&seed), "{name}: {text}");
        }
        // The mirror loads to the same settings as the file.
        let from_file = UserConfig::load_from_with(&path, &FakeCodec(0x42));
        let from_backup =
            UserConfig::load_from_with(&crate::settings_file::backup_path(&path), &FakeCodec(0x42));
        assert_eq!(serde_json::to_value(&from_backup).unwrap(), serde_json::to_value(&from_file).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_password_leaves_the_file_and_the_backup_with_the_same_save() {
        use crate::secret_store::tests::FakeCodec;
        let codec = FakeCodec(0x42);
        let dir = temp_config_dir("removed-password");
        let path = dir.join("config.json");
        let mut c = numbered("x");
        c.airplay_passwords.insert("airplay:bb".into(), "gone-soon".into());
        c.save_to_with(&path, &codec);
        for (name, text) in folder_text(&dir) {
            assert!(text.contains("/airplay_passwords/airplay:bb"), "{name}: sealed there before");
        }
        c.remove_airplay_password("airplay:bb");
        c.save_to_with(&path, &codec);
        for (name, text) in folder_text(&dir) {
            assert!(!text.contains("airplay:bb"), "{name}: {text}");
            assert!(text.contains("/airplay_passwords/airplay:aa"), "{name}: the other one stays");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_empty_or_broken_file_gives_way_to_a_good_backup() {
        use crate::secret_store::tests::FakeCodec;
        use crate::settings_file::tests::PowerCutDisk;
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let codec = FakeCodec(0x42);
        for (what, file) in [
            ("missing", None),
            ("empty", Some(&b""[..])),
            ("not JSON", Some(&b"{ \"last_speaker_id\": tru"[..])),
            ("not an object", Some(&b"[1, 2]"[..])),
        ] {
            let disk = PowerCutDisk::default();
            numbered("older").try_save_to_ops(&path, &codec, &disk).unwrap();
            numbered("good").try_save_to_ops(&path, &codec, &disk).unwrap();
            disk.remove(&path).unwrap();
            if let Some(bytes) = file {
                disk.create_flushed(&path, bytes).unwrap();
            }
            let loaded = UserConfig::load_with_ops(&path, &codec, &disk);
            assert_eq!(loaded.last_speaker_id.as_deref(), Some("good"), "{what}");
            assert!(loaded.load_notice().restored_from_backup, "{what}");
            assert_eq!(loaded.load_notice().unusable_kept_as.is_some(), file.is_some(), "{what}");
        }
        // Neither file usable: defaults, said so, both kept.
        let disk = PowerCutDisk::default();
        disk.create_flushed(&path, b"\0\0\0").unwrap();
        disk.create_flushed(&crate::settings_file::backup_path(&path), b"").unwrap();
        let loaded = UserConfig::load_with_ops(&path, &codec, &disk);
        assert_eq!(loaded.last_speaker_id, None);
        assert!(loaded.load_notice().all_reset && !loaded.load_notice().restored_from_backup);
        let aside = loaded.load_notice().unusable_kept_as.clone().unwrap();
        assert_eq!(disk.get(&aside).unwrap(), b"\0\0\0");
        // A fresh install has nothing to tell.
        let fresh = UserConfig::load_with_ops(&path, &codec, &PowerCutDisk::default());
        assert!(fresh.load_notice().is_empty());
    }

    #[test]
    fn one_value_that_cannot_be_read_costs_that_value_only() {
        let dir = temp_config_dir("one-value");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let text = r#"{"last_speaker_id":"x","prefer_realtime_airplay":"true","airplay_latency_ms":250.5,
            "speaker_settings":{"a":{"auto_connect":"when_idle","hidden":true},"b":{"hidden":true}},
            "manual_speakers":[{"kind":"air_play","host":"10.0.0.5","port":70000},
                               {"kind":"bluetooth","host":"10.0.0.6"},
                               {"kind":"upnp","host":5},
                               {"kind":"upnp","host":"http://10.0.0.7/d.xml"}],
            "airplay_passwords":{"airplay:aa":"pw","airplay:bb":7},
            "future_setting":{"anything":1}}"#;
        std::fs::write(&path, text).unwrap();
        let c = UserConfig::load_from(&path);
        assert_eq!(c.last_speaker_id.as_deref(), Some("x"));
        assert!(!c.prefer_realtime_airplay);
        assert_eq!(c.airplay_latency_ms, AIRPLAY_LATENCY_MS_DEFAULT);
        assert_eq!(c.speaker_settings["a"], SpeakerSettings { hidden: true, ..Default::default() });
        assert!(c.speaker_settings["b"].hidden);
        let hosts: Vec<(&str, u16)> = c.manual_speakers.iter().map(|m| (m.host.as_str(), m.port)).collect();
        assert_eq!(hosts, [("10.0.0.5", 0), ("http://10.0.0.7/d.xml", 0)]);
        assert_eq!(c.airplay_passwords.len(), 1);
        assert_eq!(c.airplay_passwords["airplay:aa"], "pw");
        assert!(c.unknown_keys.contains_key("future_setting"));
        assert!(path.exists(), "the file stays where it is");
        // The person is told which, and where the file as it was is kept.
        let keys = |paths: &[KeyPath]| -> Vec<String> { paths.iter().map(|p| secret_store::pointer(p)).collect() };
        let notice = c.load_notice().clone();
        assert_eq!(
            keys(&notice.reset),
            [
                "/airplay_latency_ms",
                "/airplay_passwords/airplay:bb",
                "/manual_speakers/0/port",
                "/manual_speakers/1",
                "/manual_speakers/2",
                "/prefer_realtime_airplay",
                "/speaker_settings/a/auto_connect",
            ]
        );
        assert!(!notice.restored_from_backup && !notice.all_reset && notice.unusable_kept_as.is_none());
        let copy = notice.reset_kept_as.clone().unwrap();
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        let found: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(kept, found, "the copy is the file as it was (nothing to seal off Windows)");
        // What was read is saved at once: the file holds what the app runs
        // with, and the next start has nothing to tell.
        let on_disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk["last_speaker_id"], "x");
        assert!(on_disk.get("prefer_realtime_airplay").is_some_and(|v| v == false), "{on_disk}");
        let again = UserConfig::load_from(&path);
        assert!(again.load_notice().is_empty(), "{:?}", again.load_notice());
        let copies = |dir: &std::path::Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.before-reset-"))
                .count()
        };
        assert_eq!(copies(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn values_that_cannot_be_read_stay_in_the_file_when_no_copy_can_be_kept() {
        use crate::settings_file::tests::PowerCutDisk;
        /// A disk on which no copy kept aside can be made.
        struct NoCopies(PowerCutDisk);
        impl FileOps for NoCopies {
            fn read(&self, p: &std::path::Path) -> std::io::Result<Vec<u8>> {
                self.0.read(p)
            }
            fn create_flushed(&self, p: &std::path::Path, b: &[u8]) -> std::io::Result<()> {
                if p.to_string_lossy().contains("before-reset") {
                    return Err(std::io::ErrorKind::PermissionDenied.into());
                }
                self.0.create_flushed(p, b)
            }
            fn rename(&self, f: &std::path::Path, t: &std::path::Path) -> std::io::Result<()> {
                self.0.rename(f, t)
            }
            fn remove(&self, p: &std::path::Path) -> std::io::Result<()> {
                self.0.remove(p)
            }
            fn create_dir_all(&self, d: &std::path::Path) -> std::io::Result<()> {
                self.0.create_dir_all(d)
            }
            fn list(&self, d: &std::path::Path) -> std::io::Result<Vec<String>> {
                self.0.list(d)
            }
            fn age(&self, p: &std::path::Path) -> Option<std::time::Duration> {
                self.0.age(p)
            }
        }
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let text = br#"{"last_speaker_id":"x","prefer_realtime_airplay":"true"}"#;
        let disk = NoCopies(PowerCutDisk::with_file(&path, text));
        let c = UserConfig::load_with_ops(&path, &PlatformCodec, &disk);
        assert_eq!(c.last_speaker_id.as_deref(), Some("x"));
        // Nothing is written over the file, now or later in the run, and the
        // window says why.
        assert!(c.try_save_to_ops(&path, &PlatformCodec, &disk).is_err());
        assert_eq!(disk.0.get(&path).unwrap(), text);
        let problem = c.save_health().problem().unwrap();
        assert!(matches!(problem, SaveProblem::CopyNotKept { .. }), "{problem:?}");
        assert_eq!(c.load_notice().reset_kept_as, None);
    }

    #[test]
    fn a_value_the_app_learns_again_is_dropped_quietly() {
        let dir = temp_config_dir("relearned");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let text = r#"{"last_speaker_id":"x","speaker_settings":{"uuid:a":{"hidden":true,"upnp_stream_profile":"flac"},
            "airplay:B":{"hidden":true,"airplay_opener":"something-newer"}}}"#;
        std::fs::write(&path, text).unwrap();
        let c = UserConfig::load_from(&path);
        assert!(c.speaker_settings["uuid:a"].hidden);
        assert_eq!(c.speaker_settings["uuid:a"].upnp_stream_profile, None);
        assert!(c.speaker_settings["airplay:B"].hidden);
        assert_eq!(c.speaker_settings["airplay:B"].airplay_opener, None);
        // Nothing to tell the person...
        assert!(c.load_notice().is_empty(), "{:?}", c.load_notice());
        assert_eq!(c.load_notice().describe(&no_names), None);
        // ...but the copy and the save happen as for any value left out.
        let copies: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.json.before-reset-"))
            .collect();
        assert_eq!(copies.len(), 1);
        let on_disk: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk["speaker_settings"]["uuid:a"]["upnp_stream_profile"], serde_json::Value::Null);
        // With a value the person chose dropped too, the notice names only that.
        std::fs::write(
            &path,
            r#"{"speaker_settings":{"uuid:a":{"hidden":"yes","upnp_stream_profile":"flac"}}}"#,
        )
        .unwrap();
        let c = UserConfig::load_from(&path);
        let names = |id: &str| (id == "uuid:a").then(|| "Kitchen".to_string());
        let said = c.load_notice().describe(&names).unwrap();
        assert!(said.contains("Kitchen: Hide this speaker from the list."), "{said}");
        assert!(!said.contains("stream"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_copy_kept_aside_holds_secrets_sealed() {
        use crate::secret_store::tests::FakeCodec;
        let dir = temp_config_dir("reset-copy-sealed");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        // As an older version wrote it, with a value this one can't read.
        std::fs::write(
            &path,
            r#"{"airplay_passwords":{"airplay:aa":"hunter2"},"prefer_realtime_airplay":"true"}"#,
        )
        .unwrap();
        let c = UserConfig::load_from_with(&path, &FakeCodec(0x42));
        assert_eq!(c.airplay_passwords["airplay:aa"], "hunter2");
        let copy = c.load_notice().reset_kept_as.clone().unwrap();
        for (name, text) in folder_text(&dir) {
            assert!(!text.contains("hunter2"), "{name}: {text}");
        }
        // The copy opens to the file as it was, rejected value included.
        let kept = UserConfig::stored_web_secret_at(&copy, &FakeCodec(0x42));
        assert_eq!(kept, None);
        let mut tree: serde_json::Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        secret_store::unseal(&mut tree, &FakeCodec(0x42));
        assert_eq!(tree["airplay_passwords"]["airplay:aa"], "hunter2");
        assert_eq!(tree["prefer_realtime_airplay"], "true");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sealed_value_that_opens_to_the_wrong_type_costs_that_value_only() {
        use crate::secret_store::tests::FakeCodec;
        let codec = FakeCodec(0x42);
        let dir = temp_config_dir("sealed-wrong-type");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        // Sealed as a later version might store it: a number, not text.
        let mut tree = serde_json::json!({
            "last_speaker_id": "x",
            "airplay_passwords": {"airplay:aa": "hunter2", "airplay:bb": 7}
        });
        secret_store::seal(&mut tree, &[], &codec);
        assert!(!tree.to_string().contains("hunter2"));
        std::fs::write(&path, tree.to_string()).unwrap();
        let c = UserConfig::load_from_with(&path, &codec);
        assert_eq!(c.last_speaker_id.as_deref(), Some("x"));
        assert_eq!(c.airplay_passwords.len(), 1);
        assert_eq!(c.airplay_passwords["airplay:aa"], "hunter2");
        let reset: Vec<String> = c.load_notice().reset.iter().map(|p| secret_store::pointer(p)).collect();
        assert_eq!(reset, ["/airplay_passwords/airplay:bb"]);
        // One that can't be opened at all is kept as it is, as before, and
        // isn't a reset (the file as it was: the load above saved without
        // the value it couldn't read).
        std::fs::write(&path, tree.to_string()).unwrap();
        let other = UserConfig::load_from_with(&path, &FakeCodec(0x43));
        assert_eq!(other.last_speaker_id.as_deref(), Some("x"));
        assert!(other.airplay_passwords.is_empty());
        assert!(other.load_notice().reset.is_empty());
        assert_eq!(other.locked_secrets.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Settings in which every field of every struct, and of every entry
    /// of every list and map, differs from what it is when left out. A
    /// field added to any of these types must be set here as well:
    /// `every_field_differs_from_its_default` fails until it is.
    fn everything_set() -> UserConfig {
        use crate::discovery_net::SavedAdapter;
        use crate::manual_speakers::ManualKind;
        let mut c = UserConfig::default();
        c.last_speaker_id = Some("uuid:RINCON_ALL".into());
        c.onboarding_dismissed = true;
        c.always_minimise_to_tray = true;
        c.donation_prompt_hidden_until = Some(1_800_000_000);
        c.auto_reconnect_on_launch = false;
        c.chose_this_pc = true;
        c.closed_for_update = true;
        c.pc_output = Some("{0.0.0.00000000}.{realtek}".into());
        c.pc_outputs_open = true;
        c.ma_level = Some(60);
        c.last_version_seen = Some("0.1.9".into());
        c.prefer_realtime_airplay = true;
        c.auto_reconnect_on_drop = false;
        c.airplay_mfi_encryption = true;
        c.airplay_passwords.insert("airplay:aa".into(), "hunter2".into());
        c.airplay_pairings.insert(
            "airplay:cc".into(),
            PairingCredentials {
                controller_id: "ctl".into(),
                controller_seed_hex: "ab".repeat(32),
                accessory_id: "acc".into(),
                accessory_ltpk_hex: "cd".repeat(32),
            },
        );
        c.airplay_controller_id = Some("ctl".into());
        c.airplay_controller_seed_hex = Some("ef".repeat(32));
        c.web_secret = Some("00112233445566778899aabbccddeeff".into());
        c.unknown_keys.insert("future_setting".into(), serde_json::json!({"x": 1}));
        c.forward_now_playing = true;
        c.airplay_uncompressed_alac = true;
        c.privacy_mode = true;
        c.airplay_latency_ms = 300;
        c.airplay_homepod_buffered = true;
        c.airplay_homepod_latency_ms = 150;
        c.check_for_updates = false;
        c.update_last_check_unix = Some(1_700_000_000);
        c.update_latest_tag = Some("v9.9.9".into());
        c.update_latest_url = Some("https://example.invalid/r".into());
        c.update_skipped_tag = Some("v9.9.8".into());
        c.update_banner_hidden_until = Some(1_700_000_100);
        c.sendspin = paired_sendspin();
        c.sendspin.source_name = Some("Office PC".into());
        c.sendspin.last_playback_server = Some("srv-a".into());
        c.sendspin.player_extra_latency_ms = 40;
        c.sendspin.pairing_code_failures = 2;
        c.manual_speakers.push(ManualSpeaker {
            kind: ManualKind::AirPlay,
            name: "Den".into(),
            host: "10.0.0.5".into(),
            port: 7000,
        });
        c.discovery_adapter = Some(SavedAdapter { key: "{GUID}".into(), name: "Ethernet".into() });
        c.speaker_settings.insert(
            "uuid:RINCON_ALL".into(),
            SpeakerSettings {
                hidden: true,
                auto_connect: AutoConnect::Always,
                airplay_opener: Some(crate::airplay::session::Opener::OptionsThenAuthSetup),
                airplay_latency_ms: Some(120),
                upnp_stream_profile: Some(StreamProfile::L16),
                delay_change_ms: -50,
                steady_delay_change_ms: 120,
                delay_choice: Some(crate::delay::DelayChoice::Short),
                level: Some(32),
                song_titles: Some(true),
                tone: Some(EqSettings { enabled: true, preamp_db: -1.0, bands_db: [2.0; crate::equalizer::EQ_BANDS] }),
                last_played: Some(1_790_000_000),
                volume_limit: Some(70),
                show_as: Some("Kitchen speaker".into()),
                unknown_keys: [("future_speaker_setting".to_string(), serde_json::json!({"x": 1}))].into_iter().collect(),
            },
        );
        c.equalizer = EqSettings { enabled: true, preamp_db: -3.0, bands_db: [1.5; crate::equalizer::EQ_BANDS] };
        c.equalizer.bands_db[0] = -2.0;
        c.show_lab = true;
        c.delay_at_once = true;
        c
    }

    /// What a field holds when nothing was set (left out, or the entry type
    /// has no default): null, false, 0, "", [] or {}.
    fn is_unset(v: &serde_json::Value) -> bool {
        use serde_json::Value;
        match v {
            Value::Null => true,
            Value::Bool(b) => !b,
            Value::Number(n) => n.as_f64() == Some(0.0),
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
        }
    }

    /// Check that `value` differs everywhere from `default` (the same place
    /// in the defaults; `None` where they have nothing, such as a list or
    /// map entry, which then must hold no unset value).
    fn assert_differs(value: &serde_json::Value, default: Option<&serde_json::Value>, path: &mut Vec<String>) {
        use serde_json::Value;
        // Entries of these maps have a default of their own to differ from.
        let entry_default = |path: &[String]| -> Option<Value> {
            (path.len() == 2 && path[0] == "speaker_settings")
                .then(|| serde_json::to_value(SpeakerSettings::default()).unwrap())
        };
        match (value, default) {
            (Value::Object(map), Some(Value::Object(d))) if !d.is_empty() => {
                for (k, dv) in d {
                    path.push(k.clone());
                    let v = map.get(k).unwrap_or_else(|| panic!("{path:?} not saved"));
                    assert_differs(v, Some(dv), path);
                    path.pop();
                }
                for (k, v) in map.iter().filter(|(k, _)| !d.contains_key(*k)) {
                    path.push(k.clone());
                    assert_differs(v, None, path);
                    path.pop();
                }
            }
            (Value::Object(map), _) => {
                assert!(!map.is_empty(), "{path:?}: put an entry here");
                for (k, v) in map {
                    path.push(k.clone());
                    let d = entry_default(path);
                    assert_differs(v, d.as_ref(), path);
                    path.pop();
                }
            }
            (Value::Array(items), Some(Value::Array(d))) if !d.is_empty() => {
                assert_eq!(items.len(), d.len(), "{path:?}");
                for (i, (v, dv)) in items.iter().zip(d).enumerate() {
                    path.push(i.to_string());
                    assert_differs(v, Some(dv), path);
                    path.pop();
                }
            }
            (Value::Array(items), _) => {
                assert!(!items.is_empty(), "{path:?}: put an entry here");
                for (i, v) in items.iter().enumerate() {
                    path.push(i.to_string());
                    assert_differs(v, None, path);
                    path.pop();
                }
            }
            (leaf, Some(d)) => assert_ne!(leaf, d, "{path:?} is still its default"),
            (leaf, None) => assert!(!is_unset(leaf), "{path:?} is unset"),
        }
    }

    #[test]
    fn every_field_differs_from_its_default_and_survives_a_save() {
        use crate::secret_store::tests::FakeCodec;
        let all = everything_set();
        let saved = serde_json::to_value(&all).unwrap();
        let defaults = serde_json::to_value(UserConfig::default()).unwrap();
        assert_differs(&saved, Some(&defaults), &mut Vec::new());
        // Saved (sealed) and loaded again: the whole value comes back.
        let dir = temp_config_dir("everything");
        let path = dir.join("config.json");
        all.try_save_to_with(&path, &FakeCodec(0x42)).unwrap();
        let back = UserConfig::load_from_with(&path, &FakeCodec(0x42));
        assert!(back.load_notice().is_empty(), "{:?}", back.load_notice());
        assert_eq!(serde_json::to_value(&back).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every key path in `tree`, each node once, the root left out.
    fn every_path(tree: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        let children: Vec<(String, &serde_json::Value)> = match tree {
            serde_json::Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v)).collect(),
            serde_json::Value::Array(a) => a.iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect(),
            _ => Vec::new(),
        };
        for (k, v) in children {
            path.push(k);
            out.push(path.clone());
            every_path(v, path, out);
            path.pop();
        }
    }

    fn node_mut<'a>(tree: &'a mut serde_json::Value, path: &[String]) -> &'a mut serde_json::Value {
        path.iter().fold(tree, |node, k| match node {
            serde_json::Value::Array(a) => &mut a[k.parse::<usize>().unwrap()],
            other => &mut other[k.as_str()],
        })
    }

    fn without(tree: &serde_json::Value, path: &[String]) -> serde_json::Value {
        let mut t = tree.clone();
        let (last, parent) = path.split_last().unwrap();
        match node_mut(&mut t, parent) {
            serde_json::Value::Array(a) => {
                a.remove(last.parse::<usize>().unwrap());
            }
            serde_json::Value::Object(m) => {
                m.remove(last);
            }
            _ => unreachable!(),
        }
        t
    }

    #[test]
    fn a_value_of_the_wrong_type_anywhere_costs_that_field_or_entry_only() {
        use crate::settings_file::tests::PowerCutDisk;
        let saved = serde_json::to_value(everything_set()).unwrap();
        let mut paths = Vec::new();
        every_path(&saved, &mut Vec::new(), &mut paths);
        assert!(paths.len() > 60, "{}", paths.len());
        // Fields with no default: an entry without one of them is left out.
        // Every other field must have a default, so a file from before the
        // field existed (or one where it can't be read) keeps the rest.
        let required = [
            &["airplay_pairings", "*", "controller_id"][..],
            &["airplay_pairings", "*", "controller_seed_hex"],
            &["airplay_pairings", "*", "accessory_id"],
            &["airplay_pairings", "*", "accessory_ltpk_hex"],
            &["manual_speakers", "*", "kind"],
            &["manual_speakers", "*", "host"],
            &["discovery_adapter", "key"],
            &["discovery_adapter", "name"],
            &["sendspin", "server_pairings", "*", "server_id"],
            &["sendspin", "server_pairings", "*", "psk"],
            &["sendspin", "player_pairings", "*", "psk"],
        ];
        let is_required = |p: &[String]| {
            required.iter().any(|r| r.len() == p.len() && r.iter().zip(p).all(|(r, k)| *r == "*" || r == k))
        };
        let in_an_object = |p: &[String]| {
            let mut tree = saved.clone();
            p.split_last().is_some_and(|(_, parent)| node_mut(&mut tree, parent).is_object())
        };
        for p in paths.iter().filter(|p| in_an_object(p)) {
            let strict = serde_json::from_value::<UserConfig>(without(&saved, p));
            assert_eq!(strict.is_err(), is_required(p), "{p:?}: a field left out must take its default");
        }
        let file = PathBuf::from("/profile/StreamToSpeaker/config.json");
        // Keys a newer version wrote are kept whatever they hold.
        let newer = |p: &[String]| p.iter().any(|k| k.starts_with("future_"));
        for p in paths.iter().filter(|p| !newer(p)) {
            let mut tree = saved.clone();
            let node = node_mut(&mut tree, p);
            *node = match node {
                serde_json::Value::String(_) => serde_json::json!(7),
                serde_json::Value::Object(_) => serde_json::json!(7),
                _ => serde_json::json!("x"),
            };
            let disk = PowerCutDisk::with_file(&file, serde_json::to_string(&tree).unwrap().as_bytes());
            let loaded = UserConfig::load_with_ops(&file, &PlatformCodec, &disk);
            let notice = loaded.load_notice();
            assert!(!notice.all_reset && !notice.restored_from_backup, "{p:?}: the file was put aside");
            assert!(disk.get(&file).is_some(), "{p:?}");
            assert_eq!(notice.reset.len(), 1, "{p:?}: {:?}", notice.reset);
            let lost = &notice.reset[0];
            assert!(p.starts_with(lost), "{p:?} cost {lost:?}");
            // What was lost is the smallest part without which the rest reads:
            // the value itself, or else its list or map entry.
            for depth in lost.len() + 1..=p.len() {
                let rest = without(&saved, &p[..depth]);
                assert!(serde_json::from_value::<UserConfig>(rest).is_err(), "{p:?}: {:?} would do", &p[..depth]);
            }
            let expected = serde_json::from_value::<UserConfig>(without(&saved, lost)).unwrap();
            assert_eq!(serde_json::to_value(&loaded).unwrap(), serde_json::to_value(&expected).unwrap(), "{p:?}");
            assert!(notice.reset_kept_as.is_some(), "{p:?}");
        }
    }

    #[test]
    fn a_failed_save_is_reported_until_a_save_succeeds() {
        use crate::settings_file::tests::PowerCutDisk;
        let file = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let disk = PowerCutDisk::default();
        let c = numbered("x");
        let health = c.save_health();
        c.try_save_to_ops(&file, &PlatformCodec, &disk).unwrap();
        assert_eq!(health.problem(), None);
        // The disk goes away: every save fails, and the problem stays.
        disk.cut_after(0);
        assert!(c.try_save_to_ops(&file, &PlatformCodec, &disk).is_err());
        c.clone().save_to_ops(&file, &PlatformCodec, &disk);
        let problem = health.problem().unwrap();
        assert!(matches!(problem, SaveProblem::Failed { .. }), "{problem:?}");
        assert!(problem.describe().starts_with("Settings can't be saved, so changes made now will be lost"));
        // It comes back: the next save clears it.
        disk.cut_after(usize::MAX);
        c.save_to_ops(&file, &PlatformCodec, &disk);
        assert_eq!(health.problem(), None);
    }

    #[test]
    fn a_save_under_the_settings_lock_is_written_once_the_lock_is_free() {
        let dir = temp_config_dir("deferred-save");
        let path = dir.join("config.json");
        let cell = std::sync::Arc::new(SettingsCell::new(UserConfig::at_path(path.clone())));
        {
            let mut uc = cell.lock().unwrap();
            uc.last_speaker_id = Some("first".into());
            uc.save();
            uc.last_speaker_id = Some("second".into());
            uc.save();
            assert!(!path.exists(), "nothing written while the lock is held");
            // Another thread can read the settings meanwhile without waiting
            // for this one's writes, and sees the lock held.
            let other = cell.clone();
            assert!(std::thread::spawn(move || other.0.try_lock().is_err()).join().unwrap());
        }
        // Written as the lock went: the newest state.
        assert_eq!(UserConfig::load_from(&path).last_speaker_id.as_deref(), Some("second"));
        assert!(cell.0.try_lock().is_ok());
        // Without the lock, a save is written at once, as before.
        let mut plain = UserConfig::at_path(path.clone());
        plain.last_speaker_id = Some("third".into());
        plain.save();
        assert_eq!(UserConfig::load_from(&path).last_speaker_id.as_deref(), Some("third"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_and_a_read_never_wait_for_another_save_being_written() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::{channel, Receiver, Sender};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        /// The real disk, whose first rename waits until it is let go: a
        /// slow flush.
        struct Slow {
            entered: Mutex<Sender<()>>,
            go: Mutex<Receiver<()>>,
            held: AtomicBool,
        }
        impl FileOps for Slow {
            fn read(&self, p: &std::path::Path) -> std::io::Result<Vec<u8>> {
                Disk.read(p)
            }
            fn create_flushed(&self, p: &std::path::Path, b: &[u8]) -> std::io::Result<()> {
                Disk.create_flushed(p, b)
            }
            fn rename(&self, f: &std::path::Path, t: &std::path::Path) -> std::io::Result<()> {
                if !self.held.swap(true, Ordering::SeqCst) {
                    let _ = self.entered.lock().unwrap().send(());
                    let _ = self.go.lock().unwrap().recv();
                }
                Disk.rename(f, t)
            }
            fn remove(&self, p: &std::path::Path) -> std::io::Result<()> {
                Disk.remove(p)
            }
            fn create_dir_all(&self, d: &std::path::Path) -> std::io::Result<()> {
                Disk.create_dir_all(d)
            }
            fn list(&self, d: &std::path::Path) -> std::io::Result<Vec<String>> {
                Disk.list(d)
            }
            fn age(&self, p: &std::path::Path) -> Option<std::time::Duration> {
                Disk.age(p)
            }
        }
        let dir = temp_config_dir("overlapping-saves");
        let path = dir.join("config.json");
        let cell = Arc::new(SettingsCell::new(UserConfig::at_path(path.clone())));
        let (entered_tx, entered) = channel();
        let (go, go_rx) = channel();
        let slow = Slow { entered: Mutex::new(entered_tx), go: Mutex::new(go_rx), held: AtomicBool::new(false) };
        // One save is being written, slowly.
        let first = {
            let mut uc = cell.lock().unwrap();
            uc.last_speaker_id = Some("first".into());
            uc.prepare(&path, &PlatformCodec, &Disk).unwrap().unwrap()
        };
        let writer = std::thread::spawn(move || first.run(&slow));
        entered.recv_timeout(Duration::from_secs(10)).expect("the first write started");
        // Meanwhile another thread changes a setting and saves: the save
        // returns at once, and the lock is free again for a reader.
        let (saved_tx, saved) = channel();
        let saver = {
            let cell = cell.clone();
            std::thread::spawn(move || {
                let mut uc = cell.lock().unwrap();
                uc.last_speaker_id = Some("second".into());
                uc.save();
                saved_tx.send(()).unwrap();
            })
        };
        saved.recv_timeout(Duration::from_secs(5)).expect("a save waited for another save's write");
        let (read_tx, read) = channel();
        let reader = {
            let cell = cell.clone();
            std::thread::spawn(move || read_tx.send(cell.lock().unwrap().last_speaker_id.clone()).unwrap())
        };
        let seen = read.recv_timeout(Duration::from_secs(5)).expect("a reader waited for a write");
        assert_eq!(seen.as_deref(), Some("second"));
        // Let the slow write finish: the newer save follows it.
        go.send(()).unwrap();
        writer.join().unwrap().unwrap();
        saver.join().unwrap();
        reader.join().unwrap();
        assert_eq!(UserConfig::load_from(&path).last_speaker_id.as_deref(), Some("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn of_two_saves_the_newer_state_stays_on_the_disk() {
        use crate::settings_file::tests::PowerCutDisk;
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let disk = PowerCutDisk::default();
        let mut c = numbered("older");
        let older = c.prepare(&path, &PlatformCodec, &disk).unwrap().unwrap();
        c.last_speaker_id = Some("newer".into());
        let newer = c.prepare(&path, &PlatformCodec, &disk).unwrap().unwrap();
        // Written in the other order (two threads that let go of the lock
        // in turn): the older one is not written over the newer.
        newer.run(&disk).unwrap();
        older.run(&disk).unwrap();
        assert_eq!(UserConfig::load_with_ops(&path, &PlatformCodec, &disk).last_speaker_id.as_deref(), Some("newer"));
    }

    #[test]
    fn a_failed_save_says_what_the_error_shows() {
        let failed = |e: std::io::Error| SaveProblem::failed(&e).describe();
        let refused = failed(std::io::ErrorKind::PermissionDenied.into());
        assert!(
            refused.contains("Windows refused to write config.json: the file or its folder may be read-only, or \
                 another program may be holding the file."),
            "{refused}"
        );
        let full = failed(std::io::ErrorKind::StorageFull.into());
        assert!(full.starts_with("Settings can't be saved because the disk is full"), "{full}");
        assert!(!full.contains("read-only"), "{full}");
        let other = failed(std::io::Error::other("The network path was not found."));
        assert!(other.ends_with("Details: The network path was not found."), "{other}");
        assert!(!other.contains("read-only") && !other.contains("disk is full"), "{other}");
        for text in [refused, full, other] {
            assert!(text.contains("changes made now will be lost when the app closes"), "{text}");
            assert!(text.contains("The app keeps trying"), "{text}");
        }
    }

    #[test]
    fn settings_that_could_not_be_read_say_so_from_the_start() {
        let c = UserConfig::unsaveable();
        let problem = c.save_health().problem().unwrap();
        assert!(matches!(problem, SaveProblem::NotRead { .. }));
        assert!(problem.describe().contains("changes made now won't be kept"), "{}", problem.describe());
        // Saves are refused, and the problem stays what it was.
        assert!(c.try_save_to_with(&std::env::temp_dir().join("never-written.json"), &PlatformCodec).is_err());
        assert_eq!(c.save_health().problem(), Some(problem));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_config_dir("not-read-health");
            let path = dir.join("config.json");
            numbered("x").save_to(&path);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read(&path).is_err() {
                let loaded = UserConfig::load_from(&path);
                assert!(matches!(loaded.save_health().problem(), Some(SaveProblem::NotRead { .. })));
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn no_names(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn every_setting_has_a_name_the_window_uses() {
        // Every setting, and every setting of a speaker, has a name for the
        // reset notice or is one the app learns again by itself.
        let defaults = serde_json::to_value(UserConfig::default()).unwrap();
        for key in defaults.as_object().unwrap().keys() {
            let path = [key.clone()];
            assert!(setting_label(key).is_some() || relearned(&path), "{key}: no label for the reset notice");
        }
        let speaker = serde_json::to_value(SpeakerSettings::default()).unwrap();
        for key in speaker.as_object().unwrap().keys() {
            let labelled = SPEAKER_LABELS.iter().any(|(k, _, _)| k == key);
            let path = ["speaker_settings".to_string(), "uuid:x".into(), key.clone()];
            assert!(labelled || relearned(&path), "speaker setting {key}: no label for the reset notice");
        }
        // The names said to be the window's are, word for word, in the
        // window's code.
        const WINDOW: &str = concat!(include_str!("gui.rs"), include_str!("model/text.rs"));
        let named = SETTING_LABELS.iter().chain(SPEAKER_LABELS).chain([&WHOLE_SPEAKER_LABEL]);
        for (key, label, in_window) in named {
            if *in_window {
                assert!(WINDOW.contains(label), "{key}: the window doesn't say {label:?}");
            }
        }
        // A speaker setting left out of the saved form while unset
        // (`skip_serializing_if`) isn't among the defaults' keys: the entry
        // of `everything_set()`, where every field is set, has them all.
        let all = everything_set();
        let (id, entry) = all.speaker_settings.iter().next().unwrap();
        let known = serde_json::to_value(entry).unwrap();
        let newer = |k: &String| entry.unknown_keys.contains_key(k);
        for key in known.as_object().unwrap().keys().filter(|k| !newer(k)) {
            let labelled = SPEAKER_LABELS.iter().any(|(k, _, _)| k == key);
            let path = ["speaker_settings".to_string(), id.clone(), key.clone()];
            assert!(labelled || relearned(&path), "speaker setting {key}: no label for the reset notice");
        }
    }

    #[test]
    fn what_a_load_found_is_said_in_plain_words() {
        let kept = |n: &str| Some(PathBuf::from("/profile/StreamToSpeaker").join(n));
        assert_eq!(LoadNotice::default().describe(&no_names), None);
        let restored = LoadNotice {
            restored_from_backup: true,
            unusable_kept_as: kept("config.json.corrupt-5"),
            ..Default::default()
        };
        assert_eq!(
            restored.describe(&no_names).unwrap(),
            "Your settings file was damaged, so the app used its backup copy instead. The damaged \
             file is kept as config.json.corrupt-5 in the settings folder."
        );
        let missing = LoadNotice { restored_from_backup: true, ..Default::default() };
        assert_eq!(
            missing.describe(&no_names).unwrap(),
            "Your settings file was missing, so the app used its backup copy instead."
        );
        let all = LoadNotice { all_reset: true, unusable_kept_as: kept("config.json.corrupt-6"), ..Default::default() };
        assert!(all.describe(&no_names).unwrap().contains("started with default settings"));
        assert!(all.describe(&no_names).unwrap().ends_with("kept as config.json.corrupt-6 in the settings folder."));
        let path = |p: &str| -> KeyPath { p.split('/').map(String::from).collect() };
        let some = LoadNotice {
            reset: vec![
                path("prefer_realtime_airplay"),
                path("speaker_settings/uuid:RINCON_1/auto_connect"),
                path("speaker_settings/uuid:RINCON_1/hidden"),
                path("airplay_passwords/airplay:aa"),
            ],
            reset_kept_as: kept("config.json.before-reset-7"),
            ..Default::default()
        };
        let names = |id: &str| (id == "uuid:RINCON_1").then(|| "Kitchen".to_string());
        assert_eq!(
            some.describe(&names).unwrap(),
            "Some settings couldn't be read and were reset or left out: AirPlay 2 stream mode; \
             Kitchen: Connect automatically; Kitchen: Hide this speaker from the list; A speaker: \
             AirPlay password. The file as it was is kept as config.json.before-reset-7 in the \
             settings folder."
        );
        // The same label once; more than four: a count.
        let many = LoadNotice {
            reset: ["equalizer/bands_db", "equalizer/enabled", "sendspin/enabled", "manual_speakers/0", "privacy_mode", "web_secret", "update_latest_tag"]
                .iter()
                .map(|p| path(p))
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            many.describe(&no_names).unwrap(),
            "Some settings couldn't be read and were reset or left out: Equalizer; Sendspin and Music \
             Assistant; Speakers added by address; Privacy mode; and 2 more."
        );
    }

    #[test]
    fn two_files_moved_aside_keep_both() {
        use crate::settings_file::tests::PowerCutDisk;
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let disk = PowerCutDisk::default();
        for text in [&b"{ first"[..], b"{ second"] {
            disk.create_flushed(&path, text).unwrap();
            UserConfig::load_with_ops(&path, &PlatformCodec, &disk);
        }
        let mut kept: Vec<Vec<u8>> = disk
            .names()
            .iter()
            .filter(|n| n.starts_with("config.json.corrupt-"))
            .map(|n| disk.get(&path.with_file_name(n)).unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, [b"{ first".to_vec(), b"{ second".to_vec()]);
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
        let mut names: Vec<_> =
            std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        names.sort();
        assert_eq!(names, ["config.json", "config.json.bak"], "nothing quarantined");
        // Rewritten without the mark at once, as every version reads it.
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"{"), "saved without a BOM");
        let back: UserConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.last_speaker_id.as_deref(), Some("uuid:RINCON_BOM"));
        // Only one: a second mark is not a config file this app wrote. It
        // is moved aside, and the backup (the file as it was before the
        // rewrite) is read instead.
        std::fs::write(&path, "\u{feff}\u{feff}{}").unwrap();
        let c = UserConfig::load_from(&path);
        assert_eq!(c.last_speaker_id.as_deref(), Some("uuid:RINCON_BOM"));
        assert!(c.load_notice().restored_from_backup);
        let kept = c.load_notice().unusable_kept_as.clone().unwrap();
        assert_eq!(std::fs::read(&kept).unwrap(), "\u{feff}\u{feff}{}".as_bytes());
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
        // No backup: an earlier version's file (with one, see
        // `an_unreadable_file_with_a_good_backup_runs_on_the_backup_without_saving`).
        std::fs::remove_file(crate::settings_file::backup_path(&path)).unwrap();
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
        let mut names: Vec<_> =
            std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        names.sort();
        assert_eq!(names, ["config.json"]);
        // The next launch reads it as it was.
        let next = UserConfig::load_from(&path);
        assert_eq!(next.last_speaker_id.as_deref(), Some("kept"));
        assert_eq!(next.airplay_passwords["airplay:aa"], "hunter2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_backup_is_never_overwritten_either() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_config_dir("unreadable-backup");
        let path = dir.join("config.json");
        numbered("a").save_to(&path);
        numbered("b").save_to(&path);
        let bak = crate::settings_file::backup_path(&path);
        let backup = std::fs::read(&bak).unwrap();
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&bak, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&bak).is_ok() {
            // Permissions don't apply (running as root): nothing to test.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let loaded = UserConfig::load_from(&path);
        assert_eq!(loaded.last_speaker_id, None);
        loaded.save_to(&path);
        loaded.clone().save_to(&path);
        std::fs::set_permissions(&bak, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"", "the broken file is left as it is");
        assert_eq!(std::fs::read(&bak).unwrap(), backup);
        // Readable at the next start: the backup is used.
        assert_eq!(UserConfig::load_from(&path).last_speaker_id.as_deref(), Some("b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_used_or_moved_aside_runs_on_the_backup_without_saving() {
        use crate::settings_file::tests::PowerCutDisk;
        /// A disk where `config.json` can't be renamed (another program has
        /// it open without sharing delete, say).
        struct Stuck(PowerCutDisk, PathBuf);
        impl FileOps for Stuck {
            fn read(&self, p: &std::path::Path) -> std::io::Result<Vec<u8>> {
                self.0.read(p)
            }
            fn create_flushed(&self, p: &std::path::Path, b: &[u8]) -> std::io::Result<()> {
                self.0.create_flushed(p, b)
            }
            fn rename(&self, f: &std::path::Path, t: &std::path::Path) -> std::io::Result<()> {
                if f == self.1 || t == self.1 {
                    return Err(std::io::ErrorKind::PermissionDenied.into());
                }
                self.0.rename(f, t)
            }
            fn remove(&self, p: &std::path::Path) -> std::io::Result<()> {
                self.0.remove(p)
            }
            fn create_dir_all(&self, d: &std::path::Path) -> std::io::Result<()> {
                self.0.create_dir_all(d)
            }
            fn list(&self, d: &std::path::Path) -> std::io::Result<Vec<String>> {
                self.0.list(d)
            }
            fn age(&self, p: &std::path::Path) -> Option<std::time::Duration> {
                self.0.age(p)
            }
        }
        let path = PathBuf::from("/profile/StreamToSpeaker/config.json");
        let inner = PowerCutDisk::default();
        numbered("saved").try_save_to_ops(&path, &PlatformCodec, &inner).unwrap();
        inner.remove(&path).unwrap();
        inner.create_flushed(&path, b"\0\0\0\0").unwrap();
        let disk = Stuck(inner, path.clone());
        let c = UserConfig::load_with_ops(&path, &PlatformCodec, &disk);
        assert_eq!(c.last_speaker_id.as_deref(), Some("saved"), "the backup's settings");
        assert!(c.try_save_to_ops(&path, &PlatformCodec, &disk).is_err());
        assert_eq!(disk.0.get(&path).unwrap(), b"\0\0\0\0", "never written over");
        let problem = c.save_health().problem().unwrap();
        assert_eq!(problem, SaveProblem::NotRead { why: problem_why(&problem), damaged: true, on_backup: true });
        assert!(problem.describe().starts_with(
            "Your settings file is damaged and couldn't be moved aside, so the app is using its backup copy"
        ), "{}", problem.describe());
    }

    fn problem_why(p: &SaveProblem) -> String {
        match p {
            SaveProblem::NotRead { why, .. } | SaveProblem::Failed { why, .. } | SaveProblem::CopyNotKept { why } => {
                why.clone()
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_with_a_good_backup_runs_on_the_backup_without_saving() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_config_dir("unreadable-with-backup");
        let path = dir.join("config.json");
        numbered("saved").save_to(&path);
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Permissions don't apply (running as root): nothing to test.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let c = UserConfig::load_from(&path);
        assert_eq!(c.last_speaker_id.as_deref(), Some("saved"));
        c.save_to(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let problem = c.save_health().problem().unwrap();
        assert!(matches!(problem, SaveProblem::NotRead { damaged: false, on_backup: true, .. }), "{problem:?}");
        assert!(problem.describe().starts_with("Your settings file couldn't be read, so the app is using its backup copy"));
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
    fn an_unknown_or_malformed_stream_profile_means_nothing_remembered() {
        // A later version's profile name, a wrong type, garbage: that one
        // field reads as nothing remembered; nothing else is lost.
        for value in [r#""flac""#, "42", r#"{"x":1}"#, "[]", "null"] {
            let text = format!(
                r#"{{"last_speaker_id":"uuid:a","airplay_latency_ms":700,"speaker_settings":{{
                    "uuid:a":{{"hidden":true,"upnp_stream_profile":{value},"airplay_latency_ms":150}},
                    "uuid:b":{{"upnp_stream_profile":"wav"}}}}}}"#
            );
            let c = from_text(&text);
            assert_eq!(c.last_speaker_id.as_deref(), Some("uuid:a"), "{value}");
            assert_eq!(c.airplay_latency_ms, 700, "{value}");
            let a = &c.speaker_settings["uuid:a"];
            assert!(a.hidden, "{value}");
            assert_eq!((a.upnp_stream_profile, a.airplay_latency_ms), (None, Some(150)), "{value}");
            assert_eq!(c.speaker_settings["uuid:b"].upnp_stream_profile, Some(StreamProfile::Wav), "{value}");
        }
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
        let c = from_text(r#"{"last_speaker_id":"x","equalizer":{"bands_db":[1,2,3],"enabled":true}}"#);
        assert_eq!(c.last_speaker_id.as_deref(), Some("x"));
        assert_eq!(c.equalizer, EqSettings { enabled: true, ..EqSettings::default() });
        // Out-of-range values are clamped onto the grid.
        let c = from_text(r#"{"equalizer":{"enabled":true,"preamp_db":40.2}}"#);
        assert!(c.equalizer.enabled);
        assert_eq!(c.equalizer.preamp_db, 0.0);
    }
}
