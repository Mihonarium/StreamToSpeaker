//! mDNS-based discovery of AirPlay receivers.
//!
//! AirPlay devices advertise (up to) two Bonjour services:
//!
//!   * `_raop._tcp.local.` — the classic **R**emote **A**udio **O**utput
//!     **P**rotocol (AirPlay 1) audio endpoint. Service name is
//!     `<MAC>@<FriendlyName>`. Carries the `et`/`cn`/`sr`/… TXT keys.
//!   * `_airplay._tcp.local.` — the AirPlay **2** endpoint. Service name
//!     is just `<FriendlyName>`; the MAC lives in the `deviceid` TXT key.
//!     Carries the 64-bit `features`/`ft` flag word, `pk` (the device's
//!     Ed25519 HomeKit public key), `model`, `srcvers`, etc.
//!
//! A modern receiver (HomePod, Apple TV, AirPort Express, Sonos in AP2
//! mode) advertises **both**. We browse both service types and correlate
//! records by MAC so a single [`AirPlayRenderer`] carries everything we
//! need to decide *how* to talk to it:
//!
//!   * **Legacy RAOP** — for devices that accept the unencrypted (`et=0`)
//!     or Apple-RSA (`et=1`) AirPlay-1 flow. Handled by `rtsp.rs`/`rtp.rs`.
//!   * **AirPlay 2 / HomeKit** — for HomePod and AP2-only receivers that
//!     require HomeKit pairing + ChaCha20-Poly1305. Handled by the
//!     `pairing` / `ap2` path.
//!
//! ## RAOP TXT keys (`_raop._tcp`)
//!
//! | Key | Meaning                                              |
//! |-----|------------------------------------------------------|
//! | `cn`      | Comma-separated codecs: 0=PCM 1=ALAC 2=AAC ... |
//! | `et`      | Comma-separated encryption types: 0=none 1=RSA 3=FairPlay 4=FairPlay-SAPv2.5 5=MFi |
//! | `vn`      | RSA version (3 = the published Apple key)       |
//! | `pw`      | Password protected                              |
//! | `am`      | Apple model — purely informational              |
//!
//! ## AirPlay 2 TXT keys (`_airplay._tcp`)
//!
//! | Key | Meaning                                                  |
//! |-----|----------------------------------------------------------|
//! | `deviceid`  | MAC address `AA:BB:CC:DD:EE:FF` — our correlation key |
//! | `features`/`ft` | 64-bit capability bitfield (see [`features`])     |
//! | `flags`     | status flags                                         |
//! | `pk`        | device HomeKit Ed25519 public key (hex)              |
//! | `model`     | e.g. `AudioAccessory5,1` (HomePod), `AppleTV6,2`     |
//! | `srcvers`   | AirPlay source version, e.g. `366.0`                 |
//! | `gid`       | group UUID (see [`GroupHints`])                      |
//! | `igl`       | is group leader (`1`/`0`)                            |
//! | `gcgl`      | group contains (discoverable) group leader           |
//! | `gpn`       | group public name — what the iPhone picker shows     |
//! | `pgid`/`pgcgl` | parent group UUID / parent contains leader        |
//! | `tsid`      | tight-sync (stereo pair) UUID                        |

use anyhow::Result;
use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperties};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Service type for AirPlay 1 / RAOP audio receivers.
pub const RAOP_SERVICE: &str = "_raop._tcp.local.";
/// Service type for AirPlay 2 receivers.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

// ---------------------------------------------------------------------------
// Feature-flag bits (subset we care about). Bit numbers verified against
// OwnTone's `outputs/airplay.c` features map.
// ---------------------------------------------------------------------------

/// Bit 9 — `SupportsAirPlayAudio`. Set by every AirPlay-2 audio receiver.
pub const FEAT_AUDIO: u64 = 1 << 9;
/// Bit 40 — `SupportsBufferedAudio` (srcvers ≥ 354.54.6).
pub const FEAT_BUFFERED_AUDIO: u64 = 1 << 40;
/// Bit 41 — `SupportsPTP`. Devices with this expect IEEE-1588 PTP timing.
pub const FEAT_PTP: u64 = 1 << 41;
/// Bit 27 — `SupportsLegacyPairing`.
pub const FEAT_LEGACY_PAIRING: u64 = 1 << 27;
/// Bit 43 — `SupportsSystemPairing`.
pub const FEAT_SYSTEM_PAIRING: u64 = 1 << 43;
/// Bit 46 — `SupportsHKPairingAndAccessControl`.
pub const FEAT_HK_PAIRING: u64 = 1 << 46;
/// Bit 48 — `SupportsCoreUtilsPairingAndEncryption`. When set the device
/// accepts HomeKit *transient* pairing (PIN-less, the path we use).
pub const FEAT_TRANSIENT_PAIRING: u64 = 1 << 48;

/// Which audio transport we'll use to talk to a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// AirPlay 1 / RAOP: unencrypted (`et=0`) or RSA-AES (`et=1`), no
    /// pairing. The existing `rtsp.rs` + `rtp.rs` path handles this.
    RaopLegacy,
    /// AirPlay 2 with HomeKit transient pairing + ChaCha20-Poly1305.
    /// Required by HomePod and AP2-only receivers.
    AirPlay2,
}

/// One resolved AirPlay receiver, merged across both service types.
#[derive(Debug, Clone)]
pub struct AirPlayRenderer {
    /// Friendly name shown to the user.
    pub friendly_name: String,
    /// Stable ID — the MAC address, normalised to upper-case hex with no
    /// separators (`B8E93735AC11`). Survives device renames / DHCP churn.
    pub mac_id: String,
    /// IPv4 of the receiver (we prefer v4 over v6 for socket simplicity).
    pub ip: IpAddr,
    /// RTSP port advertised by `_raop._tcp` (the legacy audio endpoint).
    /// 0 if the device only advertises `_airplay._tcp`.
    pub port: u16,
    /// RTSP port advertised by `_airplay._tcp` (the AirPlay-2 endpoint),
    /// if present (usually 7000).
    pub airplay_port: Option<u16>,
    /// Encryption types from RAOP `et=` (0=none 1=RSA 3/4/5=FairPlay).
    pub encryption_types: Vec<u8>,
    /// Codec ids from RAOP `cn=` (1=ALAC).
    pub codecs: Vec<u8>,
    /// True if the RAOP service set `pw=true` (password protected). We
    /// don't speak RAOP HTTP-digest auth, so these are legacy-unsupported.
    pub password_protected: bool,
    /// The RAOP `ek=1` flag — "encryption key (expected)". Classic
    /// AirPort Express advertises it and genuinely wants an RSA-wrapped
    /// key; shairport advertises it too but also accepts plaintext. We
    /// use it (with the model) to decide whether to encrypt (see
    /// [`AirPlayRenderer::prefers_rsa_encryption`]).
    pub encryption_key_required: bool,
    /// 64-bit AirPlay-2 `features`/`ft` bitfield, if the device advertised
    /// `_airplay._tcp`.
    pub features: Option<u64>,
    /// Device HomeKit public key (`pk`, hex) from `_airplay._tcp`.
    pub pk: Option<String>,
    /// Model string (`am=` on RAOP or `model` on AirPlay), e.g.
    /// `AudioAccessory5,1` for a HomePod.
    pub model: Option<String>,
    /// Group-membership hints from `_airplay._tcp` (all optional).
    pub group: GroupHints,
}

