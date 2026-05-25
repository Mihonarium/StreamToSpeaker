//! mDNS-based discovery of `_raop._tcp.local.` services.
//!
//! Modelled on `ssdp.rs` so the GUI can treat AirPlay and UPnP
//! discoveries symmetrically: both produce a shared list of speakers,
//! both expose `find_by_id` / `replace`, both run a background thread
//! that keeps the list fresh.
//!
//! ## RAOP TXT record fields
//!
//! Per the unofficial AirPlay specification, the keys we care about are:
//!
//! | Key | Meaning                                              |
//! |-----|------------------------------------------------------|
//! | `txtvers` | TXT record version (always 1)                  |
//! | `ch`      | Channel count (we want 2)                      |
//! | `sr`      | Sample rate (we want 44100)                    |
//! | `ss`      | Sample size in bits (we want 16)               |
//! | `tp`      | Transport: `UDP` / `TCP`                       |
//! | `cn`      | Comma-separated codecs: 0=PCM 1=ALAC 2=AAC ... |
//! | `et`      | Comma-separated encryption types: 0=none 1=RSA |
//! | `vn`      | RSA version (3 = the published Apple key)      |
//! | `md`      | Metadata types supported (we don't send any)   |
//! | `pw`      | Password protected (we don't support this yet) |
//! | `am`      | Apple model — purely informational             |
//!
//! The mDNS *service name* is conventionally `<MAC>@<FriendlyName>`,
//! e.g. `B8E93735AC11@Living Room`. We split on `@` to get the user-
//! facing name; the MAC is the receiver's stable identifier and is what
//! we use for our `stable_id`.

use anyhow::Result;
use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent, TxtProperties};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Service type for AirPlay 1 audio receivers.
pub const RAOP_SERVICE: &str = "_raop._tcp.local.";

/// One resolved AirPlay receiver.
#[derive(Debug, Clone)]
pub struct AirPlayRenderer {
    /// Friendly name (the part after `@` in the mDNS service name).
    pub friendly_name: String,
    /// Stable ID — the MAC address portion of the service name. We use
    /// the original case-folded form so the ID survives device renames.
    pub mac_id: String,
    /// IPv4 of the receiver (we prefer v4 over v6 for socket simplicity).
    pub ip: IpAddr,
    /// RTSP control port (defaults to 5000 historically, but discovery
    /// is authoritative).
    pub port: u16,
    /// Encryption types the receiver advertises (from `et=`).
    pub encryption_types: Vec<u8>,
    /// Codec ids the receiver advertises (from `cn=`).
    pub codecs: Vec<u8>,
    /// Whether `pw=true` was set. We refuse to connect to password-
    /// protected receivers — implementing RAOP password auth is a
    /// secondary priority.
    pub password_protected: bool,
    /// Raw `am=` model string, surfaced in logs / tooltips.
    pub model: Option<String>,
}

impl AirPlayRenderer {
    /// Stable identifier used by the UI / persistence layer. Prefixed
    /// so it can't collide with the UPnP `Renderer::stable_id()` (which
    /// is either a UDN like `uuid:RINCON-…` or an IP literal). The MAC
    /// is the right anchor — IP can change with DHCP, name with user
    /// renames.
    pub fn stable_id(&self) -> String {
        format!("airplay:{}", self.mac_id)
    }

    /// True if this receiver supports a path we know how to talk to.
    ///
    /// We require:
    ///   * Not password-protected (we don't speak HTTP-Digest yet).
    ///   * ALAC (`cn=1`) advertised — universal across AirPlay
    ///     receivers, but a handful of weird devices only do PCM.
    ///   * Either no-encryption (`et=0`) OR Apple RSA (`et=1`)
    ///     advertised. FairPlay-only receivers (`et=3/4/5` exclusive,
    ///     no `et=0`) require HomeKit pairing and are out of scope.
    pub fn is_supported(&self) -> bool {
        if self.password_protected {
            return false;
        }
        if !self.codecs.contains(&1) {
            return false;
        }
        self.encryption_types.contains(&0) || self.encryption_types.contains(&1)
    }
}

/// Shared discovery state. Mirrors `ssdp::DiscoveryState` semantics.
#[derive(Default)]
pub struct AirPlayDiscoveryState {
    inner: Mutex<HashMap<String, AirPlayRenderer>>,
}

impl AirPlayDiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot of currently-known receivers, sorted by friendly name.
    pub fn renderers(&self) -> Vec<AirPlayRenderer> {
        let map = self.inner.lock().unwrap();
        let mut v: Vec<AirPlayRenderer> = map.values().cloned().collect();
        v.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        v
    }

    /// Replace the entire renderer set. Used by the discovery thread
    /// when an mDNS browse completes a full round.
    pub fn replace_all(&self, renderers: Vec<AirPlayRenderer>) {
        let mut map = self.inner.lock().unwrap();
        map.clear();
        for r in renderers {
            map.insert(r.mac_id.clone(), r);
        }
    }

    /// Add or update one renderer (called when mDNS resolves a single
    /// service in the streaming-style event loop).
    pub fn upsert(&self, renderer: AirPlayRenderer) {
        let mut map = self.inner.lock().unwrap();
        map.insert(renderer.mac_id.clone(), renderer);
    }

    /// Remove by MAC id (mDNS `ServiceRemoved` event).
    pub fn remove(&self, mac_id: &str) {
        let mut map = self.inner.lock().unwrap();
        map.remove(mac_id);
    }

    /// Find by our public `stable_id()` (i.e. with the `airplay:` prefix).
    pub fn find_by_id(&self, id: &str) -> Option<AirPlayRenderer> {
        let mac = id.strip_prefix("airplay:")?;
        self.inner.lock().unwrap().get(mac).cloned()
    }
}

