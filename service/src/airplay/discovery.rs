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
//! | `tsid`      | group id shared by the members of a stereo pair      |
//! | `gpn`       | group (pair) name                                    |
//! | `igl`       | `1` on the group leader                              |
//!
//! ## Stereo pairs
//!
//! Two HomePods configured as a stereo pair advertise separately but share
//! a `tsid` (only HomePods are grouped this way). Once two records with the same `tsid` have been seen they are
//! listed as **one** entry (`airplay:pair:<tsid>`, see [`StereoPair`]) and
//! the members are hidden; selecting a member selects the pair. The entry
//! stays listed while a member is offline, but can only be played with
//! every member present.

use anyhow::Result;
use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperties};
use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// Group TXT keys from `_airplay._tcp`, when the device advertises a
    /// group id.
    pub group: Option<GroupTxt>,
    /// Set on the synthetic entry that stands for a whole stereo pair.
    pub pair: Option<StereoPair>,
}

/// Group-related TXT keys of one receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTxt {
    /// `tsid`, normalised (trimmed, lower-case).
    pub tsid: String,
    /// `gpn` — the group's display name.
    pub name: Option<String>,
    /// `igl=1` — this member leads the group.
    pub leader: bool,
}

/// A stereo pair: members seen sharing one `tsid`.
#[derive(Debug, Clone)]
pub struct StereoPair {
    pub tsid: String,
    /// Members currently discovered, leader first.
    pub members: Vec<AirPlayRenderer>,
    /// Members known to belong to the pair (seen this run).
    pub expected: usize,
}

impl StereoPair {
    /// Every known member is present.
    pub fn is_complete(&self) -> bool {
        self.members.len() >= self.expected && self.expected >= 2
    }
}