/// AirPlay-2 group hints, straight from the `_airplay._tcp` TXT record.
///
/// Field semantics per the openairplay service-discovery table, cross-
/// checked against real captures (owntone#1413 — HomePod stereo pair and
/// a user-built multi-room group; spoticonn — Apple TV with HomePods as
/// its default audio output):
///
/// * `gid` — the group UUID. Every AP2 receiver advertises one, even
///   standalone (Sonos / AirPort Express set `gid = pi`, `gcgl=0`), so a
///   shared `gid` — not its presence — is what makes a group. A HomePod
///   stereo pair suffixes it (`<tsid>+0+<uuid>` / `<tsid>+1+<uuid>`);
///   we strip everything from the first `+` so the halves match.
/// * `pgid` — parent group UUID when a pair is nested in a larger group;
///   it takes precedence as the grouping key.
/// * `igl` — "is group leader". An idle Apple TV advertises `igl=1
///   gcgl=1`; HomePods set as that TV's default output share its `gid`
///   with `igl=0 gcgl=1`. **Both** halves of a stereo pair advertise
///   `igl=1` (the tight-sync sub-group), and the hints are not
///   advertised continuously, so `igl` alone never decides a leader —
///   see [`pick_group_leader`]. Sonos spells the key `isGroupLeader`.
/// * `gcgl` — group contains a discoverable leader.
/// * `gpn` — the group's public name (the iPhone picker's label).
/// * `tsid` — tight-sync UUID, present on stereo-pair members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupHints {
    /// `gid`, normalised (lower-case, `+` suffix stripped); `None` when
    /// absent, empty or the all-zero UUID.
    pub group_id: Option<String>,
    /// `pgid`, normalised like `group_id`.
    pub parent_group_id: Option<String>,
    /// `igl` / `isGroupLeader`.
    pub is_group_leader: Option<bool>,
    /// `gcgl`.
    pub group_contains_leader: Option<bool>,
    /// `pgcgl`.
    pub parent_group_contains_leader: Option<bool>,
    /// `gpn`.
    pub group_name: Option<String>,
    /// `tsid`.
    pub tight_sync_id: Option<String>,
}

impl GroupHints {
    /// The key two receivers must share to be in one group: the parent
    /// group when nested, else the group itself (owntone#1413's rule).
    pub fn grouping_key(&self) -> Option<&str> {
        self.parent_group_id
            .as_deref()
            .or(self.group_id.as_deref())
    }

    /// True if any group key was advertised at all.
    pub fn any(&self) -> bool {
        self.group_id.is_some()
            || self.parent_group_id.is_some()
            || self.is_group_leader.is_some()
            || self.group_contains_leader.is_some()
            || self.group_name.is_some()
    }
}

/// One row of the AirPlay speaker list after group folding: either a
/// plain device, a group (the leader plus the members folded under it),
/// or a member hidden under a leader. Built by [`group_renderers`].
#[derive(Debug, Clone)]
pub struct AirPlayEntry {
    /// The device a click on this row targets. For a group row this is
    /// the group leader.
    pub renderer: AirPlayRenderer,
    /// The other members of the group this renderer leads, sorted by
    /// name. Empty for a plain device.
    pub led_members: Vec<AirPlayRenderer>,
    /// `Some(leader stable id)` when this renderer is a member that was
    /// folded under a group row — hidden from the list by default.
    pub folded_under: Option<String>,
    /// Names of same-group peers when the group could NOT be collapsed
    /// (no unambiguous leader, e.g. a HomePod stereo pair). Purely
    /// informational; the row is a plain device.
    pub ungrouped_peers: Vec<String>,
}

impl AirPlayEntry {
    fn plain(renderer: AirPlayRenderer) -> Self {
        Self {
            renderer,
            led_members: Vec::new(),
            folded_under: None,
            ungrouped_peers: Vec::new(),
        }
    }

    /// True if this row stands for a group (leader + ≥1 folded member).
    pub fn is_group(&self) -> bool {
        !self.led_members.is_empty()
    }

    /// Row label: the group's public name (`gpn`) for a group, else the
    /// device's friendly name.
    pub fn display_name(&self) -> String {
        if self.is_group() {
            if let Some(n) = self.renderer.group.group_name.as_deref() {
                let n = n.trim();
                if !n.is_empty() {
                    return n.to_string();
                }
            }
        }
        self.renderer.friendly_name.clone()
    }

    /// Names of every device in the group, leader first.
    pub fn member_names(&self) -> Vec<String> {
        let mut v = vec![self.renderer.friendly_name.clone()];
        v.extend(self.led_members.iter().map(|m| m.friendly_name.clone()));
        v
    }

    /// The transport a click on this row uses. A group leader is driven
    /// over AirPlay 2 whenever it can be (see
    /// [`AirPlayRenderer::transport_as_group_leader`]); everything else
    /// is exactly [`AirPlayRenderer::transport`].
    pub fn transport(&self) -> Option<Transport> {
        if self.is_group() {
            self.renderer.transport_as_group_leader()
        } else {
            self.renderer.transport()
        }
    }
}

impl AirPlayRenderer {
    /// Stable identifier used by the UI / persistence layer. Prefixed so
    /// it can't collide with the UPnP `Renderer::stable_id()`.
    pub fn stable_id(&self) -> String {
        format!("airplay:{}", self.mac_id)
    }

    /// True if this device exposes a usable **legacy RAOP** path: a RAOP
    /// port, ALAC, no password, and either no-encryption or Apple-RSA
    /// (i.e. not a FairPlay-only `et`).
    pub fn supports_legacy_raop(&self) -> bool {
        if self.port == 0 {
            return false;
        }
        if !self.codecs.contains(&1) {
            return false;
        }
        // Password-protected (`pw=true`) receivers ARE supported — we
        // speak RTSP Digest auth (the session uses the stored password,
        // or the UI prompts for one).
        self.encryption_types.contains(&0) || self.encryption_types.contains(&1)
    }

    /// Whether to encrypt the audio with the RSA-wrapped-key path rather
    /// than stream plaintext. Reference behaviour (OwnTone/libraop):
    /// classic AirPort Express genuinely *requires* AES, while sending
    /// RSA keys to a device that only does plaintext breaks it — so we
    /// only turn it on for the receiver classes that want it:
    ///
    ///   * the device advertised `ek=1` (AirPort Express gen1, shairport —
    ///     the latter also accepts plaintext, so RSA is safe there), or
    ///   * the model is a gen2 802.11n AirPort Express (`am=AirPort4,x`;
    ///     OwnTone's `APEX2` class, which it force-encrypts).
    ///
    /// **A device advertising `et=4` (the AirPlay-2 era, incl. Sonos and
    /// the AP2 `AirPort10,x`) always takes plaintext** — matching OwnTone,
    /// which force-disables encryption for those. So `et=1` must be on
    /// offer AND `et=4` must be absent. Everything else — Apple TV,
    /// third-party `et=0,4`, and non-AirPort models — stays plaintext.
    pub fn prefers_rsa_encryption(&self) -> bool {
        if !self.encryption_types.contains(&1) || self.encryption_types.contains(&4) {
            return false;
        }
        if self.encryption_key_required {
            return true;
        }
        // OwnTone's APEX2 (gen2 802.11n) is exactly `am=AirPort4,x`; other
        // `AirPort*` models (APEX3, e.g. AirPort10,x) it streams plaintext.
        self.model
            .as_deref()
            .map(|m| m.starts_with("AirPort4"))
            .unwrap_or(false)
    }

    /// True if this device is reachable via the AirPlay-2 HomeKit path:
    /// it advertises `_airplay._tcp` audio support plus a HomeKit
    /// pairing/encryption capability we can satisfy (transient pairing).
    pub fn supports_airplay2(&self) -> bool {
        let Some(ft) = self.features else {
            return false;
        };
        if self.airplay_port.is_none() {
            return false;
        }
        if ft & FEAT_AUDIO == 0 {
            return false;
        }
        // We implement *transient* pairing, advertised by bit 48; bit 46
        // (HK pairing + access control) is the broader capability HomePods
        // and Apple TVs set. Either is sufficient for us to attempt it.
        ft & (FEAT_TRANSIENT_PAIRING | FEAT_HK_PAIRING) != 0
    }

