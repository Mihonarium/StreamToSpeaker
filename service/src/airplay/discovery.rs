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
//! | `cn`      | Comma-separated codecs: 0=PCM 1=ALAC 2=AAC 3=AAC-ELD; absent = ALAC (every RAOP receiver plays it) |
//! | `et`      | Comma-separated encryption types: 0=none 1=RSA (AirPort Express key) 3=FairPlay 4=MFi-SAP (third-party MFi devices) 5=FairPlay SAPv2.5; absent = none |
//! | `tp`      | Transports: `UDP`, `TCP`; one without UDP is unsupported here |
//! | `md`      | Metadata it takes: 0=track text 1=artwork 2=progress |
//! | `vn`      | RSA version (3 = the published Apple key)       |
//! | `pw`      | Password protected                              |
//! | `ek`      | Expects an encryption key (classic AirPort Express) |
//! | `am`      | Apple model, e.g. `AirPort4,107`                |
//!
//! `et` decides the encryption: plaintext when 0 is offered, and for MFi
//! receivers (`et` lists 4, with or without 0) after `/auth-setup`, as
//! OwnTone and libraop do; RSA for classic AirPort Express (see
//! [`AirPlayRenderer::prefers_rsa_encryption`]); never FairPlay, so a
//! receiver offering only 3 and/or 5 is unsupported.
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
use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::discovery_net::LanTargets;

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
    /// Encryption types from RAOP `et=` (0=none 1=RSA 3=FairPlay 4=MFi-SAP
    /// 5=FairPlay SAPv2.5); see the table at the top of this module.
    pub encryption_types: Vec<u8>,
    /// Codec ids from RAOP `cn=` (1=ALAC).
    pub codecs: Vec<u8>,
    /// True if the RAOP service set `pw=true` (password protected). These
    /// are supported: the session answers RTSP Digest auth with the
    /// stored password, and the window asks for one.
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
    /// What the `_raop._tcp` announcement says beyond the fields above.
    pub raop: RaopDetails,
}

/// Details of a receiver's `_raop._tcp` announcement. Default (empty)
/// for a receiver without one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaopDetails {
    /// The TXT record as announced, in order, for the log of a connect.
    pub txt: Vec<(String, String)>,
    /// Every IPv4 address announced for the service, in the order a
    /// connect tries them (see [`order_addresses`]); `ip` is the first.
    pub addresses: Vec<IpAddr>,
    /// `tp`: the transports it offers (`UDP`, `TCP`), when it says.
    pub transports: Option<Vec<String>>,
    /// `md`: the metadata it takes: 0 track text, 1 artwork, 2 progress.
    pub metadata: Vec<u8>,
    /// The `_raop._tcp` instance (full name, lower case) this entry
    /// describes.
    pub instance: Option<String>,
    /// Set when another live instance claimed this entry's identity (MAC)
    /// before it. Gates ask [`AirPlayRenderer::is_newcomer`]: a newcomer
    /// is played only when a person picks it (by its own id,
    /// `airplay:<MAC>/<instance>`), one session at a time, is never folded
    /// into a stereo pair or taken as another entry's AirPlay 2 sibling,
    /// and nothing stored for a speaker is used with it.
    pub newcomer: bool,
}

/// May a connect go to `ip`, an address a receiver announced? Each is
/// judged by the classification UPnP discovery uses
/// ([`LanTargets::refusal`]; an IPv4-mapped IPv6 address counts as the
/// IPv4 address it carries): this PC (loopback, one of its own addresses)
/// and what is no device's address (unspecified, multicast, broadcast,
/// a subnet's own address) are dropped; an address outside this PC's
/// subnets stays (a speaker on another VLAN, announced through an mDNS
/// repeater). Only IPv4: AirPlay 1 has never connected over IPv6 here,
/// and this PC's own IPv6 addresses aren't known to the classification.
/// A receiver running on this PC is added by address instead.
pub fn may_contact(ip: IpAddr, lan: &LanTargets) -> bool {
    let ip = crate::stream_gate::canonical_ip(ip);
    match lan.refusal(ip) {
        None | Some(crate::discovery_net::Refusal::Elsewhere(_)) => ip.is_ipv4(),
        Some(_) => false,
    }
}

/// The addresses of `addrs` a connect tries, in order: the one that last
/// carried a session in this run, then those on a subnet of one of this
/// PC's adapters, then other routable ones, then link-local, each group by
/// address so the order is the same on every start. Only addresses a
/// connect may go to are kept (see [`may_contact`]); link-local addresses
/// stay, since a PC and a speaker on a link with no DHCP use them.
pub fn order_addresses(addrs: &[IpAddr], working: Option<IpAddr>, lan: &LanTargets) -> Vec<IpAddr> {
    let class = |a: &IpAddr| -> u8 {
        let link_local = matches!(a, IpAddr::V4(v4) if v4.is_link_local());
        if Some(*a) == working {
            0
        } else if lan.allows(*a) {
            1
        } else if !link_local {
            2
        } else {
            3
        }
    };
    let mut v: Vec<IpAddr> = addrs.iter().copied().filter(|a| may_contact(*a, lan)).collect();
    v.sort_by_key(|a| (class(a), *a));
    v.dedup();
    v
}

/// Most TXT entries, and characters per value, a log line shows.
const TXT_LOG_ENTRIES: usize = 40;
const TXT_LOG_VALUE_CHARS: usize = 100;