/// Spawn the long-running mDNS browser. The daemon thread is owned by
/// the `mdns-sd` crate; we keep a single event-consumer thread that
/// upserts records into `state` as they resolve.
///
/// `iface_hint` is informational only — `mdns-sd` listens on all
/// interfaces by default, and the daemon picks the right one for each
/// outgoing query based on the receiver's source address. We log the
/// hint for parity with the SSDP path but don't bind to it.
pub fn spawn_airplay_discovery(
    state: Arc<AirPlayDiscoveryState>,
    iface_hint: Option<Ipv4Addr>,
) -> Result<()> {
    let daemon =
        ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon init: {}", e))?;
    let receiver = daemon
        .browse(RAOP_SERVICE)
        .map_err(|e| anyhow::anyhow!("mdns browse {}: {}", RAOP_SERVICE, e))?;

    info!(
        "AirPlay (RAOP) discovery: browsing {} (iface hint {:?})",
        RAOP_SERVICE, iface_hint,
    );

    thread::Builder::new()
        .name("stream-to-speaker-airplay-mdns".to_string())
        .spawn(move || {
            // Hold on to the daemon so it isn't dropped (which would
            // tear down the background thread it owns).
            let _daemon_keepalive = daemon;
            loop {
                match receiver.recv_timeout(Duration::from_secs(60)) {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        match build_renderer_from_resolution(&info) {
                            Some(r) => {
                                debug!(
                                    "AirPlay resolved: {} @ {}:{} (mac={}, supported={})",
                                    r.friendly_name,
                                    r.ip,
                                    r.port,
                                    r.mac_id,
                                    r.is_supported(),
                                );
                                state.upsert(r);
                            }
                            None => {
                                debug!(
                                    "AirPlay resolved {} but couldn't build renderer (no IPv4?)",
                                    info.get_fullname()
                                );
                            }
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                        if let Some(mac) = mac_from_fullname(&fullname) {
                            debug!("AirPlay removed: {}", fullname);
                            state.remove(&mac);
                        }
                    }
                    Ok(_) => { /* SearchStarted / ServiceFound / etc. */ }
                    Err(flume::RecvTimeoutError::Timeout) => {
                        // No traffic in a minute; loop and keep listening.
                    }
                    Err(flume::RecvTimeoutError::Disconnected) => {
                        warn!("AirPlay mDNS daemon disconnected; discovery stops");
                        return;
                    }
                }
            }
        })?;

    Ok(())
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Build an [`AirPlayRenderer`] from a resolved mDNS service. Returns
/// None if the service has no usable IPv4 address.
fn build_renderer_from_resolution(info: &mdns_sd::ServiceInfo) -> Option<AirPlayRenderer> {
    let fullname = info.get_fullname();

    // Service name layout: "<MAC>@<FriendlyName>._raop._tcp.local."
    let (mac_id, friendly_name) = split_service_name(fullname)?;

    let v4_addrs: Vec<IpAddr> = info
        .get_addresses()
        .iter()
        .filter_map(|a| match a {
            IpAddr::V4(v4) => Some(IpAddr::V4(*v4)),
            _ => None,
        })
        .collect();
    let ip = match v4_addrs.first().copied() {
        Some(v) => v,
        None => return None,
    };

    let port = info.get_port();
    let txt = info.get_properties();

    let password_protected = read_txt_bool(txt, "pw");
    let encryption_types = read_txt_int_list(txt, "et");
    let codecs = read_txt_int_list(txt, "cn");
    let model = read_txt_string(txt, "am");

    Some(AirPlayRenderer {
        friendly_name,
        mac_id,
        ip,
        port,
        encryption_types,
        codecs,
        password_protected,
        model,
    })
}

fn split_service_name(fullname: &str) -> Option<(String, String)> {
    // Strip the service-type suffix.
    let trimmed = fullname
        .strip_suffix(&format!(".{}", RAOP_SERVICE))
        .or_else(|| fullname.strip_suffix(RAOP_SERVICE))
        .unwrap_or(fullname);
    let (mac, friendly) = trimmed.split_once('@')?;
    let mac = mac.trim().to_string();
    let friendly = friendly.trim().to_string();
    if mac.is_empty() || friendly.is_empty() {
        return None;
    }
    Some((mac, friendly))
}

fn mac_from_fullname(fullname: &str) -> Option<String> {
    Some(split_service_name(fullname)?.0)
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