    /// True if this device *requires* the AirPlay-2 path — i.e. it won't
    /// play via legacy RAOP. HomePods (model `AudioAccessory*`) gate all
    /// audio on HomeKit pairing even though they still advertise a RAOP
    /// service, so we route them to AP2 regardless of their `et` list.
    pub fn requires_airplay2(&self) -> bool {
        if self.is_homepod() {
            return true;
        }
        // No usable legacy path but a usable AP2 path ⇒ AP2 is required.
        self.supports_airplay2() && !self.supports_legacy_raop()
    }

    /// HomePod / HomePod mini detection by model prefix.
    pub fn is_homepod(&self) -> bool {
        self.model
            .as_deref()
            .map(|m| m.starts_with("AudioAccessory"))
            .unwrap_or(false)
    }

    /// Apple TV detection by model prefix (the openairplay subtype rule).
    pub fn is_apple_tv(&self) -> bool {
        self.model
            .as_deref()
            .map(|m| m.starts_with("AppleTV"))
            .unwrap_or(false)
    }

    /// Transport for a device that LEADS a group (an Apple TV with
    /// HomePods as its default audio output). Its legacy `_raop._tcp`
    /// service accepts a session but the audio never reaches the
    /// group's speakers — the tvOS system AirPlay-1 receiver doesn't
    /// forward to the default output (Rogue Amoeba documents exactly this
    /// "connects, no audio" symptom for Airfoil). iPhones drive such a
    /// group through the leader's AirPlay-2 endpoint, so that is what we
    /// use whenever the leader advertises one; otherwise the plain
    /// per-device choice applies.
    pub fn transport_as_group_leader(&self) -> Option<Transport> {
        if self.supports_airplay2() {
            Some(Transport::AirPlay2)
        } else {
            self.transport()
        }
    }

    /// True if the device expects IEEE-1588 PTP timing (feature bit 41).
    /// HomePods set this; many older AP2 devices accept NTP instead.
    pub fn expects_ptp(&self) -> bool {
        self.features.map(|f| f & FEAT_PTP != 0).unwrap_or(false)
    }

    /// True if the device supports buffered audio (feature bit 40) — the
    /// TCP type-103 stream modern iOS senders use. Current Sonos firmware
    /// appears to *only* actually play this stream kind.
    pub fn supports_buffered_audio(&self) -> bool {
        self.features.map(|f| f & FEAT_BUFFERED_AUDIO != 0).unwrap_or(false)
    }

    /// The transport we'll use for this device, if any. Prefers the
    /// proven legacy RAOP path for devices that support it and don't
    /// *require* AP2 — that keeps Apple TV / Sonos / AirPort Express on
    /// the well-tested code path and reserves the AP2 path for HomePods
    /// and AP2-only receivers.
    pub fn transport(&self) -> Option<Transport> {
        if self.requires_airplay2() {
            return self.supports_airplay2().then_some(Transport::AirPlay2);
        }
        if self.supports_legacy_raop() {
            return Some(Transport::RaopLegacy);
        }
        if self.supports_airplay2() {
            return Some(Transport::AirPlay2);
        }
        None
    }

    /// True if we have *any* path to this device.
    pub fn is_supported(&self) -> bool {
        self.transport().is_some()
    }
}

// ---------------------------------------------------------------------------
// Per-service partial records. We keep RAOP and AirPlay info in separate
// maps keyed by normalised MAC, then merge on read so resolution order
// (and one service vanishing) can't corrupt the other half.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RaopInfo {
    friendly_name: String,
    ip: Option<IpAddr>,
    port: u16,
    encryption_types: Vec<u8>,
    codecs: Vec<u8>,
    password_protected: bool,
    encryption_key_required: bool,
    model: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct AirPlayInfo {
    friendly_name: String,
    ip: Option<IpAddr>,
    port: u16,
    features: Option<u64>,
    pk: Option<String>,
    model: Option<String>,
    group: GroupHints,
}

/// Shared discovery state. Mirrors `ssdp::DiscoveryState` semantics.
#[derive(Default)]
pub struct AirPlayDiscoveryState {
    raop: Mutex<HashMap<String, RaopInfo>>,
    airplay: Mutex<HashMap<String, AirPlayInfo>>,
}

impl AirPlayDiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot of currently-known receivers, sorted by friendly name.
    pub fn renderers(&self) -> Vec<AirPlayRenderer> {
        let raop = self.raop.lock().unwrap();
        let airplay = self.airplay.lock().unwrap();
        let mut macs: HashSet<String> = HashSet::new();
        macs.extend(raop.keys().cloned());
        macs.extend(airplay.keys().cloned());

        let mut v: Vec<AirPlayRenderer> = macs
            .into_iter()
            .filter_map(|mac| merge(&mac, raop.get(&mac), airplay.get(&mac)))
            .collect();
        v.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        v
    }

    /// The speaker list with AirPlay groups folded: one row per group
    /// (targeting the leader), members hidden under it. See
    /// [`group_renderers`].
    pub fn entries(&self) -> Vec<AirPlayEntry> {
        group_renderers(self.renderers())
    }

    /// The list row for one device id — a group row if that device leads
    /// a group, a folded-member row if it was hidden under one.
    pub fn entry_by_id(&self, id: &str) -> Option<AirPlayEntry> {
        self.entries()
            .into_iter()
            .find(|e| e.renderer.stable_id() == id)
    }

    /// Find by our public `stable_id()` (i.e. with the `airplay:` prefix).
    pub fn find_by_id(&self, id: &str) -> Option<AirPlayRenderer> {
        let mac = id.strip_prefix("airplay:")?.to_string();
        let raop = self.raop.lock().unwrap();
        let airplay = self.airplay.lock().unwrap();
        merge(&mac, raop.get(&mac), airplay.get(&mac))
    }

    fn upsert_raop(&self, mac: String, info: RaopInfo) {
        self.raop.lock().unwrap().insert(mac, info);
    }

    fn upsert_airplay(&self, mac: String, info: AirPlayInfo) {
        self.airplay.lock().unwrap().insert(mac, info);
    }

    fn remove(&self, mac: &str, service: &str) {
        if service == RAOP_SERVICE {
            self.raop.lock().unwrap().remove(mac);
        } else {
            self.airplay.lock().unwrap().remove(mac);
        }
    }
}