impl RaopDetails {
    /// The TXT record for a log line: `key="value"` pairs, values quoted
    /// and escaped (they come from the network) and cut short.
    pub fn txt_line(&self) -> String {
        let mut parts: Vec<String> = self
            .txt
            .iter()
            .take(TXT_LOG_ENTRIES)
            .map(|(k, v)| {
                let mut cut: String = v.chars().take(TXT_LOG_VALUE_CHARS).collect();
                if cut.len() < v.len() {
                    cut.push('…');
                }
                format!("{}={:?}", k.escape_debug(), cut)
            })
            .collect();
        if self.txt.len() > TXT_LOG_ENTRIES {
            parts.push(format!("(and {} more)", self.txt.len() - TXT_LOG_ENTRIES));
        }
        parts.join(" ")
    }
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
        match (&self.pair, self.raop.newcomer.then_some(self.raop.instance.as_deref()).flatten()) {
            (Some(p), _) => format!("{}{}", PAIR_ID_PREFIX, p.tsid),
            (None, Some(instance)) => format!("airplay:{}/{}", self.mac_id, instance),
            (None, None) => format!("airplay:{}", self.mac_id),
        }
    }

    /// True if this device exposes a usable **legacy RAOP** path: a RAOP
    /// port, ALAC, and either no encryption or Apple-RSA (i.e. not a
    /// FairPlay-only `et`). Password-protected (`pw=true`) receivers are
    /// supported — we speak RTSP Digest auth.
    ///
    /// A receiver that also has an AirPlay 2 path is judged by the rule it
    /// has always been routed by: its record must list ALAC (`cn` 1) and
    /// plaintext or RSA (`et` 0 or 1), else it plays over AirPlay 2 only.
    /// For one without, the record is read as the reference senders read
    /// it: no codec list means ALAC (every RAOP receiver plays it; OwnTone
    /// never reads `cn`), no `et`, or `et` with 4 but neither 0 nor 1,
    /// means plaintext (see [`Self::raop_encryption_types`]), and a `tp`
    /// that names transports without UDP rules it out (OwnTone discards
    /// those; we have no TCP transport).
    pub fn supports_legacy_raop(&self) -> bool {
        if self.port == 0 {
            return false;
        }
        if self.supports_airplay2() {
            let et = &self.encryption_types;
            return self.codecs.contains(&1) && (et.contains(&0) || et.contains(&1));
        }
        if let Some(tp) = &self.raop.transports {
            if !tp.iter().any(|t| t.eq_ignore_ascii_case("UDP")) {
                return false;
            }
        }
        if !self.codecs.is_empty() && !self.codecs.contains(&1) {
            return false;
        }
        let et = self.raop_encryption_types();
        et.contains(&0) || et.contains(&1)
    }

    /// Does this entry stand for a newcomer (see [`RaopDetails::newcomer`]):
    /// it is one, or a pair with one among its members?
    pub fn is_newcomer(&self) -> bool {
        self.raop.newcomer || self.pair.as_ref().is_some_and(|p| p.members.iter().any(|m| m.raop.newcomer))
    }

    /// Does the receiver take track text (`md` lists 0)? OwnTone sends
    /// text only then (`RAOP_MD_WANTS_TEXT`), libraop likewise.
    pub fn wants_track_text(&self) -> bool {
        self.raop.metadata.contains(&0)
    }

    /// The `et` list to go by: as announced, with plaintext (0) added when
    /// the receiver announced none, or announced MFi (4) without 0 or 1.
    /// The reference senders stream plaintext to both: OwnTone reads `et`
    /// only to decide on `/auth-setup` and encrypts only for `ek=1` or a
    /// classic AirPort Express; libraop sends `/auth-setup` when `et`
    /// lists 4 and encrypts only when its caller asks.
    pub fn raop_encryption_types(&self) -> Vec<u8> {
        let et = &self.encryption_types;
        if et.is_empty() {
            return vec![0];
        }
        let mut et = et.clone();
        if et.contains(&4) && !et.contains(&0) && !et.contains(&1) {
            et.insert(0, 0);
        }
        et
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
// Per-service partial records. RAOP records are kept per announced
// instance (the MAC in an instance name is only a claim), AirPlay ones by
// MAC; they are merged on read so resolution order (and one service
// vanishing) can't corrupt the other half.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RaopInfo {
    /// The instance's full name, lower case: what identifies it.
    fullname: String,
    metadata: Vec<u8>,
    /// The MAC the instance name claims.
    mac: String,
    transports: Option<Vec<String>>,
    friendly_name: String,
    txt: Vec<(String, String)>,
    /// Its IPv4 addresses, sorted.
    addresses: Vec<IpAddr>,
    /// When this instance was first resolved, in resolution order (kept
    /// when it is resolved again).
    first_seen: u64,
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
    /// Its IPv4 addresses, sorted.
    addresses: Vec<IpAddr>,
    port: u16,
    features: Option<u64>,
    pk: Option<String>,
    model: Option<String>,
    group: Option<GroupTxt>,
}

/// Stable-id prefix of a stereo-pair entry.
pub const PAIR_ID_PREFIX: &str = "airplay:pair:";

/// How long this PC's adapter list is reused for ordering addresses.
const LAN_REFRESH: Duration = Duration::from_secs(5);

/// This PC's networks, for ordering addresses: read again every
/// [`LAN_REFRESH`], or fixed (tests).
#[derive(Default)]
struct LanView {
    fixed: Option<LanTargets>,
    cached: Mutex<Option<(Instant, LanTargets)>>,
}

impl LanView {
    fn get(&self) -> LanTargets {
        if let Some(f) = &self.fixed {
            return f.clone();
        }
        let mut c = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        match c.as_ref() {
            Some((at, lan)) if at.elapsed() < LAN_REFRESH => lan.clone(),
            _ => {
                let lan = LanTargets::current();
                *c = Some((Instant::now(), lan.clone()));
                lan
            }
        }
    }
}

/// Most `_raop._tcp` instances kept, and most claiming one identity
/// (MAC): far beyond any real network (a rename shows two for a while,
/// two receiver programs on one machine may share one), small enough that
/// building the list on every repaint stays cheap whatever is announced.
/// Instances past either bound are not listed (and logged once); the ones
/// seen first stay, so an incumbent is never pushed out.
const MAX_RAOP_INSTANCES: usize = 256;
const MAX_CLAIMS_PER_IDENTITY: usize = 4;
/// Most `_airplay._tcp` records kept (one per device ID), on the same
/// reasoning.
const MAX_AIRPLAY_RECORDS: usize = 256;

/// Does `id` have the form `airplay:<MAC>/<instance>`? Only a newcomer's
/// id has it (see [`AirPlayRenderer::stable_id`]): it names that one
/// announcement, so nothing is stored under it, even once the conflict
/// has ended and the record is no longer marked.
pub fn is_instance_id(id: &str) -> bool {
    id.strip_prefix("airplay:")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(mac, instance)| !mac.is_empty() && !mac.contains(':') && !instance.is_empty())
}