impl AirPlayRenderer {
    /// Stable identifier used by the UI / persistence layer. Prefixed so
    /// it can't collide with the UPnP `Renderer::stable_id()`.
    pub fn stable_id(&self) -> String {
        match &self.pair {
            Some(p) => format!("{}{}", PAIR_ID_PREFIX, p.tsid),
            None => format!("airplay:{}", self.mac_id),
        }
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
        if self.is_homepod() || self.pair.is_some() {
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
        // A pair is played over AirPlay 2 by every member, so all of them
        // must be present and reachable that way.
        if let Some(p) = &self.pair {
            let ok = p.is_complete() && p.members.iter().all(|m| m.supports_airplay2());
            return ok.then_some(Transport::AirPlay2);
        }
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
    group: Option<GroupTxt>,
}

/// Stable-id prefix of a stereo-pair entry.
pub const PAIR_ID_PREFIX: &str = "airplay:pair:";

/// Shared discovery state. Mirrors `ssdp::DiscoveryState` semantics.
#[derive(Default)]
pub struct AirPlayDiscoveryState {
    raop: Mutex<HashMap<String, RaopInfo>>,
    airplay: Mutex<HashMap<String, AirPlayInfo>>,
    /// `_airplay._tcp` instance fullname → MAC, so a removal (which only
    /// names the instance) can evict the right record.
    airplay_names: Mutex<HashMap<String, String>>,
    /// tsid → MACs seen advertising it this run. Two or more make a pair;
    /// remembering them keeps the pair listed while a member is offline.
    pair_members: Mutex<HashMap<String, BTreeSet<String>>>,
}

impl AirPlayDiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot of currently-known receivers, sorted by friendly name.
    /// Stereo-pair members are folded into one entry per pair.
    pub fn renderers(&self) -> Vec<AirPlayRenderer> {
        let singles = {
            let raop = self.raop.lock().unwrap();
            let airplay = self.airplay.lock().unwrap();
            let mut macs: HashSet<String> = HashSet::new();
            macs.extend(raop.keys().cloned());
            macs.extend(airplay.keys().cloned());
            macs.into_iter()
                .filter_map(|mac| merge(&mac, raop.get(&mac), airplay.get(&mac)))
                .collect::<Vec<_>>()
        };
        let memory = self.pair_members.lock().unwrap().clone();
        let mut v = fold_pairs(singles, &memory);
        v.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        v
    }

    /// Find by our public `stable_id()` (i.e. with the `airplay:` prefix).
    /// A stereo-pair member's id resolves to its pair: playing one member
    /// alone would break the pair.
    pub fn find_by_id(&self, id: &str) -> Option<AirPlayRenderer> {
        if id.starts_with(PAIR_ID_PREFIX) {
            return self.renderers().into_iter().find(|r| r.stable_id() == id);
        }
        let mac = id.strip_prefix("airplay:")?.to_string();
        let tsid = self
            .pair_members
            .lock()
            .unwrap()
            .iter()
            .find(|(_, macs)| macs.len() >= 2 && macs.contains(&mac))
            .map(|(tsid, _)| tsid.clone());
        if let Some(tsid) = tsid {
            let pair_id = format!("{}{}", PAIR_ID_PREFIX, tsid);
            return self.renderers().into_iter().find(|r| r.stable_id() == pair_id);
        }
        let raop = self.raop.lock().unwrap();
        let airplay = self.airplay.lock().unwrap();
        merge(&mac, raop.get(&mac), airplay.get(&mac))
    }

    /// Launch reconnect for a saved stereo pair (or a saved id of one of
    /// its members): wait up to `max` for every member to resolve and
    /// return the pair's id. `None` when the id isn't a HomePod pair (or
    /// a possible member of one) or the pair didn't complete in time.
    pub fn wait_for_pair(&self, id: &str, max: Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + max;
        loop {
            match pair_launch_step(id, self.find_by_id(id).as_ref()) {
                PairLaunch::Ready(pair_id) => return Some(pair_id),
                PairLaunch::NotAPair => return None,
                PairLaunch::Wait if std::time::Instant::now() >= deadline => return None,
                PairLaunch::Wait => thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    fn upsert_raop(&self, mac: String, info: RaopInfo) {
        self.raop.lock().unwrap().insert(mac, info);
    }

    fn upsert_airplay(&self, mac: String, fullname: &str, info: AirPlayInfo) {
        {
            let mut memory = self.pair_members.lock().unwrap();
            // A device that left a pair (or changed groups) is forgotten
            // under its old tsid.
            for (tsid, macs) in memory.iter_mut() {
                if info.group.as_ref().map(|g| &g.tsid) != Some(tsid) {
                    macs.remove(&mac);
                }
            }
            memory.retain(|_, macs| !macs.is_empty());
            // Only HomePods are folded into pairs; other receivers keep
            // their own entries whatever group keys they advertise.
            let homepod = info.model.as_deref().is_some_and(|m| m.starts_with("AudioAccessory"));
            if let Some(g) = info.group.as_ref().filter(|_| homepod) {
                memory.entry(g.tsid.clone()).or_default().insert(mac.clone());
            }
        }
        {
            // One instance name per device: a device that re-advertises
            // under a new name forgets the old one, so a late removal of
            // the old name can't evict the current record.
            let mut names = self.airplay_names.lock().unwrap();
            names.retain(|n, m| *m != mac || n == fullname);
            names.insert(fullname.to_string(), mac.clone());
        }
        self.airplay.lock().unwrap().insert(mac, info);
    }

    fn remove(&self, mac: &str, service: &str) {
        if service == RAOP_SERVICE {
            self.raop.lock().unwrap().remove(mac);
        } else {
            self.airplay.lock().unwrap().remove(mac);
        }
    }

    /// Evict the `_airplay._tcp` record named `fullname` — HomePods only,
    /// where it tells which stereo-pair member is offline. Other receivers
    /// keep their record (and so their routing) until re-resolved.
    fn remove_airplay_instance(&self, fullname: &str) {
        let mut names = self.airplay_names.lock().unwrap();
        let Some(mac) = names.get(fullname).cloned() else { return };
        let mut airplay = self.airplay.lock().unwrap();
        let homepod = airplay
            .get(&mac)
            .and_then(|a| a.model.as_deref())
            .is_some_and(|m| m.starts_with("AudioAccessory"));
        if homepod {
            names.remove(fullname);
            airplay.remove(&mac);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PairLaunch {
    Ready(String),
    Wait,
    NotAPair,
}

/// One step of [`AirPlayDiscoveryState::wait_for_pair`]: a complete pair
/// is ready; an incomplete pair, a pair id not resolved yet, or a HomePod
/// advertising a group id (its partner may not have resolved) is worth
/// waiting for; anything else isn't a pair.
fn pair_launch_step(id: &str, found: Option<&AirPlayRenderer>) -> PairLaunch {
    match found {
        Some(r) => match &r.pair {
            Some(p) if p.is_complete() => PairLaunch::Ready(r.stable_id()),
            Some(_) => PairLaunch::Wait,
            None if r.is_homepod() && r.group.is_some() => PairLaunch::Wait,
            None => PairLaunch::NotAPair,
        },
        None if id.starts_with(PAIR_ID_PREFIX) => PairLaunch::Wait,
        None => PairLaunch::NotAPair,
    }
}

/// Replace the members of every known pair (`memory`: tsid → MACs seen)
/// with one pair entry. The entry copies its leader's record (`igl=1`,
/// else the lowest MAC present) and is named by `gpn`, else the leader's
/// name. Pairs with no member present are omitted.
fn fold_pairs(singles: Vec<AirPlayRenderer>, memory: &HashMap<String, BTreeSet<String>>) -> Vec<AirPlayRenderer> {
    let pairs: HashMap<&String, &BTreeSet<String>> =
        memory.iter().filter(|(_, macs)| macs.len() >= 2).collect();
    let pair_of = |r: &AirPlayRenderer| -> Option<String> {
        pairs
            .iter()
            .find(|(_, macs)| macs.contains(&r.mac_id))
            .map(|(tsid, _)| (*tsid).clone())
    };
    let mut out = Vec::new();
    let mut grouped: HashMap<String, Vec<AirPlayRenderer>> = HashMap::new();
    for r in singles {
        match pair_of(&r) {
            Some(tsid) => grouped.entry(tsid).or_default().push(r),
            None => out.push(r),
        }
    }
    for (tsid, mut members) in grouped {
        members.sort_by(|a, b| {
            let lead = |r: &AirPlayRenderer| !r.group.as_ref().map(|g| g.leader).unwrap_or(false);
            lead(a).cmp(&lead(b)).then_with(|| a.mac_id.cmp(&b.mac_id))
        });
        let leader = members[0].clone();
        let name = members
            .iter()
            .find_map(|m| m.group.as_ref().and_then(|g| g.name.clone()))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| leader.friendly_name.clone());
        let expected = pairs.get(&tsid).map(|m| m.len()).unwrap_or(2);
        let mut entry = leader;
        entry.friendly_name = name;
        entry.pair = Some(StereoPair { tsid, members, expected });
        out.push(entry);
    }
    out
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
        group: airplay.and_then(|a| a.group.clone()),
        pair: None,
    })
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
                            debug!("AirPlay removed ({}): {}", service, fullname);
                            if let Some(mac) = mac_from_event(service, &fullname) {
                                state.remove(&mac, service);
                            } else if service == AIRPLAY_SERVICE {
                                state.remove_airplay_instance(&fullname);
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

fn ingest_resolution(state: &AirPlayDiscoveryState, service: &str, info: &ServiceInfo) {
    let ip = first_v4(info);
    let txt = info.get_properties();

    if service == RAOP_SERVICE {
        // Service name: "<MAC>@<FriendlyName>._raop._tcp.local."
        let Some((mac, friendly)) = split_raop_name(info.get_fullname()) else {
            debug!("AirPlay RAOP: can't parse name {}", info.get_fullname());
            return;
        };
        let r = RaopInfo {
            friendly_name: friendly,
            ip,
            port: info.get_port(),
            encryption_types: read_txt_int_list(txt, "et"),
            codecs: read_txt_int_list(txt, "cn"),
            password_protected: read_txt_bool(txt, "pw"),
            encryption_key_required: read_txt_bool(txt, "ek"),
            model: read_txt_string(txt, "am"),
        };
        debug!(
            "AirPlay RAOP resolved: {} @ {:?}:{} et={:?} cn={:?}",
            r.friendly_name, r.ip, r.port, r.encryption_types, r.codecs
        );
        state.upsert_raop(mac, r);
    } else {
        // _airplay._tcp: MAC is in the `deviceid` TXT key; the service
        // instance name is the friendly name.
        let Some(mac) = read_txt_string(txt, "deviceid").map(|s| normalise_mac(&s)) else {
            debug!("AirPlay v2: no deviceid in {}", info.get_fullname());
            return;
        };
        let a = AirPlayInfo {
            friendly_name: airplay_instance_name(info.get_fullname()),
            ip,
            port: info.get_port(),
            features: read_features(txt),
            pk: read_txt_string(txt, "pk"),
            model: read_txt_string(txt, "model"),
            group: group_txt(
                read_txt_string(txt, "tsid").as_deref(),
                read_txt_string(txt, "gpn"),
                read_txt_bool(txt, "igl"),
            ),
        };
        debug!(
            "AirPlay v2 resolved: {} @ {:?}:{} ft={:?} model={:?} group={:?}",
            a.friendly_name, a.ip, a.port, a.features, a.model, a.group
        );
        state.upsert_airplay(mac, info.get_fullname(), a);
    }
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

/// Group keys from TXT. An empty or all-zero `tsid` means "no group".
fn group_txt(tsid: Option<&str>, name: Option<String>, leader: bool) -> Option<GroupTxt> {
    let tsid = tsid?.trim().to_ascii_lowercase();
    if tsid.is_empty() || tsid.chars().all(|c| c == '0' || c == '-') {
        return None;
    }
    Some(GroupTxt { tsid, name: name.map(|n| n.trim().to_string()), leader })
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

fn read_txt_string(txt: &TxtProperties, key: &str) -> Option<String> {
    txt.get_property_val_str(key).map(|s| s.to_string())
}

fn read_txt_bool(txt: &TxtProperties, key: &str) -> bool {
    match read_txt_string(txt, key) {
        Some(s) => {
            let s = s.to_ascii_lowercase();
            s == "true" || s == "1" || s == "yes"
        }
        None => false,
    }
}

/// Parse a TXT field whose value is a comma-separated integer list
/// (`cn=0,1,3` → `[0, 1, 3]`). Returns an empty vec on absence.
fn read_txt_int_list(txt: &TxtProperties, key: &str) -> Vec<u8> {
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
fn read_features(txt: &TxtProperties) -> Option<u64> {
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
            group: None,
            pair: None,
        }
    }

    const HOMEPOD_FT: u64 = FEAT_AUDIO | FEAT_PTP | FEAT_TRANSIENT_PAIRING | FEAT_HK_PAIRING;

    fn pod(mac: &str, tsid: Option<&str>, leader: bool, gpn: Option<&str>) -> AirPlayRenderer {
        let mut r = renderer(Some("AudioAccessory5,1"), vec![0], Some(HOMEPOD_FT), 7000, Some(7000));
        r.mac_id = mac.into();
        r.friendly_name = format!("Pod {mac}");
        r.group = tsid.and_then(|t| group_txt(Some(t), gpn.map(String::from), leader));
        r
    }

    fn memory(entries: &[(&str, &[&str])]) -> HashMap<String, BTreeSet<String>> {
        entries
            .iter()
            .map(|(t, macs)| (t.to_ascii_lowercase(), macs.iter().map(|m| m.to_string()).collect()))
            .collect()
    }

    #[test]
    fn pair_launch_waits_for_members() {
        let mem = memory(&[("t1", &["AA", "BB"])]);
        let both = fold_pairs(vec![pod("AA", Some("t1"), true, None), pod("BB", Some("t1"), false, None)], &mem);
        let one = fold_pairs(vec![pod("AA", Some("t1"), true, None)], &mem);
        assert_eq!(pair_launch_step("airplay:AA", Some(&both[0])), PairLaunch::Ready("airplay:pair:t1".into()));
        assert_eq!(pair_launch_step("airplay:pair:t1", Some(&one[0])), PairLaunch::Wait);
        assert_eq!(pair_launch_step("airplay:pair:t1", None), PairLaunch::Wait);
        // A HomePod with a group id, partner not yet seen.
        assert_eq!(pair_launch_step("airplay:AA", Some(&pod("AA", Some("t1"), true, None))), PairLaunch::Wait);
        assert_eq!(pair_launch_step("airplay:CC", Some(&pod("CC", None, false, None))), PairLaunch::NotAPair);
        assert_eq!(pair_launch_step("airplay:ZZ", None), PairLaunch::NotAPair);
    }

    #[test]
    fn group_txt_normalises_and_ignores_empty() {
        assert_eq!(group_txt(Some(" ABC-1 "), None, true).unwrap().tsid, "abc-1");
        assert!(group_txt(Some(""), None, false).is_none());
        assert!(group_txt(Some("00000000-0000"), None, false).is_none());
        assert!(group_txt(None, None, false).is_none());
    }

    #[test]
    fn pair_folds_into_one_entry_with_leader_first() {
        let singles = vec![
            pod("AA", Some("T1"), false, Some("Living Room")),
            pod("BB", Some("T1"), true, Some("Living Room")),
            pod("CC", Some("T9"), false, None), // single device with its own tsid
        ];
        let v = fold_pairs(singles, &memory(&[("T1", &["AA", "BB"]), ("T9", &["CC"])]));
        assert_eq!(v.len(), 2);
        let pair = v.iter().find(|r| r.pair.is_some()).unwrap();
        assert_eq!(pair.stable_id(), "airplay:pair:t1");
        assert_eq!(pair.friendly_name, "Living Room");
        let p = pair.pair.as_ref().unwrap();
        assert_eq!(p.members[0].mac_id, "BB"); // igl=1 leads
        assert!(p.is_complete());
        assert_eq!(pair.transport(), Some(Transport::AirPlay2));
        assert!(v.iter().any(|r| r.stable_id() == "airplay:CC"));
    }

    #[test]
    fn incomplete_pair_stays_listed_but_unplayable() {
        let v = fold_pairs(vec![pod("AA", Some("t1"), false, None)], &memory(&[("t1", &["AA", "BB"])]));
        assert_eq!(v.len(), 1);
        let p = v[0].pair.as_ref().unwrap();
        assert_eq!((p.members.len(), p.expected), (1, 2));
        assert!(!p.is_complete());
        assert_eq!(v[0].transport(), None);
        // No gpn: named after the member present.
        assert_eq!(v[0].friendly_name, "Pod AA");
    }

    #[test]
    fn member_id_resolves_to_pair_and_membership_follows_tsid() {
        let state = AirPlayDiscoveryState::default();
        let info = |tsid: &str, leader: bool| AirPlayInfo {
            friendly_name: "x".into(),
            ip: Some("10.0.0.2".parse().unwrap()),
            port: 7000,
            features: Some(HOMEPOD_FT),
            pk: None,
            model: Some("AudioAccessory5,1".into()),
            group: group_txt(Some(tsid), Some("Pair".into()), leader),
        };
        state.upsert_airplay("AA".into(), "a._airplay._tcp.local.", info("t1", true));
        state.upsert_airplay("BB".into(), "b._airplay._tcp.local.", info("t1", false));
        assert_eq!(state.renderers().len(), 1);
        assert_eq!(state.find_by_id("airplay:BB").unwrap().stable_id(), "airplay:pair:t1");
        // Member goes offline: still one entry, 1 of 2.
        state.remove_airplay_instance("b._airplay._tcp.local.");
        let v = state.renderers();
        assert_eq!(v.len(), 1);
        assert!(!v[0].pair.as_ref().unwrap().is_complete());
        // A late removal of a device's previous instance name doesn't
        // evict its current record.
        state.upsert_airplay("BB".into(), "b2._airplay._tcp.local.", info("t1", false));
        state.upsert_airplay("BB".into(), "b3._airplay._tcp.local.", info("t1", false));
        state.remove_airplay_instance("b2._airplay._tcp.local.");
        assert!(state.renderers()[0].pair.as_ref().unwrap().is_complete());
        // Other receivers sharing a tsid are never folded, and a removal
        // never evicts their record.
        let mut other = info("t3", false);
        other.model = Some("Speaker1,1".into());
        state.upsert_airplay("CC".into(), "c._airplay._tcp.local.", other.clone());
        state.upsert_airplay("DD".into(), "d._airplay._tcp.local.", other);
        assert!(state.find_by_id("airplay:CC").unwrap().pair.is_none());
        state.remove_airplay_instance("c._airplay._tcp.local.");
        assert!(state.find_by_id("airplay:CC").unwrap().features.is_some());
        // Unpaired (new tsid): back to two single devices.
        state.upsert_airplay("BB".into(), "b._airplay._tcp.local.", info("t2", true));
        assert_eq!(state.renderers().iter().filter(|r| r.pair.is_none()).count(), 4);
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
