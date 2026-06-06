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
    /// 64-bit AirPlay-2 `features`/`ft` bitfield, if the device advertised
    /// `_airplay._tcp`.
    pub features: Option<u64>,
    /// Device HomeKit public key (`pk`, hex) from `_airplay._tcp`.
    pub pk: Option<String>,
    /// Model string (`am=` on RAOP or `model` on AirPlay), e.g.
    /// `AudioAccessory5,1` for a HomePod.
    pub model: Option<String>,
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
        if self.password_protected || self.port == 0 {
            return false;
        }
        if !self.codecs.contains(&1) {
            return false;
        }
        self.encryption_types.contains(&0) || self.encryption_types.contains(&1)
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

    /// True if the device expects IEEE-1588 PTP timing (feature bit 41).
    /// HomePods set this; many older AP2 devices accept NTP instead.
    pub fn expects_ptp(&self) -> bool {
        self.features.map(|f| f & FEAT_PTP != 0).unwrap_or(false)
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
        features: airplay.and_then(|a| a.features),
        pk: airplay.and_then(|a| a.pk.clone()),
        model: airplay
            .and_then(|a| a.model.clone())
            .or_else(|| raop.and_then(|r| r.model.clone())),
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
        };
        debug!(
            "AirPlay v2 resolved: {} @ {:?}:{} ft={:?} model={:?}",
            a.friendly_name, a.ip, a.port, a.features, a.model
        );
        state.upsert_airplay(mac, a);
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
            features: ft,
            pk: None,
            model: model.map(|s| s.to_string()),
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
}