/// Shared discovery state. Mirrors `ssdp::DiscoveryState` semantics.
#[derive(Default)]
pub struct AirPlayDiscoveryState {
    /// `_raop._tcp` records by instance fullname (lower case).
    raop: Mutex<HashMap<String, RaopInfo>>,
    /// Resolutions so far, for [`RaopInfo::first_seen`].
    resolved: AtomicU64,
    /// MAC → the address that last carried a session in this run.
    working: Mutex<HashMap<String, IpAddr>>,
    /// Announcements not listed because of their addresses, reported once.
    reported: crate::discovery_net::Reported,
    lan: LanView,
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
            let lan = self.lan.get();
            let working = self.working.lock().unwrap().clone();
            let raop = self.raop.lock().unwrap();
            let airplay = self.airplay.lock().unwrap();
            // One pass to group instances by identity.
            let mut by_mac: HashMap<&str, Vec<&RaopInfo>> = HashMap::new();
            for r in raop.values() {
                by_mac.entry(r.mac.as_str()).or_default().push(r);
            }
            let mut macs: BTreeSet<String> = BTreeSet::new();
            macs.extend(by_mac.keys().map(|m| m.to_string()));
            macs.extend(airplay.keys().cloned());
            macs.into_iter()
                .flat_map(|mac| {
                    let claims = by_mac.get(mac.as_str()).cloned().unwrap_or_default();
                    entries_for(&mac, &claims, airplay.get(&mac), working.get(&mac).copied(), &lan)
                })
                .collect::<Vec<_>>()
        };
        let memory = self.pair_members.lock().unwrap().clone();
        let mut v = fold_pairs(singles, &memory);
        v.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        v
    }

    /// The entries for one MAC: one, or one per instance while several
    /// claim it (see [`entries_for`]).
    fn entries_for_mac(&self, mac: &str) -> Vec<AirPlayRenderer> {
        let lan = self.lan.get();
        let working = self.working.lock().unwrap().get(mac).copied();
        let raop = self.raop.lock().unwrap();
        let airplay = self.airplay.lock().unwrap();
        let claims: Vec<&RaopInfo> = raop.values().filter(|r| r.mac == mac).collect();
        entries_for(mac, &claims, airplay.get(mac), working, &lan)
    }

    /// Find by our public `stable_id()` (i.e. with the `airplay:` prefix).
    /// A stereo-pair member's id resolves to its pair: playing one member
    /// alone would break the pair. While several instances claim one MAC,
    /// the plain id finds the incumbent (see `entries_for`), and
    /// `airplay:<MAC>/<instance>` the instance it names, also once the
    /// conflict is over.
    pub fn find_by_id(&self, id: &str) -> Option<AirPlayRenderer> {
        if id.starts_with(PAIR_ID_PREFIX) {
            return self.renderers().into_iter().find(|r| r.stable_id() == id);
        }
        let rest = id.strip_prefix("airplay:")?;
        if let Some((mac, instance)) = rest.split_once('/') {
            let instance = instance.to_ascii_lowercase();
            return self.entries_for_mac(mac).into_iter().find(|r| r.raop.instance.as_deref() == Some(instance.as_str()));
        }
        let mac = rest.to_string();
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
        self.entries_for_mac(&mac).into_iter().find(|r| !r.raop.newcomer)
    }

    /// A session to `r` came up (RECORD answered, any password
    /// accepted): its address is tried first from now on. A session on a
    /// newcomer changes nothing: it is one session, nothing more.
    pub fn note_session(&self, r: &AirPlayRenderer) {
        if !r.raop.newcomer {
            self.working.lock().unwrap().insert(r.mac_id.clone(), r.ip);
        }
    }

    #[cfg(test)]
    pub(crate) fn working_address(&self, mac: &str) -> Option<IpAddr> {
        self.working.lock().unwrap().get(mac).copied()
    }

    /// A resolution, as the browse threads deliver it (for tests
    /// elsewhere in the crate).
    #[cfg(test)]
    pub(crate) fn ingest_for_tests(&self, service: &str, info: &ServiceInfo) {
        ingest_resolution(self, service, info);
    }

    /// A goodbye, as the browse threads deliver it (for tests elsewhere in
    /// the crate).
    #[cfg(test)]
    pub(crate) fn remove_for_tests(&self, service: &str, fullname: &str) {
        ingest_removal(self, service, fullname);
    }

    #[cfg(test)]
    pub(crate) fn with_lan(lan: LanTargets) -> Self {
        Self { lan: LanView { fixed: Some(lan), cached: Mutex::new(None) }, ..Default::default() }
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

    /// Store the record of one `_raop._tcp` instance, replacing what that
    /// instance announced before (a new address included).
    fn upsert_raop(&self, mut info: RaopInfo) {
        let mut raop = self.raop.lock().unwrap();
        let known = raop.get(&info.fullname).map(|r| r.first_seen);
        if known.is_none() {
            // A new instance. Bounded, so a flood of made-up announcements
            // can't make the list (built on every repaint) expensive; what
            // is kept is the instances seen first, the incumbents among
            // them.
            let same_identity = raop.values().filter(|r| r.mac == info.mac).count();
            if same_identity >= MAX_CLAIMS_PER_IDENTITY || raop.len() >= MAX_RAOP_INSTANCES {
                let name = crate::discovery_net::shown(&info.fullname);
                self.reported.once(&format!("airplay-full:{}", info.fullname), || {
                    format!("AirPlay: not listing {}: too many AirPlay receivers, or claims of one identity, announced", name)
                });
                return;
            }
            if let Some(other) = raop.values().find(|r| r.mac == info.mac) {
                let (name, theirs) = (crate::discovery_net::shown(&info.fullname), crate::discovery_net::shown(&other.fullname));
                let (ours_at, theirs_at) = (info.addresses.clone(), other.addresses.clone());
                self.reported.once(&format!("airplay-claim:{}", info.fullname), || {
                    format!(
                        "AirPlay: {} at {:?} announces the identity of {} at {:?}; it is listed as another receiver with the same ID",
                        name, ours_at, theirs, theirs_at
                    )
                });
            }
        }
        info.first_seen = known.unwrap_or_else(|| self.resolved.fetch_add(1, Ordering::Relaxed));
        raop.insert(info.fullname.clone(), info);
    }

    fn upsert_airplay(&self, mac: String, fullname: &str, info: AirPlayInfo) {
        {
            let airplay = self.airplay.lock().unwrap();
            if !airplay.contains_key(&mac) && airplay.len() >= MAX_AIRPLAY_RECORDS {
                let name = crate::discovery_net::shown(fullname);
                self.reported.once(&format!("airplay2-full:{}", fullname), || {
                    format!("AirPlay: not listing {}: too many AirPlay 2 receivers announced", name)
                });
                return;
            }
        }
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

    /// Forget every receiver (the browse is being restarted on a
    /// different set of interfaces).
    pub fn clear(&self) {
        self.raop.lock().unwrap().clear();
        self.airplay.lock().unwrap().clear();
        self.reported.clear();
    }

    /// A goodbye for one `_raop._tcp` instance removes that instance only:
    /// a renamed receiver's late goodbye for its old name leaves the new
    /// record, and a device announcing someone else's MAC can't remove
    /// theirs.
    fn remove_raop_instance(&self, fullname: &str) {
        self.raop.lock().unwrap().remove(&fullname.to_ascii_lowercase());
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
    // A newcomer claiming a member's MAC is never folded in: it stays an
    // entry of its own.
    let pair_of = |r: &AirPlayRenderer| -> Option<String> {
        if r.is_newcomer() {
            return None;
        }
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

/// The entries for one MAC. One `_raop._tcp` instance (or none) makes one
/// entry, as always. Several live instances claiming one MAC are never
/// merged, whatever their addresses: anyone can announce a receiver's
/// MAC, and with it addresses that include the receiver's own. Each is
/// listed on its own, with only what it announced, named with its
/// address. The incumbent is the instance the app knew before the
/// conflict began, the one resolved first, and nothing during the
/// conflict changes that (not a session on another one, not a newer
/// announcement): it keeps the plain id, what is stored under it, and
/// the `_airplay._tcp` record; the others are newcomers (see
/// [`RaopDetails::newcomer`]). The conflict ends when one of them goes
/// (goodbye or expiry); the one left is then an ordinary entry. A rename
/// looks like this for as long as both names are announced.
fn entries_for(mac: &str, claims: &[&RaopInfo], airplay: Option<&AirPlayInfo>, working: Option<IpAddr>, lan: &LanTargets) -> Vec<AirPlayRenderer> {
    let mut claims: Vec<&RaopInfo> = claims.to_vec();
    claims.sort_by_key(|r| r.first_seen);
    if claims.len() <= 1 {
        return merge(mac, claims.first().copied(), airplay, working, lan).into_iter().collect();
    }
    let mut out = Vec::new();
    let incumbent = merge(mac, claims.first().copied(), airplay, working, lan);
    // The speaker's own name, for the newcomers' rows.
    let speaker = incumbent
        .as_ref()
        .map(|e| e.friendly_name.clone())
        .or_else(|| claims.first().map(|r| r.friendly_name.clone()))
        .unwrap_or_else(|| mac.to_string());
    let speaker_addresses: Vec<IpAddr> = claims.first().map(|r| r.addresses.clone()).unwrap_or_default();
    if let Some(mut e) = incumbent {
        e.friendly_name = format!("{} ({})", e.friendly_name, e.ip);
        out.push(e);
    }
    for r in claims.into_iter().skip(1) {
        let Some(mut e) = merge(mac, Some(r), None, None, lan) else { continue };
        // Its own addresses before any it shares with the speaker: that is
        // where this other receiver is, and what its row shows.
        e.raop.addresses.sort_by_key(|a| speaker_addresses.contains(a));
        if let Some(first) = e.raop.addresses.first() {
            e.ip = *first;
        }
        // Says what it is, true also for two receiver programs on one
        // machine that share an identity; starting with the speaker's name
        // keeps it below the speaker's row in the sorted list.
        e.friendly_name = format!("{} — another receiver with the same ID: {} ({})", speaker, e.friendly_name, e.ip);
        e.raop.newcomer = true;
        out.push(e);
    }
    out
}

/// Build a public [`AirPlayRenderer`] from whichever partial records we
/// have for one MAC. Returns None if neither half has a usable IPv4.
fn merge(
    mac: &str,
    raop: Option<&RaopInfo>,
    airplay: Option<&AirPlayInfo>,
    working: Option<IpAddr>,
    lan: &LanTargets,
) -> Option<AirPlayRenderer> {
    let raop_addresses = raop.map(|r| order_addresses(&r.addresses, working, lan)).unwrap_or_default();
    let ip = raop_addresses
        .first()
        .copied()
        .or_else(|| airplay.and_then(|a| order_addresses(&a.addresses, working, lan).first().copied()))?;

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
        raop: RaopDetails {
            txt: raop.map(|r| r.txt.clone()).unwrap_or_default(),
            addresses: raop_addresses,
            transports: raop.and_then(|r| r.transports.clone()),
            metadata: raop.map(|r| r.metadata.clone()).unwrap_or_default(),
            instance: raop.map(|r| r.fullname.clone()),
            newcomer: false,
        },
    })
}

/// Browse both `_raop._tcp` and `_airplay._tcp` on a caller-built daemon
/// (one whose interface selection the caller controls); a consumer
/// thread per service keeps the corresponding map fresh. Consumers exit
/// when the daemon shuts down.
pub fn spawn_airplay_discovery_on(
    state: Arc<AirPlayDiscoveryState>,
    daemon: ServiceDaemon,
) -> Result<()> {
    info!("AirPlay discovery: browsing {} + {}", RAOP_SERVICE, AIRPLAY_SERVICE);

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
                            ingest_removal(&state, service, &fullname);
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

/// A goodbye (or expiry) of one instance.
fn ingest_removal(state: &AirPlayDiscoveryState, service: &str, fullname: &str) {
    debug!("AirPlay removed ({}): {}", service, fullname);
    if service == RAOP_SERVICE {
        state.remove_raop_instance(fullname);
    } else {
        state.remove_airplay_instance(fullname);
    }
}

fn ingest_resolution(state: &AirPlayDiscoveryState, service: &str, info: &ServiceInfo) {
    let addresses = v4_addresses(info);
    let txt = info.get_properties();
    report_refused(state, &state.lan.get(), info.get_fullname(), &addresses);

    if service == RAOP_SERVICE {
        // Service name: "<MAC>@<FriendlyName>._raop._tcp.local."
        let Some((mac, friendly)) = split_raop_name(info.get_fullname()) else {
            debug!("AirPlay RAOP: can't parse name {}", info.get_fullname());
            return;
        };
        let r = RaopInfo {
            fullname: info.get_fullname().to_ascii_lowercase(),
            metadata: read_txt_int_list(txt, "md"),
            mac,
            // An empty `tp` says nothing, like none.
            transports: read_txt_string(txt, "tp")
                .map(|tp| tp.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect::<Vec<_>>())
                .filter(|tp| !tp.is_empty()),
            friendly_name: friendly,
            txt: txt
                .iter()
                .map(|p| (p.key().to_string(), p.val().map(|v| String::from_utf8_lossy(v).into_owned()).unwrap_or_default()))
                .collect(),
            addresses,
            first_seen: 0,
            port: info.get_port(),
            encryption_types: read_txt_int_list(txt, "et"),
            codecs: read_txt_int_list(txt, "cn"),
            password_protected: read_txt_bool(txt, "pw"),
            encryption_key_required: read_txt_bool(txt, "ek"),
            model: read_txt_string(txt, "am"),
        };
        debug!(
            "AirPlay RAOP resolved: {} @ {:?}:{} et={:?} cn={:?}",
            r.friendly_name, r.addresses, r.port, r.encryption_types, r.codecs
        );
        state.upsert_raop(r);
    } else {
        // _airplay._tcp: MAC is in the `deviceid` TXT key; the service
        // instance name is the friendly name.
        let Some(mac) = read_txt_string(txt, "deviceid").map(|s| normalise_mac(&s)) else {
            debug!("AirPlay v2: no deviceid in {}", info.get_fullname());
            return;
        };
        let a = AirPlayInfo {
            friendly_name: airplay_instance_name(info.get_fullname()),
            addresses,
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
            a.friendly_name, a.addresses, a.port, a.features, a.model, a.group
        );
        state.upsert_airplay(mac, info.get_fullname(), a);
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Say why an announcement whose addresses this app won't contact (see
/// [`may_contact`]) isn't listed: once per instance at info level, later
/// at debug (as UPnP discovery reports a refused address). An
/// announcement with some addresses left says nothing.
fn report_refused(state: &AirPlayDiscoveryState, lan: &LanTargets, fullname: &str, addresses: &[IpAddr]) {
    if addresses.iter().any(|a| may_contact(*a, lan)) {
        return;
    }
    let reasons: Vec<String> = addresses
        .iter()
        .map(|a| match lan.refusal(crate::stream_gate::canonical_ip(*a)) {
            Some(why) => format!("{} ({})", a, why),
            None => format!("{} (an IPv6 address)", a),
        })
        .collect();
    let hint = addresses
        .iter()
        .find_map(|a| lan.refusal(crate::stream_gate::canonical_ip(*a)).and_then(|why| why.hint()));
    let name = crate::discovery_net::shown(fullname);
    state.reported.once(&format!("airplay:{}", fullname.to_ascii_lowercase()), || {
        format!(
            "AirPlay: not listing {}: not contacting it at {}.{}",
            name,
            if reasons.is_empty() { "no address".to_string() } else { reasons.join(", ") },
            hint.map(|h| format!(" {}", h)).unwrap_or_default()
        )
    });
}

/// The instance's IPv4 addresses, sorted (the order is chosen on read,
/// see [`order_addresses`]).
fn v4_addresses(info: &ServiceInfo) -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = info.get_addresses_v4().into_iter().map(|a| IpAddr::V4(*a)).collect();
    v.sort();
    v
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
    parse_features(&read_txt_string(txt, "features").or_else(|| read_txt_string(txt, "ft"))?)
}

/// The flag word of a `features` / `ft` value (see [`read_features`]).
fn parse_features(raw: &str) -> Option<u64> {
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
            raop: RaopDetails::default(),
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
        let state = AirPlayDiscoveryState::with_lan(LanTargets::from_networks([("10.0.0.1".parse().unwrap(), 24)]));
        let info = |tsid: &str, leader: bool| AirPlayInfo {
            friendly_name: "x".into(),
            addresses: vec!["10.0.0.2".parse().unwrap()],
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

    fn raop_info(instance: &str, addrs: &str, txt: &[(&str, &str)]) -> ServiceInfo {
        ServiceInfo::new(RAOP_SERVICE, instance, "speaker.local.", addrs, 5000, txt).unwrap()
    }

    #[test]
    fn the_raop_txt_record_is_kept_for_the_log() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        let info = raop_info("AABBCCDDEEFF@Dock", "192.168.1.5", &[("et", "0,4"), ("cn", "0,1"), ("am", "Dock\n1")]);
        ingest_resolution(&state, RAOP_SERVICE, &info);
        let r = state.find_by_id("airplay:AABBCCDDEEFF").unwrap();
        let mut txt = r.raop.txt.clone();
        txt.sort();
        assert_eq!(txt, [("am".to_string(), "Dock\n1".to_string()), ("cn".into(), "0,1".into()), ("et".into(), "0,4".into())]);
        // Quoted and escaped, so a value can't break the log line.
        assert!(r.raop.txt_line().contains(r#"am="Dock\n1""#), "{}", r.raop.txt_line());
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn home_lan() -> LanTargets {
        LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)])
    }

    /// The addresses a connect tries: those it may contact, worked address
    /// first, then this PC's subnets, then others, then link-local, in a
    /// fixed order whatever order they were announced in.
    #[test]
    fn addresses_are_ordered_whatever_order_they_come_in() {
        let lan = home_lan();
        let want = vec![ip("192.168.1.65"), ip("10.0.0.9"), ip("169.254.248.229")];
        let never = [
            ip("0.0.0.0"),
            ip("224.0.0.251"),
            ip("255.255.255.255"),
            ip("192.168.1.255"),
            ip("192.168.1.0"),
            ip("127.0.0.1"),
            ip("192.168.1.10"),
        ];
        for rot in 0..want.len() {
            let mut given = want.clone();
            given.rotate_left(rot);
            given.extend(never);
            given.rotate_left(rot);
            assert_eq!(order_addresses(&given, None, &lan), want, "{given:?}");
        }
        // The address that last carried a session goes first.
        assert_eq!(order_addresses(&want, Some(ip("10.0.0.9")), &lan)[0], ip("10.0.0.9"));
        // Link-local stays, also without a link-local adapter.
        assert_eq!(order_addresses(&[ip("169.254.248.229")], None, &lan), [ip("169.254.248.229")]);
    }

    /// This PC and what is no device's address are never candidates, in
    /// either address family; IPv6 isn't used at all.
    #[test]
    fn this_pc_and_non_addresses_are_never_candidates() {
        let lan = home_lan();
        for a in [
            "::1",
            "::",
            "ff02::fb",
            "::ffff:127.0.0.1",
            "::ffff:0.0.0.0",
            "::ffff:192.168.1.10",
            "::ffff:224.0.0.251",
            "127.0.0.1",
            "127.1.2.3",
            "192.168.1.10",
            "0.0.0.0",
            "255.255.255.255",
            "2001:db8::1",
            "fe80::1",
        ] {
            assert!(!may_contact(ip(a), &lan), "{a}");
        }
        for a in ["192.168.1.65", "10.20.0.5", "169.254.9.9", "::ffff:192.168.1.65"] {
            assert!(may_contact(ip(a), &lan), "{a}");
        }
    }

    /// Through the real resolution path: an AirPort Express announcing a
    /// link-local address next to its DHCP one is reached at the DHCP one,
    /// on every resolution (the announced set has no order of its own).
    #[test]
    fn a_receiver_with_a_link_local_address_is_listed_at_its_routable_one() {
        for _ in 0..20 {
            let state = AirPlayDiscoveryState::with_lan(home_lan());
            let info = raop_info("AABBCCDDEEFF@Express", "169.254.248.229,192.168.1.65", &[("et", "0,1"), ("cn", "0,1")]);
            ingest_resolution(&state, RAOP_SERVICE, &info);
            let r = state.find_by_id("airplay:AABBCCDDEEFF").unwrap();
            assert_eq!(r.ip, ip("192.168.1.65"));
            assert_eq!(r.raop.addresses, [ip("192.168.1.65"), ip("169.254.248.229")]);
        }
    }

    /// A speaker outside this PC's subnets (another VLAN, through an mDNS
    /// repeater) is listed and reached at its address; with one on- and
    /// one off-subnet address, the on-subnet one first; one that announces
    /// this PC's address next to a real one is tried at the real one only;
    /// one that announces only this PC (its own address, loopback) or no
    /// device's address isn't listed.
    #[test]
    fn receivers_are_listed_at_the_addresses_a_connect_may_go_to() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        let txt = &[("et", "0"), ("cn", "1")];
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE01@Vlan", "10.20.0.5", txt));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE02@Here", "192.168.1.10", txt));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE03@Loop", "127.0.0.1", txt));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE04@Both", "10.20.0.6,192.168.1.66", txt));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE05@Nowhere", "224.0.0.251", txt));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE06@Mixed", "192.168.1.10,192.168.1.67", txt));
        let at = |id: &str| state.find_by_id(id).map(|r| (r.ip, r.raop.addresses.clone()));
        assert_eq!(at("airplay:AABBCCDDEE01"), Some((ip("10.20.0.5"), vec![ip("10.20.0.5")])));
        assert_eq!(at("airplay:AABBCCDDEE02"), None);
        assert_eq!(at("airplay:AABBCCDDEE03"), None);
        assert_eq!(at("airplay:AABBCCDDEE04"), Some((ip("192.168.1.66"), vec![ip("192.168.1.66"), ip("10.20.0.6")])));
        assert_eq!(at("airplay:AABBCCDDEE05"), None);
        assert_eq!(at("airplay:AABBCCDDEE06"), Some((ip("192.168.1.67"), vec![ip("192.168.1.67")])));
        assert_eq!(state.renderers().len(), 3);
        // Each instance not listed is reported once.
        assert_eq!(state.reported.len(), 3);
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE03@Loop", "127.0.0.1", txt));
        assert_eq!(state.reported.len(), 3);
    }

    const TXT: [(&str, &str); 2] = [("et", "0,1"), ("cn", "0,1")];

    fn conflict_state() -> AirPlayDiscoveryState {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("B8E93735AC11@Kitchen", "192.168.1.65", &TXT));
        // Another host claims the MAC, at the receiver's own address and
        // one more, with its own port and TXT.
        let fake = ServiceInfo::new(RAOP_SERVICE, "B8E93735AC11@Kitchen 2", "evil.local.", "192.168.1.65,192.168.1.66", 6666, &[("et", "1"), ("cn", "1"), ("md", "0")][..]).unwrap();
        ingest_resolution(&state, RAOP_SERVICE, &fake);
        state
    }

    const NEWCOMER: &str = "airplay:B8E93735AC11/b8e93735ac11@kitchen 2._raop._tcp.local.";

    /// A second instance claiming a receiver's MAC is never merged into
    /// it, even with an address list that includes the receiver's own:
    /// both are listed and marked, and the newcomer's port, TXT and
    /// addresses never reach the incumbent's entry.
    #[test]
    fn a_second_instance_claiming_a_mac_never_merges_into_it() {
        let state = conflict_state();
        let v = state.renderers();
        assert_eq!(v.len(), 2, "{v:?}");
        let kitchen = state.find_by_id("airplay:B8E93735AC11").expect("the incumbent keeps the plain id");
        assert_eq!(kitchen.friendly_name, "Kitchen (192.168.1.65)");
        assert_eq!((kitchen.port, kitchen.encryption_types.clone()), (5000, vec![0, 1]));
        assert_eq!(kitchen.raop.addresses, [ip("192.168.1.65")]);
        assert_eq!(kitchen.raop.txt.len(), 2);
        assert!(!kitchen.raop.newcomer);
        let other = state.find_by_id(NEWCOMER).expect("the newcomer is picked by its own id");
        assert_eq!(other.stable_id(), NEWCOMER);
        assert!(other.raop.newcomer);
        assert_eq!(other.port, 6666);
        // Its row says what it is, shows the address it doesn't share with
        // the speaker (and connects there first), and sits below the
        // speaker's row.
        assert_eq!(other.friendly_name, "Kitchen — another receiver with the same ID: Kitchen 2 (192.168.1.66)");
        assert_eq!((other.ip, other.raop.addresses.clone()), (ip("192.168.1.66"), vec![ip("192.168.1.66"), ip("192.168.1.65")]));
        let names: Vec<String> = v.iter().map(|r| r.friendly_name.clone()).collect();
        assert_eq!(names, [kitchen.friendly_name.clone(), other.friendly_name.clone()]);
    }

    /// A newcomer whose name sorts before the speaker's still sits below it,
    /// and one that shares every address shows that address.
    #[test]
    fn a_newcomers_row_sits_below_the_speaker() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("B8E93735AC11@Kitchen", "192.168.1.65", &TXT));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("B8E93735AC11@Attic", "192.168.1.65", &TXT));
        let names: Vec<String> = state.renderers().iter().map(|r| r.friendly_name.clone()).collect();
        assert_eq!(names, ["Kitchen (192.168.1.65)", "Kitchen — another receiver with the same ID: Attic (192.168.1.65)"]);
    }

    /// The incumbent is the instance known before the conflict began;
    /// a session on the newcomer, or a newer announcement from it,
    /// changes nothing.
    #[test]
    fn nothing_during_a_conflict_makes_the_newcomer_the_incumbent() {
        let state = conflict_state();
        state.note_session(&state.find_by_id(NEWCOMER).unwrap());
        let fake = ServiceInfo::new(RAOP_SERVICE, "B8E93735AC11@Kitchen 2", "evil.local.", "192.168.1.66", 6666, &[("et", "1")][..]).unwrap();
        ingest_resolution(&state, RAOP_SERVICE, &fake);
        let plain = state.find_by_id("airplay:B8E93735AC11").unwrap();
        assert_eq!((plain.port, plain.ip, plain.raop.newcomer), (5000, ip("192.168.1.65"), false));
        assert!(state.find_by_id(NEWCOMER).unwrap().raop.newcomer);
        assert_eq!(state.working_address("B8E93735AC11"), None, "a newcomer's session marks no address");
    }

    /// A newcomer claiming a stereo-pair member's MAC is never folded into
    /// the pair; a pair with a newcomer among its members would count as
    /// one.
    #[test]
    fn a_newcomer_is_never_folded_into_a_pair() {
        let mem = memory(&[("t1", &["AA", "BB"])]);
        let mut fake = pod("AA", Some("t1"), false, None);
        fake.raop.newcomer = true;
        fake.raop.instance = Some("aa@fake._raop._tcp.local.".into());
        let v = fold_pairs(vec![pod("AA", Some("t1"), true, None), pod("BB", Some("t1"), false, None), fake], &mem);
        assert_eq!(v.len(), 2, "{v:?}");
        let pair = v.iter().find(|r| r.pair.is_some()).unwrap();
        assert!(!pair.is_newcomer());
        assert_eq!(pair.pair.as_ref().unwrap().members.len(), 2);
        let single = v.iter().find(|r| r.pair.is_none()).unwrap();
        assert!(single.is_newcomer());

        let mut with_newcomer = pair.clone();
        if let Some(p) = with_newcomer.pair.as_mut() {
            p.members[1].raop.newcomer = true;
        }
        assert!(with_newcomer.is_newcomer());
    }

    /// When the incumbent goes, the conflict is over: the one left is an
    /// ordinary entry with the plain id (and so what is stored under it).
    #[test]
    fn when_the_incumbent_goes_the_other_one_is_the_speaker() {
        let state = conflict_state();
        ingest_removal(&state, RAOP_SERVICE, "B8E93735AC11@Kitchen._raop._tcp.local.");
        let v = state.renderers();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].stable_id().as_str(), v[0].raop.newcomer, v[0].port), ("airplay:B8E93735AC11", false, 6666));
        // A pick made by its newcomer id still finds it.
        assert_eq!(state.find_by_id(NEWCOMER).unwrap().stable_id(), "airplay:B8E93735AC11");
    }

    /// A rename: both names announced for a while, then the old name's
    /// goodbye. It ends with one ordinary entry under the new name, also
    /// when the goodbye comes before the new name.
    #[test]
    fn a_rename_ends_with_one_unmarked_entry() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@Old", "192.168.1.65", &TXT));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@New", "192.168.1.65", &TXT));
        assert_eq!(state.renderers().len(), 2);
        ingest_removal(&state, RAOP_SERVICE, "AABBCCDDEEFF@Old._raop._tcp.local.");
        let v = state.renderers();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].friendly_name.as_str(), v[0].stable_id().as_str(), v[0].raop.newcomer), ("New", "airplay:AABBCCDDEEFF", false));
        // A real goodbye (the device switched off) still removes it.
        ingest_removal(&state, RAOP_SERVICE, "aabbccddeeff@new._raop._tcp.local.");
        assert!(state.find_by_id("airplay:AABBCCDDEEFF").is_none());

        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@Old", "192.168.1.65", &TXT));
        ingest_removal(&state, RAOP_SERVICE, "AABBCCDDEEFF@Old._raop._tcp.local.");
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@New", "192.168.1.65", &TXT));
        let v = state.renderers();
        assert_eq!((v.len(), v[0].friendly_name.as_str(), v[0].raop.newcomer), (1, "New", false));
    }

    /// A flood of announcements is bounded: a few claims per identity, a
    /// few hundred instances in all, the first seen kept.
    #[test]
    fn a_flood_of_announcements_is_bounded() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("B8E93735AC11@Kitchen", "192.168.1.65", &TXT));
        for i in 0..20 {
            ingest_resolution(&state, RAOP_SERVICE, &raop_info(&format!("B8E93735AC11@Fake {i}"), "192.168.1.66", &TXT));
        }
        let claims = state.renderers().into_iter().filter(|r| r.mac_id == "B8E93735AC11").count();
        assert_eq!(claims, MAX_CLAIMS_PER_IDENTITY);
        assert_eq!(state.find_by_id("airplay:B8E93735AC11").unwrap().raop.instance.as_deref(), Some("b8e93735ac11@kitchen._raop._tcp.local."));
        for i in 0..(MAX_RAOP_INSTANCES + 50) {
            let mac = format!("{:012X}", i + 1);
            ingest_resolution(&state, RAOP_SERVICE, &raop_info(&format!("{mac}@S{i}"), "192.168.1.70", &TXT));
        }
        assert_eq!(state.raop.lock().unwrap().len(), MAX_RAOP_INSTANCES);
        // Instances already kept are still updated.
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("B8E93735AC11@Kitchen", "192.168.1.71", &TXT));
        assert_eq!(state.find_by_id("airplay:B8E93735AC11").unwrap().ip, ip("192.168.1.71"));
    }

    /// A newcomer's goodbye removes only the newcomer.
    #[test]
    fn a_newcomers_goodbye_leaves_the_incumbent() {
        let state = conflict_state();
        ingest_removal(&state, RAOP_SERVICE, "B8E93735AC11@Kitchen 2._raop._tcp.local.");
        let v = state.renderers();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].friendly_name.as_str(), v[0].port, v[0].raop.newcomer), ("Kitchen", 5000, false));
    }

    /// A receiver whose address changed (its instance re-announced at a
    /// new address) just moves: no conflict, nothing to do.
    #[test]
    fn a_receiver_that_moved_keeps_working() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@Den", "192.168.1.65", &TXT));
        state.note_session(&state.find_by_id("airplay:AABBCCDDEEFF").unwrap());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@Den", "192.168.1.70", &TXT));
        let v = state.renderers();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].ip, v[0].stable_id().as_str(), v[0].friendly_name.as_str()), (ip("192.168.1.70"), "airplay:AABBCCDDEEFF", "Den"));
    }

    /// In a conflict the `_airplay._tcp` half stays with the incumbent.
    #[test]
    fn in_a_conflict_the_airplay_2_half_stays_with_the_incumbent() {
        let state = conflict_state();
        let ap = ServiceInfo::new(AIRPLAY_SERVICE, "Kitchen", "kitchen.local.", "192.168.1.65", 7000, &[("deviceid", "B8:E9:37:35:AC:11"), ("features", "0x445F8A00,0x1C340")][..]).unwrap();
        ingest_resolution(&state, AIRPLAY_SERVICE, &ap);
        let kitchen = state.find_by_id("airplay:B8E93735AC11").unwrap();
        let other = state.find_by_id(NEWCOMER).unwrap();
        assert!(kitchen.features.is_some() && kitchen.airplay_port == Some(7000));
        assert!(other.features.is_none() && other.airplay_port.is_none());
    }

    #[test]
    fn the_txt_log_line_is_bounded() {
        let long = "x".repeat(5000);
        let d = RaopDetails { txt: (0..100).map(|i| (format!("k{i}"), long.clone())).collect(), ..Default::default() };
        let line = d.txt_line();
        assert!(line.len() < 6000, "{}", line.len());
        assert!(line.ends_with("(and 60 more)"));
    }

    #[test]
    fn normalise_mac_strips_separators_and_uppercases() {
        assert_eq!(normalise_mac("b8:e9:37:35:ac:11"), "B8E93735AC11");
        assert_eq!(normalise_mac("B8E93735AC11"), "B8E93735AC11");
    }

    #[test]
    fn features_single_and_split_hex() {
        assert_eq!(parse_features("0x40000200"), Some(0x40000200));
        assert_eq!(parse_features("0X40000200"), Some(0x40000200));
        // Two halves, low first.
        assert_eq!(parse_features("0x445F8A00,0x1C340"), Some((0x1C340u64 << 32) | 0x445F8A00));
        assert_eq!(parse_features(" 0x445F8A00 , 0x1C340 "), Some((0x1C340u64 << 32) | 0x445F8A00));
        // Decimal, and hex digits without a prefix.
        assert_eq!(parse_features("512"), Some(512));
        assert_eq!(parse_features("445F8A00"), Some(0x445F8A00));
        // Nothing usable.
        assert_eq!(parse_features(""), None);
        assert_eq!(parse_features("0x"), None);
        assert_eq!(parse_features("0x445F8A00,zz"), None);
        assert_eq!(parse_features("0x1FFFFFFFFFFFFFFFF"), None);
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
        // The exact `features` word a Sonos advertises on _airplay._tcp:
        // AirPlay 2 with PTP and buffered audio.
        let ft = parse_features("0x445F8A00,0x1C340");
        assert!(ft.is_some(), "split-hex features should parse");
        let r = renderer(Some("One"), vec![0, 4], ft, 5000, Some(7000));
        assert!(r.supports_airplay2(), "Sonos must support AirPlay 2");
        assert!(r.expects_ptp(), "Sonos advertises PTP (feature bit 41)");
        assert!(r.supports_buffered_audio(), "Sonos advertises buffered audio (bit 40)");
        // It is still played over AirPlay 1 first, which is what iTunes
        // does (packet capture: /auth-setup opener, realtime UDP, NTP
        // timing); AirPlay 2 is the fallback the app tries next.
        assert!(r.supports_legacy_raop());
        assert!(!r.requires_airplay2());
        assert_eq!(r.transport(), Some(Transport::RaopLegacy));
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

    /// Follow the reference senders: no `cn` means ALAC, no `et` means
    /// plaintext; a `tp` without UDP rules the receiver out; FairPlay-only
    /// stays unsupported.
    #[test]
    fn a_receiver_without_a_codec_list_plays_alac() {
        let mut r = renderer(Some("Dock"), vec![0], None, 5000, None);
        r.codecs = vec![];
        assert!(r.supports_legacy_raop());
        assert_eq!(r.transport(), Some(Transport::RaopLegacy));
        r.encryption_types = vec![];
        assert!(r.supports_legacy_raop(), "no et: plaintext");
        assert_eq!(r.raop_encryption_types(), [0]);
        r.codecs = vec![0, 2];
        assert!(!r.supports_legacy_raop(), "a codec list without ALAC");
        r.codecs = vec![];
        r.raop.transports = Some(vec!["TCP".into()]);
        assert!(!r.supports_legacy_raop(), "TCP only");
        r.raop.transports = Some(vec!["TCP".into(), "udp".into()]);
        assert!(r.supports_legacy_raop());
        r.encryption_types = vec![3, 5];
        assert!(!r.supports_legacy_raop(), "FairPlay only");
    }

    /// `et` lists 4 without 0 or 1: an MFi receiver, played like one that
    /// lists 0 and 4.
    #[test]
    fn an_mfi_only_receiver_is_played_like_et_0_4() {
        for et in [vec![4], vec![3, 4, 5], vec![4, 5]] {
            let r = renderer(Some("Dock"), et.clone(), None, 5000, None);
            assert!(r.supports_legacy_raop(), "{et:?}");
            assert_eq!(r.transport(), Some(Transport::RaopLegacy));
            assert!(r.raop_encryption_types().contains(&0));
            assert!(!r.prefers_rsa_encryption());
        }
        // Lists that already say what to do are left as they are.
        assert_eq!(renderer(None, vec![0, 4], None, 5000, None).raop_encryption_types(), [0, 4]);
        assert_eq!(renderer(None, vec![1, 4], None, 5000, None).raop_encryption_types(), [1, 4]);
    }

    #[test]
    fn track_text_only_to_receivers_whose_md_lists_text() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE01@A", "192.168.1.5", &[("md", "0,1,2")]));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE02@B", "192.168.1.6", &[("md", "1,2")]));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE03@C", "192.168.1.7", &[("et", "0")]));
        let wants = |id: &str| state.find_by_id(id).unwrap().wants_track_text();
        assert!(wants("airplay:AABBCCDDEE01"));
        assert!(!wants("airplay:AABBCCDDEE02"));
        assert!(!wants("airplay:AABBCCDDEE03"));
        assert_eq!(state.find_by_id("airplay:AABBCCDDEE01").unwrap().raop.metadata, [0, 1, 2]);
    }

    #[test]
    fn only_a_newcomers_id_has_the_instance_form() {
        let state = conflict_state();
        assert!(is_instance_id(NEWCOMER));
        assert!(is_instance_id(&state.find_by_id(NEWCOMER).unwrap().stable_id()));
        for id in ["airplay:B8E93735AC11", "airplay:pair:3F2A/x", "airplay:manual-192.168.1.20:7000", "airplay:/x", "airplay:B8E93735AC11/", "uuid:RINCON_1/x"] {
            assert!(!is_instance_id(id), "{id}");
        }
    }

    #[test]
    fn an_empty_tp_counts_as_none() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE11@E", "192.168.1.5", &[("tp", ""), ("et", "0"), ("cn", "1")]));
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEE12@F", "192.168.1.6", &[("tp", " , "), ("et", "0"), ("cn", "1")]));
        for id in ["airplay:AABBCCDDEE11", "airplay:AABBCCDDEE12"] {
            let r = state.find_by_id(id).unwrap();
            assert_eq!(r.raop.transports, None);
            assert!(r.supports_legacy_raop());
        }
    }

    #[test]
    fn tp_is_read_from_the_txt_record() {
        let state = AirPlayDiscoveryState::with_lan(home_lan());
        ingest_resolution(&state, RAOP_SERVICE, &raop_info("AABBCCDDEEFF@T", "192.168.1.5", &[("tp", "TCP"), ("et", "0")]));
        let r = state.find_by_id("airplay:AABBCCDDEEFF").unwrap();
        assert_eq!(r.raop.transports, Some(vec!["TCP".to_string()]));
        assert!(r.codecs.is_empty());
        assert!(!r.supports_legacy_raop());
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