/// Build a public [`AirPlayRenderer`] from whichever partial records we
/// have for one MAC. Returns None if neither half has a usable IPv4.
fn merge(mac: &str, raop: Option<&RaopInfo>, airplay: Option<&AirPlayInfo>) -> Option<AirPlayRenderer> {
    let ip = raop
        .and_then(|r| r.ip)
        .or_else(|| airplay.and_then(|a| a.ip))?;

    // Prefer the AirPlay-2 instance name (the user-set friendly name);
    // fall back to the RAOP `@`-suffix name.
    let friendly_name = airplay
        .map(|a| a.friendly_name.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| raop.map(|r| r.friendly_name.clone()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| mac.to_string());

    Some(AirPlayRenderer {
        friendly_name,
        mac_id: mac.to_string(),
        ip,
        port: raop.map(|r| r.port).unwrap_or(0),
        airplay_port: airplay.map(|a| a.port),
        encryption_types: raop.map(|r| r.encryption_types.clone()).unwrap_or_default(),
        codecs: raop.map(|r| r.codecs.clone()).unwrap_or_default(),
        password_protected: raop.map(|r| r.password_protected).unwrap_or(false),
        encryption_key_required: raop.map(|r| r.encryption_key_required).unwrap_or(false),
        features: airplay.and_then(|a| a.features),
        pk: airplay.and_then(|a| a.pk.clone()),
        model: airplay
            .and_then(|a| a.model.clone())
            .or_else(|| raop.and_then(|r| r.model.clone())),
        group: airplay.map(|a| a.group.clone()).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Group folding
// ---------------------------------------------------------------------------

/// Fold discovered receivers into list rows by their advertised group.
///
/// Devices sharing a [`GroupHints::grouping_key`] form a group. A group
/// collapses into ONE row — named after `gpn`, targeting the leader —
/// only when [`pick_group_leader`] finds an unambiguous leader among the
/// members in range; the other members become hidden rows
/// (`folded_under`). Groups without such a leader (a lone HomePod
/// stereo pair, whose halves both claim `igl=1`; or hints missing) stay
/// as individual rows, each noting its peers, i.e. exactly today's
/// behaviour. Devices with no group key, or whose key nobody else
/// shares (every standalone AP2 receiver advertises a private `gid`),
/// are plain rows. Output is sorted by row label.
pub fn group_renderers(renderers: Vec<AirPlayRenderer>) -> Vec<AirPlayEntry> {
    let mut by_key: HashMap<String, Vec<AirPlayRenderer>> = HashMap::new();
    let mut plain: Vec<AirPlayRenderer> = Vec::new();
    for r in renderers {
        match r.group.grouping_key() {
            Some(k) => by_key.entry(k.to_string()).or_default().push(r),
            None => plain.push(r),
        }
    }

    let mut out: Vec<AirPlayEntry> = plain.into_iter().map(AirPlayEntry::plain).collect();
    for (_, mut members) in by_key {
        if members.len() < 2 {
            out.extend(members.into_iter().map(AirPlayEntry::plain));
            continue;
        }
        members.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        match pick_group_leader(&members) {
            Some(idx) => {
                let leader = members.remove(idx);
                let leader_id = leader.stable_id();
                for m in &members {
                    out.push(AirPlayEntry {
                        renderer: m.clone(),
                        led_members: Vec::new(),
                        folded_under: Some(leader_id.clone()),
                        ungrouped_peers: Vec::new(),
                    });
                }
                out.push(AirPlayEntry {
                    renderer: leader,
                    led_members: members,
                    folded_under: None,
                    ungrouped_peers: Vec::new(),
                });
            }
            None => {
                let names: Vec<String> = members.iter().map(|m| m.friendly_name.clone()).collect();
                for (i, m) in members.into_iter().enumerate() {
                    let peers = names
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, n)| n.clone())
                        .collect();
                    out.push(AirPlayEntry {
                        renderer: m,
                        led_members: Vec::new(),
                        folded_under: None,
                        ungrouped_peers: peers,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| {
        a.display_name()
            .cmp(&b.display_name())
            .then_with(|| a.renderer.mac_id.cmp(&b.renderer.mac_id))
    });
    out
}

/// Index of the member to target for a group, or `None` when there is
/// no unambiguous leader:
///
/// 1. the one Apple TV in the group — the home-theater case (HomePods /
///    speakers set as the TV's default output). An iPhone streams to
///    the TV and the TV renders on its speakers, and the TV is the
///    leader whichever way its `igl` currently reads (it drops to
///    `igl=0` while receiving AirPlay from someone else);
/// 2. else the one member advertising `igl=1`;
/// 3. else none — several claimants (a stereo pair advertises `igl=1`
///    on BOTH halves and needs a sender that drives both, which we
///    don't) or no hint at all.
pub fn pick_group_leader(members: &[AirPlayRenderer]) -> Option<usize> {
    let unique = |pred: &dyn Fn(&AirPlayRenderer) -> bool| -> Option<usize> {
        let mut it = members.iter().enumerate().filter(|(_, m)| pred(m));
        let first = it.next()?;
        it.next().is_none().then_some(first.0)
    };
    unique(&|m| m.is_apple_tv())
        .or_else(|| unique(&|m| m.group.is_group_leader == Some(true)))
}

/// Spawn the long-running mDNS browser. Browses **both** `_raop._tcp` and
/// `_airplay._tcp` on a shared daemon; a consumer thread per service keeps
/// the corresponding map fresh.
///
/// `iface_hint` is informational only — `mdns-sd` listens on all
/// interfaces and picks the right one per outgoing query.
pub fn spawn_airplay_discovery(
    state: Arc<AirPlayDiscoveryState>,
    iface_hint: Option<Ipv4Addr>,
) -> Result<()> {
    let daemon = ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon init: {}", e))?;

    info!(
        "AirPlay discovery: browsing {} + {} (iface hint {:?})",
        RAOP_SERVICE, AIRPLAY_SERVICE, iface_hint,
    );

    for service in [RAOP_SERVICE, AIRPLAY_SERVICE] {
        let receiver = daemon
            .browse(service)
            .map_err(|e| anyhow::anyhow!("mdns browse {}: {}", service, e))?;
        let state = state.clone();
        // Each consumer holds its own clone of the daemon handle so the
        // background daemon thread stays alive as long as either consumer
        // is running.
        let daemon = daemon.clone();
        thread::Builder::new()
            .name(format!("stream-to-speaker-airplay-mdns:{}", service))
            .spawn(move || {
                let _daemon_keepalive = daemon;
                loop {
                    match receiver.recv_timeout(Duration::from_secs(60)) {
                        Ok(ServiceEvent::ServiceResolved(info)) => {
                            ingest_resolution(&state, service, &info);
                        }
                        Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                            if let Some(mac) = mac_from_event(service, &fullname) {
                                debug!("AirPlay removed ({}): {}", service, fullname);
                                state.remove(&mac, service);
                            }
                        }
                        Ok(_) => { /* SearchStarted / ServiceFound / etc. */ }
                        Err(flume::RecvTimeoutError::Timeout) => {}
                        Err(flume::RecvTimeoutError::Disconnected) => {
                            warn!("AirPlay mDNS daemon disconnected ({}); discovery stops", service);
                            return;
                        }
                    }
                }
            })?;
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Resolution ingestion
// -----------------------------------------------------------------------------

/// TXT record as plain key → value pairs (keys as advertised). Lookups
/// are case-insensitive, as mdns-sd's own accessors are: receivers spell
/// keys inconsistently (`deviceid`/`deviceID`, `features`/`Features`).
type TxtMap = HashMap<String, String>;

fn txt_map(txt: &TxtProperties) -> TxtMap {
    txt.iter()
        .map(|p| (p.key().to_string(), p.val_str().to_string()))
        .collect()
}

fn ingest_resolution(state: &AirPlayDiscoveryState, service: &str, info: &ServiceInfo) {
    let ip = first_v4(info);
    let txt = txt_map(info.get_properties());

    if service == RAOP_SERVICE {
        // Service name: "<MAC>@<FriendlyName>._raop._tcp.local."
        let Some((mac, friendly)) = split_raop_name(info.get_fullname()) else {
            debug!("AirPlay RAOP: can't parse name {}", info.get_fullname());
            return;
        };
        let r = parse_raop_txt(friendly, ip, info.get_port(), &txt);
        debug!(
            "AirPlay RAOP resolved: {} @ {:?}:{} et={:?} cn={:?}",
            r.friendly_name, r.ip, r.port, r.encryption_types, r.codecs
        );
        state.upsert_raop(mac, r);
    } else {
        // _airplay._tcp: MAC is in the `deviceid` TXT key; the service
        // instance name is the friendly name.
        let Some(mac) = read_txt_string(&txt, "deviceid").map(|s| normalise_mac(&s)) else {
            debug!("AirPlay v2: no deviceid in {}", info.get_fullname());
            return;
        };
        let a = parse_airplay_txt(airplay_instance_name(info.get_fullname()), ip, info.get_port(), &txt);
        debug!(
            "AirPlay v2 resolved: {} @ {:?}:{} ft={:?} model={:?} gid={:?} pgid={:?} igl={:?} gcgl={:?} gpn={:?}",
            a.friendly_name,
            a.ip,
            a.port,
            a.features,
            a.model,
            a.group.group_id,
            a.group.parent_group_id,
            a.group.is_group_leader,
            a.group.group_contains_leader,
            a.group.group_name,
        );
        state.upsert_airplay(mac, a);
    }
}

fn parse_raop_txt(friendly_name: String, ip: Option<IpAddr>, port: u16, txt: &TxtMap) -> RaopInfo {
    RaopInfo {
        friendly_name,
        ip,
        port,
        encryption_types: read_txt_int_list(txt, "et"),
        codecs: read_txt_int_list(txt, "cn"),
        password_protected: read_txt_bool(txt, "pw").unwrap_or(false),
        encryption_key_required: read_txt_bool(txt, "ek").unwrap_or(false),
        model: read_txt_string(txt, "am"),
    }
}

fn parse_airplay_txt(friendly_name: String, ip: Option<IpAddr>, port: u16, txt: &TxtMap) -> AirPlayInfo {
    AirPlayInfo {
        friendly_name,
        ip,
        port,
        features: read_features(txt),
        pk: read_txt_string(txt, "pk"),
        model: read_txt_string(txt, "model"),
        group: parse_group_hints(txt),
    }
}

/// Read the group keys (see [`GroupHints`]).
fn parse_group_hints(txt: &TxtMap) -> GroupHints {
    GroupHints {
        group_id: read_txt_string(txt, "gid").and_then(|s| normalise_group_id(&s)),
        parent_group_id: read_txt_string(txt, "pgid").and_then(|s| normalise_group_id(&s)),
        // Apple spells it `igl`; Sonos' AP2 stack spells it `isGroupLeader`.
        is_group_leader: read_txt_bool(txt, "igl").or_else(|| read_txt_bool(txt, "isGroupLeader")),
        group_contains_leader: read_txt_bool(txt, "gcgl"),
        parent_group_contains_leader: read_txt_bool(txt, "pgcgl"),
        group_name: read_txt_string(txt, "gpn")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        tight_sync_id: read_txt_string(txt, "tsid")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty()),
    }
}

/// Normalise a `gid`/`pgid` so records from different devices compare:
/// lower-case, trimmed, and cut at the first `+` (a HomePod stereo pair
/// advertises `<tsid>+0+<uuid>` on one half and `<tsid>+1+<uuid>` on the
/// other). Empty and all-zero ids mean "no group".
fn normalise_group_id(raw: &str) -> Option<String> {
    let s = raw.trim().split('+').next().unwrap_or("").trim().to_ascii_lowercase();
    if s.is_empty() || s.chars().all(|c| c == '0' || c == '-') {
        return None;
    }
    Some(s)
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn first_v4(info: &ServiceInfo) -> Option<IpAddr> {
    info.get_addresses().iter().find_map(|a| match a {
        IpAddr::V4(v4) => Some(IpAddr::V4(*v4)),
        _ => None,
    })
}

/// Normalise a MAC to upper-case hex with no separators so the `deviceid`
/// form (`B8:E9:37:35:AC:11`) and the RAOP-name form (`B8E93735AC11`)
/// correlate.
fn normalise_mac(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Split `<MAC>@<FriendlyName>._raop._tcp.local.` → (normalised MAC, name).
fn split_raop_name(fullname: &str) -> Option<(String, String)> {
    let trimmed = fullname
        .strip_suffix(&format!(".{}", RAOP_SERVICE))
        .or_else(|| fullname.strip_suffix(RAOP_SERVICE))
        .unwrap_or(fullname);
    let (mac, friendly) = trimmed.split_once('@')?;
    let mac = normalise_mac(mac);
    let friendly = friendly.trim().to_string();
    if mac.is_empty() || friendly.is_empty() {
        return None;
    }
    Some((mac, friendly))
}

/// Extract the instance (friendly) name from an `_airplay._tcp` fullname.
fn airplay_instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(&format!(".{}", AIRPLAY_SERVICE))
        .or_else(|| fullname.strip_suffix(AIRPLAY_SERVICE))
        .unwrap_or(fullname)
        .trim_end_matches('.')
        .to_string()
}

/// On removal we only get the fullname; recover the MAC so we can evict
/// the right map entry. For RAOP it's in the name; for `_airplay._tcp` the
/// name is just the friendly name (no MAC), so we can't reliably evict by
/// MAC — return None and let the stale entry age out / be overwritten.
fn mac_from_event(service: &str, fullname: &str) -> Option<String> {
    if service == RAOP_SERVICE {
        Some(split_raop_name(fullname)?.0)
    } else {
        None
    }
}

fn read_txt_string(txt: &TxtMap, key: &str) -> Option<String> {
    txt.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

/// `Some(true)` for `true`/`1`/`yes`, `Some(false)` for any other value,
/// `None` when the key is absent (absence is meaningful for the group
/// hints, which are not advertised continuously).
fn read_txt_bool(txt: &TxtMap, key: &str) -> Option<bool> {
    read_txt_string(txt, key).map(|s| {
        let s = s.trim().to_ascii_lowercase();
        s == "true" || s == "1" || s == "yes"
    })
}

/// Parse a TXT field whose value is a comma-separated integer list
/// (`cn=0,1,3` → `[0, 1, 3]`). Returns an empty vec on absence.
fn read_txt_int_list(txt: &TxtMap, key: &str) -> Vec<u8> {
    match read_txt_string(txt, key) {
        Some(s) => s
            .split(',')
            .filter_map(|p| p.trim().parse::<u8>().ok())
            .collect(),
        None => Vec::new(),
    }
}

/// Parse the 64-bit `features` / `ft` flag word. AirPlay advertises it
/// either as a single value (`0x445F8A00`) or — very commonly — as two
/// comma-separated 32-bit halves, low first: `0x445F8A00,0x1C340`
/// → `(high << 32) | low`. Decimal is also accepted.
fn read_features(txt: &TxtMap) -> Option<u64> {
    let raw = read_txt_string(txt, "features").or_else(|| read_txt_string(txt, "ft"))?;
    let parts: Vec<&str> = raw.split(',').map(|p| p.trim()).collect();
    let parse = |s: &str| -> Option<u64> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).ok()
        } else {
            s.parse::<u64>().ok().or_else(|| u64::from_str_radix(s, 16).ok())
        }
    };
    match parts.as_slice() {
        [single] => parse(single),
        [low, high, ..] => {
            let low = parse(low)?;
            let high = parse(high)?;
            Some((high << 32) | (low & 0xFFFF_FFFF))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer(model: Option<&str>, et: Vec<u8>, ft: Option<u64>, raop_port: u16, ap_port: Option<u16>) -> AirPlayRenderer {
        AirPlayRenderer {
            friendly_name: "Test".into(),
            mac_id: "AABBCCDDEEFF".into(),
            ip: "192.168.1.5".parse().unwrap(),
            port: raop_port,
            airplay_port: ap_port,
            encryption_types: et,
            codecs: vec![1],
            password_protected: false,
            encryption_key_required: false,
            features: ft,
            pk: None,
            model: model.map(|s| s.to_string()),
            group: GroupHints::default(),
        }
    }

    #[test]
    fn normalise_mac_strips_separators_and_uppercases() {
        assert_eq!(normalise_mac("b8:e9:37:35:ac:11"), "B8E93735AC11");
        assert_eq!(normalise_mac("B8E93735AC11"), "B8E93735AC11");
    }

    #[test]
    fn features_single_and_split_hex() {
        // single
        let f = parse_features_str("0x40000200");
        assert_eq!(f, Some(0x40000200));
        // split low,high
        let f = parse_features_str("0x445F8A00,0x1C340");
        assert_eq!(f, Some((0x1C340u64 << 32) | 0x445F8A00));
    }

    // Tiny shim so the test can exercise the same parse logic without a
    // TxtProperties (which we can't easily synthesise here).
    fn parse_features_str(raw: &str) -> Option<u64> {
        let parts: Vec<&str> = raw.split(',').map(|p| p.trim()).collect();
        let parse = |s: &str| -> Option<u64> {
            if let Some(hex) = s.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).ok()
            } else {
                s.parse().ok()
            }
        };
        match parts.as_slice() {
            [single] => parse(single),
            [low, high, ..] => Some((parse(high)? << 32) | (parse(low)? & 0xFFFF_FFFF)),
            _ => None,
        }
    }


    // ------------------------------------------------------------------
    // Group folding — realistic TXT sets. Sources: openairplay
    // service_discovery.md (Apple TV state table + _raop example), the
    // HomePod stereo-pair / multi-room captures on owntone#1413, the
    // Apple-TV-with-HomePod-default-output fixture in spoticonn#1, and
    // OwnTone airplay.c's TXT examples (Sonos, Marantz, Apple TV 4).
    // ------------------------------------------------------------------

    fn txt(pairs: &[(&str, &str)]) -> TxtMap {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn txt_lookups_ignore_key_case() {
        // mdns-sd matched keys case-insensitively; a receiver that
        // advertises mixed-case keys must still be resolved and grouped.
        let t = txt(&[("deviceID", "AA:BB:CC:DD:EE:FF"), ("Features", "0x445F8A00,0x1C340"),
                      ("gid", "ABCDEF01-2222-3333-4444-555555555555"), ("isGroupLeader", "1"),
                      ("GPN", "Kitchen")]);
        assert_eq!(read_txt_string(&t, "deviceid").as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert!(read_features(&t).is_some());
        let g = parse_group_hints(&t);
        assert_eq!(g.is_group_leader, Some(true));
        assert_eq!(g.group_name.as_deref(), Some("Kitchen"));
    }

    /// Build a merged renderer from an `_airplay._tcp` TXT set (plus an
    /// optional `_raop._tcp` one), the way discovery does.
    fn from_txt(
        name: &str,
        mac: &str,
        ip: &str,
        airplay: Option<&[(&str, &str)]>,
        raop: Option<&[(&str, &str)]>,
    ) -> AirPlayRenderer {
        let ip: IpAddr = ip.parse().unwrap();
        let a = airplay.map(|t| parse_airplay_txt(name.to_string(), Some(ip), 7000, &txt(t)));
        let r = raop.map(|t| parse_raop_txt(name.to_string(), Some(ip), 7000, &txt(t)));
        merge(&normalise_mac(mac), r.as_ref(), a.as_ref()).expect("has ip")
    }

    const TV_GID: &str = "FA6CE6B6-16B6-557C-B2D2-451F3C68D932";
    const TV_FEATURES: &str = "0x4A7FDFD5,0x3C155FDE"; // AppleTV6,2, pyatv capture
    const HOMEPOD_FEATURES: &str = "0x4A7FCA00,0xBC354BD0"; // owntone#1413 capture

    fn apple_tv_raop() -> Vec<(&'static str, &'static str)> {
        vec![
            ("txtvers", "1"), ("ch", "2"), ("cn", "0,1,2,3"), ("da", "true"), ("et", "0,3,5"),
            ("md", "0,1,2"), ("pw", "false"), ("sv", "false"), ("sr", "44100"), ("ss", "16"),
            ("tp", "UDP"), ("vn", "65537"), ("vs", "550.10"), ("am", "AppleTV6,2"), ("sf", "0x4"),
        ]
    }

    /// Apple TV, idle, leading its default-output group (spec: `igl=1
    /// gcgl=1`; spoticonn: `gpn` shared with the HomePods).
    fn apple_tv_leader() -> AirPlayRenderer {
        from_txt(
            "Living Room",
            "06:03:16:5E:17:B1",
            "192.0.2.10",
            Some(&[
                ("acl", "0"), ("deviceid", "06:03:16:5E:17:B1"), ("features", TV_FEATURES),
                ("flags", "0x244"), ("gid", TV_GID), ("igl", "1"), ("gcgl", "1"),
                ("gpn", "Living Room"), ("model", "AppleTV6,2"), ("protovers", "1.1"),
                ("pi", "de7562c4-7bd2-4005-a8e4-d584bf63161a"), ("pk", "aa"), ("srcvers", "550.10"),
                ("osvers", "14.7"), ("vv", "2"),
            ]),
            Some(&apple_tv_raop()),
        )
    }

    /// A HomePod set as that Apple TV's default audio output: shares the
    /// TV's `gid` + `gpn`, `igl=0 gcgl=1`.
    fn homepod_member(name: &str, mac: &str, ip: &str) -> AirPlayRenderer {
        from_txt(
            name,
            mac,
            ip,
            Some(&[
                ("acl", "0"), ("deviceid", mac), ("features", HOMEPOD_FEATURES), ("flags", "0xb8c04"),
                ("gid", TV_GID), ("igl", "0"), ("gcgl", "1"), ("gpn", "Living Room"),
                ("model", "AudioAccessory5,1"), ("protovers", "1.1"), ("pk", "bb"),
                ("srcvers", "710.68.3"), ("osvers", "17.4"), ("vv", "2"),
            ]),
            Some(&[
                ("cn", "0,1,2,3"), ("et", "0,3,5"), ("am", "AudioAccessory5,1"), ("tp", "UDP"),
                ("pw", "false"), ("sf", "0x8c04"), ("vs", "710.68.3"),
            ]),
        )
    }

    fn find<'a>(entries: &'a [AirPlayEntry], name: &str) -> &'a AirPlayEntry {
        entries
            .iter()
            .find(|e| e.renderer.friendly_name == name)
            .unwrap_or_else(|| panic!("no entry named {name}"))
    }

    #[test]
    fn group_hints_parse_and_normalise() {
        let g = parse_group_hints(&txt(&[
            ("gid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+0+6276FFFA-04E1-439E-8139-2C906B34E587"),
            ("igl", "1"), ("gcgl", "1"), ("gpn", "Büro 2"),
            ("tsid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C"),
        ]));
        assert_eq!(g.group_id.as_deref(), Some("eafa36aa-9785-54b2-a537-d9ee2a55cf1c"));
        assert_eq!(g.is_group_leader, Some(true));
        assert_eq!(g.group_contains_leader, Some(true));
        assert_eq!(g.group_name.as_deref(), Some("Büro 2"));
        assert_eq!(g.tight_sync_id.as_deref(), Some("eafa36aa-9785-54b2-a537-d9ee2a55cf1c"));
        assert_eq!(g.parent_group_id, None);
        assert_eq!(g.grouping_key(), Some("eafa36aa-9785-54b2-a537-d9ee2a55cf1c"));

        // Sonos spells the leader key `isGroupLeader`; pgid wins as key.
        let g = parse_group_hints(&txt(&[
            ("isGroupLeader", "0"), ("gcgl", "0"), ("gid", "A918B6A2-BB3F-4A50-A422-0BB043C9F3BF"),
            ("pgid", "0BB043C9-F3BF-4A50-A422-A918B6A2BB3F"), ("pgcgl", "0"),
        ]));
        assert_eq!(g.is_group_leader, Some(false));
        assert_eq!(g.grouping_key(), Some("0bb043c9-f3bf-4a50-a422-a918b6a2bb3f"));
        assert_eq!(g.parent_group_contains_leader, Some(false));

        // Absent keys stay None (absence ≠ false); zero/empty gid = no group.
        let g = parse_group_hints(&txt(&[("gid", "00000000-0000-0000-0000-000000000000")]));
        assert_eq!(g.group_id, None);
        assert_eq!(g.is_group_leader, None);
        assert!(!g.any());
        let g = parse_group_hints(&txt(&[("gid", "  ")]));
        assert_eq!(g.group_id, None);
    }

    #[test]
    fn apple_tv_leader_with_two_homepods_folds_into_one_row() {
        let tv = apple_tv_leader();
        let left = homepod_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        let right = homepod_member("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12");
        // Today's per-device routing is unchanged.
        assert_eq!(tv.transport(), Some(Transport::RaopLegacy));
        assert_eq!(left.transport(), Some(Transport::AirPlay2));

        let entries = group_renderers(vec![left.clone(), tv.clone(), right.clone()]);
        assert_eq!(entries.len(), 3);
        let group = find(&entries, "Living Room");
        assert!(group.is_group());
        assert_eq!(group.display_name(), "Living Room");
        assert_eq!(
            group.member_names(),
            vec!["Living Room", "Living Room L", "Living Room R"]
        );
        assert_eq!(group.renderer.stable_id(), tv.stable_id());
        // The leader is driven over AirPlay 2, not its (silent) legacy RAOP.
        assert_eq!(group.transport(), Some(Transport::AirPlay2));
        assert_eq!(group.renderer.transport_as_group_leader(), Some(Transport::AirPlay2));
        for name in ["Living Room L", "Living Room R"] {
            let m = find(&entries, name);
            assert_eq!(m.folded_under.as_deref(), Some(tv.stable_id().as_str()));
            assert!(!m.is_group());
            assert!(m.ungrouped_peers.is_empty());
            assert_eq!(m.transport(), Some(Transport::AirPlay2));
        }
        // Visible rows (what the list shows by default) = the group only.
        let visible: Vec<_> = entries.iter().filter(|e| e.folded_under.is_none()).collect();
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn apple_tv_receiving_from_another_sender_still_leads_its_homepods() {
        // Spec "Apple TV receiving AirPlay audio": igl=0 gcgl=0, gid and
        // pgid = the sender's session group. The HomePods follow suit.
        const SESSION: &str = "19F5D4B2-8A06-4792-923E-8AFA83913238";
        let tv = from_txt(
            "Living Room",
            "06:03:16:5E:17:B1",
            "192.0.2.10",
            Some(&[
                ("deviceid", "06:03:16:5E:17:B1"), ("features", TV_FEATURES), ("flags", "0x30e44"),
                ("gid", SESSION), ("igl", "0"), ("gcgl", "0"), ("pgid", SESSION), ("pgcgl", "0"),
                ("model", "AppleTV6,2"), ("pk", "aa"), ("srcvers", "550.10"),
            ]),
            Some(&apple_tv_raop()),
        );
        let pod = from_txt(
            "Living Room L",
            "6A:9C:DD:77:21:C7",
            "192.0.2.11",
            Some(&[
                ("deviceid", "6A:9C:DD:77:21:C7"), ("features", HOMEPOD_FEATURES),
                ("gid", SESSION), ("igl", "0"), ("gcgl", "0"), ("pgid", SESSION), ("pgcgl", "0"),
                ("model", "AudioAccessory5,1"), ("pk", "bb"),
            ]),
            None,
        );
        let entries = group_renderers(vec![pod, tv.clone()]);
        let group = find(&entries, "Living Room");
        assert!(group.is_group(), "the Apple TV leads regardless of its momentary igl");
        assert_eq!(group.renderer.mac_id, tv.mac_id);
        // No gpn → falls back to the leader's name.
        assert_eq!(group.display_name(), "Living Room");
    }

    #[test]
    fn lone_homepod_stereo_pair_stays_two_rows_with_peer_notes() {
        // Verbatim shape from owntone#1413: both halves advertise igl=1
        // gcgl=1, gid = "<tsid>+<0|1>+<uuid>", the same gpn and tsid.
        let half = |name: &str, mac: &str, ip: &str, gid: &str| {
            from_txt(
                name,
                mac,
                ip,
                Some(&[
                    ("deviceid", mac), ("features", HOMEPOD_FEATURES), ("flags", "0x9a404"),
                    ("gid", gid), ("igl", "1"), ("gcgl", "1"), ("gpn", "Büro 2"), ("tsm", "0"),
                    ("tsid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C"), ("model", "AudioAccessory1,1"),
                    ("pk", "cc"), ("srcvers", "530.6"), ("acl", "0"),
                ]),
                Some(&[("cn", "0,1,2,3"), ("et", "0,3,5"), ("am", "AudioAccessory1,1"), ("tp", "UDP")]),
            )
        };
        let links = half(
            "Links", "D4:A3:3D:7A:28:D8", "192.168.178.46",
            "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+0+6276FFFA-04E1-439E-8139-2C906B34E587",
        );
        let rechts = half(
            "Rechts", "50:BC:96:07:E8:6D", "192.168.178.47",
            "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+1+4671EEC3-7E13-4DC7-BEC3-C9805D3AB964",
        );
        assert_eq!(links.group.grouping_key(), rechts.group.grouping_key());
        let entries = group_renderers(vec![links, rechts]);
        assert_eq!(entries.len(), 2);
        for (me, other) in [("Links", "Rechts"), ("Rechts", "Links")] {
            let e = find(&entries, me);
            assert!(!e.is_group());
            assert!(e.folded_under.is_none(), "no unambiguous leader → not folded");
            assert_eq!(e.ungrouped_peers, vec![other.to_string()]);
            assert_eq!(e.display_name(), me);
            assert_eq!(e.transport(), Some(Transport::AirPlay2));
        }
    }

    #[test]
    fn member_with_gid_but_no_leader_in_range_is_a_plain_row() {
        let pod = homepod_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        assert_eq!(pod.group.group_contains_leader, Some(true));
        let entries = group_renderers(vec![pod.clone()]);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(!e.is_group());
        assert!(e.folded_under.is_none());
        assert!(e.ungrouped_peers.is_empty());
        assert_eq!(e.display_name(), "Living Room L");
        assert_eq!(e.transport(), pod.transport());
    }

    #[test]
    fn standalone_receivers_with_private_gids_are_not_grouped() {
        // Every standalone AP2 receiver advertises its own gid (Sonos:
        // gid = pi, gcgl=0). Distinct gids → distinct plain rows; the
        // transport choice is byte-for-byte today's.
        let sonos = from_txt(
            "Kitchen",
            "11:22:33:44:55:66",
            "192.0.2.20",
            Some(&[
                ("pk", "e5"), ("gcgl", "0"), ("gid", "0fd9c3b0-1111-2222-3333-444455556666"),
                ("pi", "0fd9c3b0-1111-2222-3333-444455556666"), ("srcvers", "366.0"),
                ("protovers", "1.1"), ("manufacturer", "Sonos"), ("model", "Bookshelf"),
                ("flags", "0x4"), ("features", "0x445F8A00,0x1C340"),
                ("deviceid", "11:22:33:44:55:66"), ("acl", "0"),
            ]),
            Some(&[("et", "0,4"), ("cn", "0,1"), ("sf", "0x4"), ("tp", "UDP"), ("am", "Bookshelf")]),
        );
        let tv = apple_tv_leader(); // idle TV, gid nobody shares
        let marantz = from_txt(
            "Marantz",
            "00:06:12:12:12:12",
            "192.0.2.30",
            Some(&[
                ("srcvers", "190.9.p6"), ("model", "NR1607"), ("flags", "0x4"),
                ("features", "0x444F8A00"), ("deviceid", "00:06:12:12:12:12"),
            ]),
            Some(&[("et", "0,1"), ("cn", "0,1"), ("tp", "UDP")]),
        );
        assert!(!marantz.group.any());
        let entries = group_renderers(vec![sonos.clone(), tv.clone(), marantz.clone()]);
        assert_eq!(entries.len(), 3);
        for (r, name) in [(&sonos, "Kitchen"), (&tv, "Living Room"), (&marantz, "Marantz")] {
            let e = find(&entries, name);
            assert!(!e.is_group());
            assert!(e.folded_under.is_none());
            assert!(e.ungrouped_peers.is_empty());
            assert_eq!(e.transport(), r.transport(), "{name} keeps today's transport");
        }
        // A lone Apple TV still takes the proven legacy path.
        assert_eq!(find(&entries, "Living Room").transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn sonos_joined_to_a_homepod_group_folds_under_the_igl_leader() {
        // owntone#1413 multi-room capture shape: a user-built group with
        // one igl=1 HomePod and a Sonos carrying `isGroupLeader=0`. No
        // Apple TV → the unique igl=1 member leads.
        const G: &str = "A918B6A2-BB3F-4A50-A422-0BB043C9F3BF";
        let leader = from_txt(
            "Büro",
            "E0:2B:96:96:FB:77",
            "192.0.2.40",
            Some(&[
                ("deviceid", "E0:2B:96:96:FB:77"), ("features", HOMEPOD_FEATURES), ("gid", G),
                ("igl", "1"), ("gcgl", "1"), ("gpn", "Büro"), ("model", "AudioAccessory5,1"), ("pk", "dd"),
            ]),
            None,
        );
        let sonos = from_txt(
            "Basement",
            "34:7E:5C:31:D9:96",
            "192.0.2.41",
            Some(&[
                ("deviceid", "34:7E:5C:31:D9:96"), ("features", "0x445F8A00,0x1C340"), ("gid", G),
                ("isGroupLeader", "0"), ("gcgl", "0"), ("model", "Bookshelf"), ("pk", "ee"),
            ]),
            Some(&[("et", "0,4"), ("cn", "0,1")]),
        );
        let entries = group_renderers(vec![sonos.clone(), leader.clone()]);
        let g = find(&entries, "Büro");
        assert!(g.is_group());
        assert_eq!(g.member_names(), vec!["Büro", "Basement"]);
        assert_eq!(find(&entries, "Basement").folded_under.as_deref(), Some(leader.stable_id().as_str()));
    }

    #[test]
    fn two_leader_claims_without_apple_tv_do_not_fold() {
        let a = renderer(Some("AudioAccessory5,1"), vec![0], Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING), 0, Some(7000));
        let mut b = a.clone();
        b.mac_id = "AABBCCDDEE00".into();
        b.friendly_name = "Other".into();
        let mut members = vec![a, b];
        for m in &mut members {
            m.group.group_id = Some("g".into());
            m.group.is_group_leader = Some(true);
        }
        assert_eq!(pick_group_leader(&members), None);
        members[1].group.is_group_leader = Some(false);
        assert_eq!(pick_group_leader(&members), Some(0));
        members[1].model = Some("AppleTV14,1".into());
        assert_eq!(pick_group_leader(&members), Some(1), "an Apple TV outranks igl");
    }

    #[test]
    fn airport_express_is_legacy() {
        let r = renderer(Some("AirPort10,115"), vec![0, 1], None, 5000, None);
        assert!(r.supports_legacy_raop());
        assert!(!r.requires_airplay2());
        assert_eq!(r.transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn homepod_routes_to_airplay2_even_with_legacy_et() {
        // HomePod advertises RAOP with et that includes 0/1, but gates on
        // pairing — must route to AP2.
        let r = renderer(
            Some("AudioAccessory5,1"),
            vec![0, 3, 5],
            Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING | FEAT_PTP),
            7000,
            Some(7000),
        );
        assert!(r.is_homepod());
        assert!(r.requires_airplay2());
        assert!(r.supports_airplay2());
        assert_eq!(r.transport(), Some(Transport::AirPlay2));
        assert!(r.expects_ptp());
    }

    #[test]
    fn real_sonos_features_classify_as_ap2_with_ptp() {
        // The exact `features` word a Sonos advertises on _airplay._tcp.
        // It must decode to AirPlay 2 + PTP so routing prefers AP2 — its
        // _raop._tcp service is vestigial and times out at OPTIONS.
        let ft = parse_features_str("0x445F8A00,0x1C340");
        assert!(ft.is_some(), "split-hex features should parse");
        let r = renderer(Some("One"), vec![0, 1], ft, 5000, Some(7000));
        assert!(r.supports_airplay2(), "Sonos must support AirPlay 2");
        assert!(r.expects_ptp(), "Sonos advertises PTP (feature bit 41)");
        assert!(r.supports_buffered_audio(), "Sonos advertises buffered audio (bit 40)");
        // It also advertises a legacy RAOP service we'd otherwise prefer,
        // which is why blind RAOP routing stalled at OPTIONS.
        assert!(r.supports_legacy_raop());
    }

    #[test]
    fn appletv_with_legacy_et_prefers_legacy() {
        // Apple TV supports HK pairing but also legacy RAOP — keep it on
        // the proven path.
        let r = renderer(
            Some("AppleTV6,2"),
            vec![0, 1],
            Some(FEAT_AUDIO | FEAT_HK_PAIRING | FEAT_TRANSIENT_PAIRING),
            7000,
            Some(7000),
        );
        assert!(!r.requires_airplay2());
        assert_eq!(r.transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn fairplay_only_without_ap2_is_unsupported() {
        let r = renderer(Some("Speaker1,1"), vec![3, 5], None, 5000, None);
        assert!(!r.supports_legacy_raop());
        assert!(!r.supports_airplay2());
        assert_eq!(r.transport(), None);
        assert!(!r.is_supported());
    }

    #[test]
    fn rsa_preference_targets_airport_express_not_sonos() {
        // Sonos-class (et=0,4, no ek, model is a friendly name) → plaintext.
        let sonos = renderer(Some("Picture frame"), vec![0, 4], None, 5000, None);
        assert!(!sonos.prefers_rsa_encryption(), "Sonos must stay on plaintext");

        // Classic AirPort Express gen2 (am=AirPort4,x, et=0,1) → RSA.
        let apex2 = renderer(Some("AirPort4,107"), vec![0, 1], None, 5000, None);
        assert!(apex2.prefers_rsa_encryption());

        // AP2-era AirPort Express (am=AirPort10,x, et=0,4) → plaintext.
        let apex_ap2 = renderer(Some("AirPort10,115"), vec![0, 4], None, 5000, None);
        assert!(!apex_ap2.prefers_rsa_encryption());

        // ek=1 with et=1 available → RSA (classic AirPort Express gen1).
        let mut ek = renderer(None, vec![0, 1], None, 5000, None);
        ek.encryption_key_required = true;
        assert!(ek.prefers_rsa_encryption());

        // ek=1 but no et=1 on offer → we can't RSA, so no.
        let mut ek_no_rsa = renderer(Some("Weird"), vec![0], None, 5000, None);
        ek_no_rsa.encryption_key_required = true;
        assert!(!ek_no_rsa.prefers_rsa_encryption());

        // Third-party et=0,1 plaintext speaker (no ek, non-AirPort model)
        // → plaintext (must NOT force RSA, which would break it).
        let thirdparty = renderer(Some("Denon"), vec![0, 1], None, 5000, None);
        assert!(!thirdparty.prefers_rsa_encryption());

        // et=0,1,4 with ek=1 → still plaintext (et=4 present ⇒ AP2-era,
        // takes plaintext regardless of ek — matches OwnTone APEX3).
        let mut et4_ek = renderer(Some("AirPort4,999"), vec![0, 1, 4], None, 5000, None);
        et4_ek.encryption_key_required = true;
        assert!(!et4_ek.prefers_rsa_encryption());
    }
}
