//! SSDP (Simple Service Discovery Protocol) discovery for Sonos / OpenHome
//! renderers.
//!
//! Sends M-SEARCH multicasts to 239.255.255.250:1900 with the relevant
//! Search Target headers, then fetches the device description XML from the
//! LOCATION URL each responder advertises. The XML parser walks the
//! `<service>` list and extracts the control / event sub-URLs we need.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Resolved details for one renderer.
#[derive(Debug, Clone)]
pub struct Renderer {
    /// Friendly name from the device description.
    pub friendly_name: String,
    /// IP we use for control plane.
    pub ip: IpAddr,
    /// Base URL (scheme://host:port) for resolving relative control URLs.
    pub base_url: String,
    /// SetAVTransportURI / Play / Stop endpoint.
    pub av_transport_control_url: String,
    /// SetVolume / SetMute / GetVolume endpoint.
    pub rendering_control_control_url: String,
    /// Event subscription URL for RenderingControl.
    pub rendering_control_event_url: String,
    /// Event subscription URL for AVTransport (handy for transport state).
    pub av_transport_event_url: Option<String>,
    /// Cached UDN if available.
    pub udn: Option<String>,
    /// Sonos only: ZoneGroupTopology control URL (GetZoneGroupState).
    /// Present ⇒ the device participates in Sonos zone grouping.
    pub zone_group_topology_control_url: Option<String>,
    /// Sonos only: GroupRenderingControl control URL — group-wide volume
    /// on the coordinator (SetGroupVolume scales every member).
    pub group_rendering_control_control_url: Option<String>,
    /// Sonos only: GroupRenderingControl event URL (GroupVolume NOTIFYs).
    pub group_rendering_control_event_url: Option<String>,
    /// Room name from zone topology ("Living Room") — cleaner than the
    /// device-description friendlyName for group display. Sonos only.
    pub zone_name: Option<String>,
    /// Zone names of the OTHER visible members of the group this
    /// renderer coordinates. Empty ⇒ standalone (or non-Sonos). Filled
    /// by `sonos::apply_topology`.
    pub group_members: Vec<String>,
    /// Sonos only: addresses of the OTHER members of the group this
    /// renderer coordinates (from their topology `Location` hosts), so
    /// privacy mode admits the whole group — any member may end up
    /// fetching the stream. Filled by `sonos::resolve_group_coordinator`.
    pub group_member_addrs: Vec<IpAddr>,
    /// Every address the LOCATION host resolves to (just `ip` when the
    /// LOCATION carries a literal address). Privacy mode admits all of
    /// them, so a hostname resolving AAAA-first can't get the speaker
    /// refused.
    pub host_addrs: Vec<IpAddr>,
    /// `modelName` from the device description.
    pub model_name: Option<String>,
    /// Several different devices claim this renderer's UDN, so it is
    /// identified by its address instead (see `stable_id`).
    pub udn_conflict: bool,
}

/// Shared discovery state.  The main loop owns one; the SSDP thread
/// updates it.
#[derive(Default)]
pub struct DiscoveryState {
    inner: Mutex<Vec<Renderer>>,
    identity: Mutex<IdentityState>,
}

/// Which addresses each UDN is trusted at. A UDN is self-reported, so
/// two devices can claim the same one; see [`resolve_udn_conflicts`].
#[derive(Default)]
struct IdentityState {
    /// UDN → every address it has been accepted at as one device (more
    /// than one for a multi-homed device). Grows while the same device
    /// keeps answering at more addresses, and is replaced when the device
    /// is seen only at new addresses (a DHCP move).
    pins: HashMap<String, Vec<IpAddr>>,
    /// UDNs whose control address went unanswered in the last scan while
    /// another of its pinned addresses answered; the control address is
    /// kept for one such scan before moving.
    control_missed: HashMap<String, u32>,
    /// UDN → addresses of different devices currently claiming it. The
    /// UDN itself selects nothing; each claimant is listed under its
    /// address instead.
    conflicted: HashMap<String, Vec<IpAddr>>,
}

/// Path part of a URL, for comparing the same device seen at two hosts.
fn url_path(u: &str) -> &str {
    let rest = u.split_once("://").map(|(_, r)| r).unwrap_or(u);
    rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
}

/// Do two descriptions with the same UDN describe one device reachable at
/// several addresses (same name, model and control paths)?
fn same_device(a: &Renderer, b: &Renderer) -> bool {
    a.friendly_name == b.friendly_name
        && a.model_name == b.model_name
        && url_path(&a.av_transport_control_url) == url_path(&b.av_transport_control_url)
        && url_path(&a.rendering_control_control_url) == url_path(&b.rendering_control_control_url)
        && url_path(&a.rendering_control_event_url) == url_path(&b.rendering_control_event_url)
}

/// One entry for a device: `control` is the entry used to control it,
/// and every address in `addrs` is admitted for streaming.
fn with_addrs(mut control: Renderer, addrs: &[IpAddr]) -> Renderer {
    control.host_addrs.extend(addrs.iter().copied());
    control.host_addrs.push(control.ip);
    control.host_addrs.sort();
    control.host_addrs.dedup();
    control
}

/// Settle UDN collisions in one discovery pass. `prev` is the list the
/// previous pass produced.
///
/// * Claimants at pinned addresses are kept, and so is any other claimant
///   with the same description as a kept one (the device answering at one
///   more address); its address joins the pin set. Claimants that differ
///   are dropped while a pinned one is present.
/// * With no pinned claimant: a single address is accepted and becomes the
///   pin set (a DHCP move); several addresses with the same description
///   are one multi-homed device (Ethernet and Wi-Fi, or seen through
///   several adapters) and are pinned together.
/// * Claimants that remain but describe different devices — strangers
///   with no pin, or pinned addresses whose descriptions have diverged —
///   are each listed marked `udn_conflict`, selected by address, and the
///   UDN is reported as conflicted.
///
/// A device is one entry controlled through one address — the previous
/// control address while it answers, kept through one scan that misses it
/// — and admitted for streaming at every pinned address, so a lost SSDP
/// reply neither moves control nor shuts an interface out.
///
/// The cost of trusting matching descriptions: a device that copies a
/// speaker's whole description is accepted as part of it. Identity is
/// self-reported, so nothing here can prove which claimant is genuine;
/// what is refused is a different device taking over a known identity.
fn resolve_udn_conflicts(
    list: Vec<Renderer>,
    ident: &mut IdentityState,
    prev: &[Renderer],
) -> (Vec<Renderer>, HashMap<String, Vec<IpAddr>>) {
    let mut out = Vec::with_capacity(list.len());
    let mut by_udn: HashMap<String, Vec<Renderer>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for r in list {
        match r.udn.clone() {
            Some(udn) => {
                if !by_udn.contains_key(&udn) {
                    order.push(udn.clone());
                }
                let v = by_udn.entry(udn).or_default();
                // One entry per address.
                if !v.iter().any(|e| e.ip == r.ip) {
                    v.push(r);
                }
            }
            None => out.push(r),
        }
    }
    let mut conflicted = HashMap::new();
    for udn in order {
        let mut entries = by_udn.remove(&udn).unwrap_or_default();
        entries.sort_by_key(|r| r.ip);
        let pins = ident.pins.get(&udn).cloned().unwrap_or_default();

        let accepted: Vec<Renderer> = if entries.iter().any(|r| pins.contains(&r.ip)) {
            let (pinned, others): (Vec<Renderer>, Vec<Renderer>) =
                entries.into_iter().partition(|r| pins.contains(&r.ip));
            let (rejoin, strangers): (Vec<Renderer>, Vec<Renderer>) =
                others.into_iter().partition(|r| same_device(r, &pinned[0]));
            if !strangers.is_empty() {
                warn!(
                    "devices at {:?} claim the identity {} already seen at {:?}; ignoring them",
                    strangers.iter().map(|r| r.ip).collect::<Vec<_>>(),
                    udn,
                    pins
                );
            }
            pinned.into_iter().chain(rejoin).collect()
        } else {
            entries
        };
        let addrs: Vec<IpAddr> = accepted.iter().map(|r| r.ip).collect();

        if !accepted.windows(2).all(|w| same_device(&w[0], &w[1])) {
            warn!(
                "different devices at {:?} all claim the identity {}; each is listed under its address",
                addrs, udn
            );
            out.extend(accepted.into_iter().map(|mut r| {
                r.udn_conflict = true;
                r
            }));
            conflicted.insert(udn, addrs);
            continue;
        }

        let known = addrs.iter().any(|a| pins.contains(a));
        let mut pin_set = if known { pins.clone() } else { Vec::new() };
        pin_set.extend(addrs.iter().copied());
        pin_set.sort();
        pin_set.dedup();
        if addrs.len() > 1 {
            debug!("{} answers at {:?}; treating it as one multi-homed device", udn, addrs);
        }

        let prev_control = prev
            .iter()
            .find(|r| !r.udn_conflict && r.udn.as_deref() == Some(udn.as_str()));
        let missed = ident.control_missed.remove(&udn).unwrap_or(0);
        let entry = match prev_control {
            Some(p) if addrs.contains(&p.ip) => {
                let i = addrs.iter().position(|a| *a == p.ip).unwrap_or(0);
                with_addrs(accepted.into_iter().nth(i).expect("index of an accepted address"), &pin_set)
            }
            Some(p) if known && missed == 0 && pin_set.contains(&p.ip) => {
                ident.control_missed.insert(udn.clone(), 1);
                with_addrs(p.clone(), &pin_set)
            }
            _ => with_addrs(accepted.into_iter().next().expect("at least one claimant"), &pin_set),
        };
        ident.pins.insert(udn, pin_set);
        out.push(entry);
    }
    out.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
    (out, conflicted)
}

impl DiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn renderers(&self) -> Vec<Renderer> {
        self.inner.lock().unwrap().clone()
    }

    /// Set the renderer list, deduping by control URL.
    /// Duplicate UDNs from different hosts are settled by
    /// [`resolve_udn_conflicts`].
    pub fn replace(&self, mut new: Vec<Renderer>) {
        new.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        new.dedup_by(|a, b| a.av_transport_control_url == b.av_transport_control_url);
        let mut identity = self.identity.lock().unwrap();
        let prev = self.inner.lock().unwrap().clone();
        let (new, conflicted) = resolve_udn_conflicts(new, &mut identity, &prev);
        identity.conflicted = conflicted;
        *self.inner.lock().unwrap() = new;
    }

    /// Addresses of the different devices currently claiming the UDN `id`
    /// when none of them was seen with it before.
    pub fn identity_conflict(&self, id: &str) -> Option<Vec<IpAddr>> {
        self.identity.lock().unwrap().conflicted.get(id).cloned()
    }

    /// Look up a renderer by friendly name (substring, case-insensitive)
    /// or IP literal.
    pub fn find(&self, query: &str) -> Option<Renderer> {
        let q = query.to_ascii_lowercase();
        let inner = self.inner.lock().unwrap();
        // IP literal match first.
        for r in inner.iter() {
            if r.ip.to_string() == query {
                return Some(r.clone());
            }
        }
        for r in inner.iter() {
            if r.friendly_name.to_ascii_lowercase().contains(&q) {
                return Some(r.clone());
            }
        }
        // Group-aware match: "--player Kitchen" should find the group
        // entry that contains Kitchen even though the row is named after
        // its coordinator.
        for r in inner.iter() {
            let zone_hit = r
                .zone_name
                .as_deref()
                .map(|z| z.to_ascii_lowercase().contains(&q))
                .unwrap_or(false);
            let member_hit = r
                .group_members
                .iter()
                .any(|m| m.to_ascii_lowercase().contains(&q));
            if zone_hit || member_hit {
                return Some(r.clone());
            }
        }
        None
    }

    /// Forget every renderer (discovery moved to another network, or
    /// paused because its adapter went away).
    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }

    pub fn first(&self) -> Option<Renderer> {
        self.inner.lock().unwrap().first().cloned()
    }

    /// Match by stable id (UDN if available, otherwise IP literal).
    /// Used by the `/api/select` HTTP endpoint.
    /// A UDN claimed by several different devices (see
    /// `identity_conflict`) matches nothing; those devices are selected by
    /// address. UDN matches take precedence over IP literals, so a device
    /// can't shadow another by advertising its address as a UDN.
    pub fn find_by_id(&self, id: &str) -> Option<Renderer> {
        if let Some(addrs) = self.identity_conflict(id) {
            warn!("not selecting {}: claimed by several devices ({:?})", id, addrs);
            return None;
        }
        let inner = self.inner.lock().unwrap();
        if let Some(r) = inner.iter().find(|r| !r.udn_conflict && r.udn.as_deref() == Some(id)) {
            return Some(r.clone());
        }
        inner.iter().find(|r| r.ip.to_string() == id).cloned()
    }
}

impl Renderer {
    /// Stable identifier suitable for `find_by_id`. UDN if the device
    /// advertises one, IP literal as fallback.
    /// Every address this renderer is known to use, for the privacy
    /// gate: the control-plane IP and all addresses its LOCATION host
    /// resolves to.
    pub fn stream_peers(&self) -> Vec<IpAddr> {
        let mut v = vec![self.ip];
        v.extend(self.host_addrs.iter().copied());
        v.extend(self.group_member_addrs.iter().copied());
        v.sort();
        v.dedup();
        v
    }

    pub fn stable_id(&self) -> String {
        match &self.udn {
            Some(udn) if !self.udn_conflict => udn.clone(),
            _ => self.ip.to_string(),
        }
    }

    /// True when this renderer coordinates a Sonos group with at least
    /// one other audible speaker.
    pub fn is_group(&self) -> bool {
        !self.group_members.is_empty()
    }

    /// Name to show the user. Standalone speakers keep their device
    /// friendlyName; group coordinators use the Sonos convention —
    /// "Living Room + Kitchen" for a pair, "Living Room + 2" beyond.
    pub fn display_name(&self) -> String {
        let name = if !self.is_group() {
            self.friendly_name.clone()
        } else {
            let base = self.zone_name.as_deref().unwrap_or(&self.friendly_name);
            if self.group_members.len() == 1 {
                format!("{} + {}", base, self.group_members[0])
            } else {
                format!("{} + {}", base, self.group_members.len())
            }
        };
        if self.udn_conflict {
            // Another device claims the same identity: tell them apart.
            format!("{} ({})", name, self.ip)
        } else {
            name
        }
    }

    /// Is `other` the same device as this one: the same UDN reachable at
    /// a shared address (or, without a UDN, the same address)? Robust to
    /// `stable_id` switching between UDN and address as identity
    /// conflicts appear and clear.
    pub fn same_device_as(&self, other: &Renderer) -> bool {
        let addrs = |r: &Renderer| {
            let mut v = r.host_addrs.clone();
            v.push(r.ip);
            v
        };
        let mine = addrs(self);
        let shares_addr = addrs(other).iter().any(|a| mine.contains(a));
        match (&self.udn, &other.udn) {
            (Some(a), Some(b)) => a == b && shares_addr,
            (None, None) => self.ip == other.ip,
            _ => false,
        }
    }

    /// GroupRenderingControl control URL, but only when volume should
    /// actually be group-routed (a real multi-speaker group). Standalone
    /// Sonos zones use plain RenderingControl — identical behavior to
    /// pre-group builds.
    pub fn group_volume_control_url(&self) -> Option<&str> {
        if self.is_group() {
            self.group_rendering_control_control_url.as_deref()
        } else {
            None
        }
    }

    /// The event URL our GENA volume-sync subscription should use:
    /// GroupRenderingControl for a group (GroupVolume events reflect the
    /// group slider), plain RenderingControl otherwise.
    pub fn volume_event_url(&self) -> &str {
        if self.is_group() {
            if let Some(u) = self.group_rendering_control_event_url.as_deref() {
                return u;
            }
        }
        &self.rendering_control_event_url
    }
}

/// Search Targets we hunt for. Sonos answers to both.
const SEARCH_TARGETS: &[&str] = &[
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "urn:av-openhome-org:service:Product:1",
];

const SSDP_MULTICAST: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

/// Where an SSDP search goes out from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    /// Every non-loopback IPv4 interface.
    All,
    /// One local IPv4 only (a pinned address or the chosen adapter's).
    Iface(Ipv4Addr),
    /// Don't search: the chosen adapter is unavailable, and searching
    /// elsewhere instead would defeat the choice.
    Paused,
}

impl SearchScope {
    /// The `iface` argument of [`discover_once`], or None when paused.
    pub fn iface(self) -> Option<Option<Ipv4Addr>> {
        match self {
            SearchScope::All => Some(None),
            SearchScope::Iface(ip) => Some(Some(ip)),
            SearchScope::Paused => None,
        }
    }
}

/// Shared "where to search" query, re-read before every sweep.
pub type ScopeFn = Arc<dyn Fn() -> SearchScope + Send + Sync>;

/// Spawn the SSDP discovery background thread.  Runs an initial discovery
/// immediately and then every `interval` afterwards, each sweep from
/// wherever `scope` says at that moment (see [`discover_once`]). A sweep
/// whose scope changed while it ran is discarded — its results belong to
/// the old network.
pub fn spawn_discovery(state: Arc<DiscoveryState>, interval: Duration, scope: ScopeFn) {
    thread::Builder::new()
        .name("stream-to-speaker-ssdp".to_string())
        .spawn(move || loop {
            let used = scope();
            match used.iface() {
                None => debug!("SSDP discovery paused (chosen adapter unavailable)"),
                Some(iface) => match discover_once(Duration::from_secs(3), iface) {
                    Ok(found) if scope() == used => {
                        info!("SSDP discovery: {} renderer(s) found", found.len());
                        state.replace(found);
                    }
                    Ok(_) => debug!("SSDP sweep discarded: discovery scope changed meanwhile"),
                    Err(e) => warn!("SSDP discovery failed: {}", e),
                },
            }
            thread::sleep(interval);
        })
        .expect("spawning SSDP thread");
}

/// One-shot discovery. Returns a deduped list of renderers.
/// `iface` pins the search to one local IPv4 (from `--advertise-ip` /
/// `--bind`); `None` searches from every non-loopback IPv4 interface,
/// so a speaker on a second NIC is found even when a VPN owns the
/// default route.
pub fn discover_once(timeout: Duration, iface: Option<Ipv4Addr>) -> Result<Vec<Renderer>> {
    // A pinned interface stays strict: failing to search from it is an
    // error, not a silent fallback to whatever the OS picks.
    if let Some(ip) = iface {
        return collect_renderers(vec![search_socket(ip)?], timeout);
    }
    let ifaces = local_ipv4_interfaces();
    let mut sockets: Vec<UdpSocket> = Vec::new();
    for ip in &ifaces {
        match search_socket(*ip) {
            Ok(s) => sockets.push(s),
            Err(e) => debug!("SSDP search on {} failed: {:#}", ip, e),
        }
    }
    if sockets.is_empty() {
        // No usable interface list (or every bind failed): let the OS pick.
        sockets.push(search_socket(Ipv4Addr::UNSPECIFIED)?);
    }
    collect_renderers(sockets, timeout)
}

/// Read M-SEARCH replies on `sockets` until `timeout`, then fetch and
/// parse each advertised device.
fn collect_renderers(sockets: Vec<UdpSocket>, timeout: Duration) -> Result<Vec<Renderer>> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    let mut locations: Vec<String> = Vec::new();

    while Instant::now() < deadline {
        for udp in &sockets {
            match udp.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    if let Some(loc) = extract_header(&text, "LOCATION") {
                        if !locations.contains(&loc) {
                            debug!("SSDP response LOCATION={} from {}", loc, peer.ip());
                            locations.push(loc);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => debug!("SSDP recv error: {}", e),
            }
        }
    }

    let mut renderers = Vec::new();
    for loc in &locations {
        match fetch_and_parse_device(loc, Duration::from_secs(3)) {
            Ok(r) => renderers.push(r),
            Err(e) => debug!("device fetch {} failed: {}", loc, e),
        }
    }

    // Sonos: fold zone-group topology in — hides bonded/grouped members,
    // annotates group coordinators. No-op without Sonos devices.
    crate::sonos::annotate_with_topology(&mut renderers);

    Ok(renderers)
}

/// Non-loopback, non-link-local IPv4 addresses of this machine (from
/// every adapter; one without a link just fails its send and is skipped).
pub fn local_ipv4_interfaces() -> Vec<Ipv4Addr> {
    let mut v: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .map(|list| {
            list.into_iter()
                .filter_map(|i| match i.ip() {
                    IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() => {
                        Some(ip)
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

/// A UDP socket bound to `ip` that has sent our M-SEARCHes out of that
/// interface. Binding to the address (not just choosing the multicast
/// interface) makes Windows send *and* receive the unicast replies on
/// that interface.
fn search_socket(ip: Ipv4Addr) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    sock.bind(&SocketAddr::new(IpAddr::V4(ip), 0).into())?;
    sock.set_multicast_ttl_v4(2)?;
    if !ip.is_unspecified() {
        sock.set_multicast_if_v4(&ip)?;
    }
    debug!("SSDP searching from {}", ip);
    let udp: UdpSocket = sock.into();
    for st in SEARCH_TARGETS {
        let req = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 2\r\n\
             ST: {}\r\n\
             USER-AGENT: stream-to-speaker/0.1\r\n\
             \r\n",
            st
        );
        udp.send_to(req.as_bytes(), SSDP_MULTICAST)
            .with_context(|| format!("sending SSDP M-SEARCH from {}", ip))?;
    }
    Ok(udp)
}

fn extract_header(response: &str, header: &str) -> Option<String> {
    let key = format!("{}:", header.to_ascii_uppercase());
    for line in response.lines() {
        let upper: String = line
            .chars()
            .take(key.len())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if upper == key {
            let rest = &line[key.len()..];
            return Some(rest.trim().to_string());
        }
    }
    None
}

pub(crate) fn fetch_and_parse_device(location: &str, timeout: Duration) -> Result<Renderer> {
    let xml = http_get(location, timeout).with_context(|| format!("GET {}", location))?;
    let url = url::Url::parse(location).with_context(|| format!("parse location {}", location))?;
    let base = format!(
        "{}://{}",
        url.scheme(),
        url.host_str()
            .map(|h| {
                if let Some(p) = url.port() {
                    format!("{}:{}", h, p)
                } else {
                    h.to_string()
                }
            })
            .unwrap_or_default()
    );
    let (ip, host_addrs): (IpAddr, Vec<IpAddr>) = match url.host_str() {
        Some(h) => match h.parse::<IpAddr>() {
            Ok(ip) => (ip, vec![ip]),
            Err(_) => {
                use std::net::ToSocketAddrs;
                let port = url.port().unwrap_or(80);
                // Keep every address, not just the first: the speaker may
                // fetch the stream from any of them.
                let addrs: Vec<IpAddr> = (h, port)
                    .to_socket_addrs()
                    .ok()
                    .map(|it| it.map(|sa| sa.ip()).collect())
                    .unwrap_or_default();
                let first = *addrs
                    .first()
                    .ok_or_else(|| anyhow!("could not resolve host {:?}", h))?;
                (first, addrs)
            }
        },
        None => return Err(anyhow!("no host in location {}", location)),
    };

    let mut renderer = parse_device_description(&xml, &base, ip)?;
    renderer.host_addrs = host_addrs;
    Ok(renderer)
}

fn parse_device_description(xml: &str, base_url: &str, ip: IpAddr) -> Result<Renderer> {
    let doc = roxmltree::Document::parse(xml.trim_start()).map_err(|e| {
        // Show the first chunk of XML so we can see what failed.
        let head: String = xml.chars().take(120).collect();
        anyhow!("parsing device XML: {} — first 120 chars: {:?}", e, head)
    })?;

    let root = doc.root_element();
    let device = root
        .descendants()
        .find(|n| n.tag_name().name() == "device")
        .ok_or_else(|| anyhow!("no <device> element"))?;

    let friendly_name = device
        .descendants()
        .find(|n| n.tag_name().name() == "friendlyName")
        .and_then(|n| n.text())
        .unwrap_or("Unknown")
        .to_string();

    let model_name = device
        .descendants()
        .find(|n| n.tag_name().name() == "modelName")
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string());

    let udn = device
        .descendants()
        .find(|n| n.tag_name().name() == "UDN")
        .and_then(|n| n.text())
        .map(|s| s.to_string());

    // Walk services. Sonos lists AVTransport and RenderingControl as
    // siblings under <serviceList>; on some renderers the MediaRenderer
    // device is nested inside the root device.
    let mut av_ctrl: Option<String> = None;
    let mut av_event: Option<String> = None;
    let mut rc_ctrl: Option<String> = None;
    let mut rc_event: Option<String> = None;
    let mut zgt_ctrl: Option<String> = None;
    let mut grc_ctrl: Option<String> = None;
    let mut grc_event: Option<String> = None;

    for svc in device.descendants().filter(|n| n.tag_name().name() == "service") {
        let st = svc
            .children()
            .find(|n| n.tag_name().name() == "serviceType")
            .and_then(|n| n.text())
            .unwrap_or("");
        let control = svc
            .children()
            .find(|n| n.tag_name().name() == "controlURL")
            .and_then(|n| n.text());
        let event = svc
            .children()
            .find(|n| n.tag_name().name() == "eventSubURL")
            .and_then(|n| n.text());
        if st.contains(":AVTransport:") {
            av_ctrl = control.map(|s| s.to_string());
            av_event = event.map(|s| s.to_string());
        } else if st.contains(":RenderingControl:") {
            rc_ctrl = control.map(|s| s.to_string());
            rc_event = event.map(|s| s.to_string());
        } else if st.contains(":ZoneGroupTopology:") {
            // Sonos zone grouping (root device on Sonos hardware).
            zgt_ctrl = control.map(|s| s.to_string());
        } else if st.contains(":GroupRenderingControl:") {
            // Sonos group-wide volume (MediaRenderer sub-device).
            grc_ctrl = control.map(|s| s.to_string());
            grc_event = event.map(|s| s.to_string());
        }
    }

    let av_ctrl = av_ctrl.ok_or_else(|| anyhow!("AVTransport controlURL missing in {}", friendly_name))?;
    let rc_ctrl = rc_ctrl.ok_or_else(|| anyhow!("RenderingControl controlURL missing in {}", friendly_name))?;
    let rc_event = rc_event.ok_or_else(|| anyhow!("RenderingControl eventSubURL missing in {}", friendly_name))?;

    Ok(Renderer {
        friendly_name,
        ip,
        base_url: base_url.to_string(),
        av_transport_control_url: absolute_url(base_url, &av_ctrl),
        rendering_control_control_url: absolute_url(base_url, &rc_ctrl),
        rendering_control_event_url: absolute_url(base_url, &rc_event),
        av_transport_event_url: av_event.map(|e| absolute_url(base_url, &e)),
        udn,
        zone_group_topology_control_url: zgt_ctrl.map(|u| absolute_url(base_url, &u)),
        group_rendering_control_control_url: grc_ctrl.map(|u| absolute_url(base_url, &u)),
        group_rendering_control_event_url: grc_event.map(|u| absolute_url(base_url, &u)),
        zone_name: None,
        group_members: Vec::new(),
        group_member_addrs: Vec::new(),
        host_addrs: vec![ip],
        model_name,
        udn_conflict: false,
    })
}

fn absolute_url(base: &str, maybe_relative: &str) -> String {
    if maybe_relative.starts_with("http://") || maybe_relative.starts_with("https://") {
        maybe_relative.to_string()
    } else if maybe_relative.starts_with('/') {
        format!("{}{}", base.trim_end_matches('/'), maybe_relative)
    } else {
        format!("{}/{}", base.trim_end_matches('/'), maybe_relative)
    }
}

/// Largest response [`http_get`] accepts. Device descriptions are a few
/// KB; anything near this is not one (a stream URL, say).
const MAX_HTTP_GET_BYTES: usize = 256 * 1024;

/// Tiny blocking HTTP GET. We don't have hyper/reqwest; this is fine for
/// the few KB of XML SSDP responders return.
pub fn http_get(url_str: &str, timeout: Duration) -> Result<String> {
    let url = url::Url::parse(url_str)?;
    if url.scheme() != "http" {
        // Sonos device descriptions are HTTP, not HTTPS.
        return Err(anyhow!("only http:// supported, got {}", url.scheme()));
    }
    let host = url.host_str().ok_or_else(|| anyhow!("no host"))?;
    let port = url.port().unwrap_or(80);
    let mut path = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
    if let Some(q) = url.query() {
        path.push('?');
        path.push_str(q);
    }
    // Whole-request deadline: per-read timeouts alone let a server that
    // trickles bytes hold us indefinitely.
    let deadline = Instant::now() + timeout * 2;

    let mut stream = TcpStream::connect_timeout(
        &(host, port).to_socket_addrs_first()?,
        timeout,
    )?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // HTTP/1.0 explicitly — avoids the chunked Transfer-Encoding Sonos
    // uses for HTTP/1.1 responses, which our minimal client doesn't
    // parse. With HTTP/1.0 + Connection: close the server sends a
    // plain Content-Length body and closes the socket.
    let req = format!(
        "GET {} HTTP/1.0\r\nHost: {}:{}\r\nUser-Agent: stream-to-speaker/0.1\r\nConnection: close\r\nAccept: text/xml, application/xml, */*\r\n\r\n",
        path, host, port
    );
    stream.write_all(req.as_bytes())?;

    let mut buf = Vec::with_capacity(16384);
    let mut chunk = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(anyhow!("GET {} took too long", url_str));
        }
        stream.set_read_timeout(Some(left.min(timeout)))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if buf.len() + n > MAX_HTTP_GET_BYTES {
            return Err(anyhow!(
                "GET {}: response larger than {} KB",
                url_str,
                MAX_HTTP_GET_BYTES / 1024
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    // Find headers/body split. Robust to header lines being CRLF or LF only.
    let split_pos = find_header_body_split(&buf)
        .ok_or_else(|| anyhow!("response missing header/body separator ({} bytes)", buf.len()))?;
    let body_bytes = &buf[split_pos..];

    // Strip a UTF-8 BOM if Sonos sends one (it usually doesn't, but be
    // tolerant — roxmltree rejects BOMs).
    let body_bytes = body_bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body_bytes);

    // If chunked encoding was used despite our HTTP/1.0 request (some
    // proxies upgrade), peel the chunks. Otherwise this is a no-op.
    let body = decode_maybe_chunked(body_bytes);

    debug!("http_get {} → {} body bytes", url_str, body.len());
    if body.len() < 200 {
        debug!("  body: {:?}", String::from_utf8_lossy(&body));
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Overall time allowed for reading one HTTP response from a LAN device,
/// however slowly it trickles in. Each read is also bounded by the
/// socket's read timeout.
pub(crate) const RESPONSE_DEADLINE: Duration = Duration::from_secs(15);

/// Read until EOF, failing once more than `limit` bytes have arrived or
/// `deadline` has passed.
pub(crate) fn read_to_end_bounded<R: Read>(r: &mut R, limit: usize, deadline: Instant) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit.min(16384));
    let mut tmp = [0u8; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!("response not complete within {:?}", RESPONSE_DEADLINE));
        }
        let n = match r.read(&mut tmp) {
            Ok(0) => return Ok(buf),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if buf.len() + n > limit {
            return Err(anyhow!("response exceeds {} bytes", limit));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Find the byte offset where the HTTP body starts (after headers' blank line).
/// Returns None if no separator is found.
fn find_header_body_split(buf: &[u8]) -> Option<usize> {
    // Try \r\n\r\n first (RFC), then \n\n (some servers).
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(pos + 4);
    }
    if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some(pos + 2);
    }
    None
}

/// If the body starts with a hex chunk-size line (Transfer-Encoding:
/// chunked), decode the chunks. Otherwise returns the body unchanged.
/// Shared with upnp.rs's SOAP client — Sonos chunks its HTTP/1.1
/// responses, and a large GetZoneGroupState body straddles multiple
/// chunks whose size lines would otherwise corrupt the XML mid-payload.
pub(crate) fn decode_maybe_chunked(body: &[u8]) -> Vec<u8> {
    // Heuristic: a hex chunk-size line is short, ends with \r\n, and is
    // followed by binary data. If the first \r\n we see is preceded by
    // pure hex digits, treat as chunked.
    let first_crlf = match body.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return body.to_vec(),
    };
    if first_crlf == 0 || first_crlf > 8 {
        return body.to_vec();
    }
    let possibly_hex = &body[..first_crlf];
    if !possibly_hex.iter().all(|&b| b.is_ascii_hexdigit() || b == b';') {
        return body.to_vec();
    }

    // It's chunked. Decode.
    let mut out = Vec::with_capacity(body.len());
    let mut p = 0;
    while p < body.len() {
        let crlf = match body[p..].windows(2).position(|w| w == b"\r\n") {
            Some(i) => p + i,
            None => break,
        };
        let size_str = std::str::from_utf8(&body[p..crlf]).unwrap_or("0");
        let size_str = size_str.split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        p = crlf + 2;
        if size == 0 {
            break;
        }
        let Some(chunk_end) = p.checked_add(size) else {
            break;
        };
        if chunk_end > body.len() {
            break;
        }
        out.extend_from_slice(&body[p..chunk_end]);
        let Some(next_chunk) = chunk_end.checked_add(2) else {
            break;
        };
        if body.get(chunk_end..next_chunk) != Some(b"\r\n") {
            break;
        }
        p = next_chunk; // skip chunk + trailing \r\n
    }
    out
}

trait ToSocketAddrFirst {
    fn to_socket_addrs_first(&self) -> std::io::Result<SocketAddr>;
}

impl ToSocketAddrFirst for (&str, u16) {
    fn to_socket_addrs_first(&self) -> std::io::Result<SocketAddr> {
        use std::net::ToSocketAddrs;
        let mut addrs = (self.0, self.1).to_socket_addrs()?;
        addrs
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no addrs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, udn: &str, ip: &str) -> Renderer {
        let ip: IpAddr = ip.parse().unwrap();
        let base = format!("http://{}:1400", ip);
        Renderer {
            friendly_name: name.into(),
            ip,
            av_transport_control_url: format!("{}/AVTransport/Control", base),
            rendering_control_control_url: format!("{}/RenderingControl/Control", base),
            rendering_control_event_url: format!("{}/RenderingControl/Event", base),
            base_url: base,
            av_transport_event_url: None,
            udn: Some(udn.into()),
            zone_group_topology_control_url: None,
            group_rendering_control_control_url: None,
            group_rendering_control_event_url: None,
            zone_name: None,
            group_members: Vec::new(),
            group_member_addrs: Vec::new(),
            host_addrs: vec![ip],
            model_name: Some("Model".into()),
            udn_conflict: false,
        }
    }

    #[test]
    fn duplicate_udn_keeps_the_pinned_address() {
        let state = DiscoveryState::new();
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10")]);
        // An impostor that sorts first claims the same UDN.
        state.replace(vec![
            r("Living Room", "uuid:A", "192.168.1.10"),
            r("AAA", "uuid:A", "192.168.1.66"),
        ]);
        assert_eq!(state.renderers().len(), 1);
        let got = state.find_by_id("uuid:A").unwrap();
        assert_eq!(got.ip.to_string(), "192.168.1.10");
        assert!(state.identity_conflict("uuid:A").is_none());
    }

    #[test]
    fn duplicate_udn_without_history_is_not_selectable() {
        let state = DiscoveryState::new();
        state.replace(vec![
            r("Living Room", "uuid:A", "192.168.1.10"),
            r("AAA", "uuid:A", "192.168.1.66"),
            r("Kitchen", "uuid:B", "192.168.1.11"),
        ]);
        assert!(state.find_by_id("uuid:A").is_none());
        assert_eq!(state.identity_conflict("uuid:A").map(|v| v.len()), Some(2));
        assert!(state.find_by_id("uuid:B").is_some());
        // Once only one claimant answers, it is accepted.
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10")]);
        assert!(state.find_by_id("uuid:A").is_some());
    }

    #[test]
    fn multi_homed_device_is_merged_and_pinned() {
        let state = DiscoveryState::new();
        state.replace(vec![
            r("Living Room", "uuid:A", "192.168.1.10"),
            r("Living Room", "uuid:A", "10.0.0.10"),
        ]);
        let list = state.renderers();
        assert_eq!(list.len(), 1);
        assert!(state.identity_conflict("uuid:A").is_none());
        let got = state.find_by_id("uuid:A").unwrap();
        assert_eq!(got.ip.to_string(), "10.0.0.10");
        let peers = got.stream_peers();
        assert!(peers.contains(&"192.168.1.10".parse().unwrap()));
        assert!(peers.contains(&"10.0.0.10".parse().unwrap()));
        // Later, a different device claiming the UDN at a new address is
        // ignored.
        let mut other = r("Living Room", "uuid:A", "192.168.1.66");
        other.model_name = Some("Other".into());
        state.replace(vec![
            r("Living Room", "uuid:A", "192.168.1.10"),
            r("Living Room", "uuid:A", "10.0.0.10"),
            other,
        ]);
        let got = state.find_by_id("uuid:A").unwrap();
        assert!(!got.stream_peers().contains(&"192.168.1.66".parse().unwrap()));
        assert_eq!(state.renderers().len(), 1);
    }

    #[test]
    fn partial_scan_keeps_control_address_and_all_pinned_peers() {
        let state = DiscoveryState::new();
        let both = || vec![r("Living Room", "uuid:A", "192.168.1.10"), r("Living Room", "uuid:A", "10.0.0.10")];
        state.replace(both());
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "10.0.0.10");
        // One lost reply: control stays put, both addresses stay admitted.
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10")]);
        let got = state.find_by_id("uuid:A").unwrap();
        assert_eq!(got.ip.to_string(), "10.0.0.10");
        assert!(got.stream_peers().contains(&"192.168.1.10".parse().unwrap()));
        // Both answer again: nothing was dropped as an impostor.
        state.replace(both());
        assert_eq!(state.renderers().len(), 1);
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "10.0.0.10");
        // Control moves only after a second scan without it.
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10")]);
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "10.0.0.10");
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10")]);
        let got = state.find_by_id("uuid:A").unwrap();
        assert_eq!(got.ip.to_string(), "192.168.1.10");
        assert!(got.stream_peers().contains(&"10.0.0.10".parse().unwrap()));
        // And stays there when the other address returns.
        state.replace(both());
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "192.168.1.10");
    }

    #[test]
    fn pinned_addresses_that_diverge_are_flagged() {
        let state = DiscoveryState::new();
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10"), r("Living Room", "uuid:A", "10.0.0.10")]);
        let mut changed = r("Living Room", "uuid:A", "10.0.0.10");
        changed.model_name = Some("Other".into());
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10"), changed]);
        let list = state.renderers();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|x| x.udn_conflict));
        assert!(state.find_by_id("uuid:A").is_none());
        assert!(state.identity_conflict("uuid:A").is_some());
    }

    #[test]
    fn same_device_survives_an_id_switch() {
        let a = r("Living Room", "uuid:A", "192.168.1.10");
        let mut flagged = a.clone();
        flagged.udn_conflict = true;
        assert_ne!(a.stable_id(), flagged.stable_id());
        assert!(a.same_device_as(&flagged));
        assert!(!a.same_device_as(&r("Living Room", "uuid:A", "192.168.1.66")));
        assert!(!a.same_device_as(&r("Living Room", "uuid:B", "192.168.1.10")));
    }

    #[test]
    fn conflicted_group_keeps_its_label() {
        let mut g = r("Living Room", "uuid:A", "192.168.1.10");
        g.zone_name = Some("Living Room".into());
        g.group_members = vec!["Kitchen".into()];
        g.udn_conflict = true;
        assert_eq!(g.display_name(), "Living Room + Kitchen (192.168.1.10)");
    }

    #[test]
    fn conflicting_devices_are_selectable_by_address() {
        let state = DiscoveryState::new();
        let mut other = r("Living Room", "uuid:A", "192.168.1.66");
        other.model_name = Some("Other".into());
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10"), other]);
        let list = state.renderers();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|x| x.udn_conflict));
        let ids: Vec<String> = list.iter().map(|x| x.stable_id()).collect();
        assert!(ids.contains(&"192.168.1.10".to_string()));
        assert!(ids.contains(&"192.168.1.66".to_string()));
        assert!(list.iter().any(|x| x.display_name() == "Living Room (192.168.1.66)"));
        assert!(state.find_by_id("uuid:A").is_none());
        assert_eq!(state.find_by_id("192.168.1.66").unwrap().model_name.as_deref(), Some("Other"));
    }

    #[test]
    fn udn_follows_an_address_change() {
        let state = DiscoveryState::new();
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.10")]);
        state.replace(vec![r("Living Room", "uuid:A", "192.168.1.20")]);
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "192.168.1.20");
        // The new address is now the pinned one.
        state.replace(vec![
            r("Living Room", "uuid:A", "192.168.1.20"),
            r("AAA", "uuid:A", "192.168.1.10"),
        ]);
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "192.168.1.20");
    }

    #[test]
    fn udn_match_beats_ip_literal_udn() {
        let state = DiscoveryState::new();
        state.replace(vec![
            r("AAA", "192.168.1.10", "192.168.1.66"),
            r("Living Room", "uuid:A", "192.168.1.10"),
        ]);
        assert_eq!(state.find_by_id("192.168.1.10").unwrap().ip.to_string(), "192.168.1.66");
        assert_eq!(state.find_by_id("uuid:A").unwrap().ip.to_string(), "192.168.1.10");
    }

    #[test]
    fn read_to_end_bounded_enforces_limit_and_deadline() {
        use std::io::Cursor;
        let far = Instant::now() + Duration::from_secs(60);
        let data = vec![7u8; 20_000];
        assert_eq!(read_to_end_bounded(&mut Cursor::new(data.clone()), 20_000, far).unwrap(), data);
        assert!(read_to_end_bounded(&mut Cursor::new(data), 19_999, far).is_err());
        let past = Instant::now() - Duration::from_millis(1);
        assert!(read_to_end_bounded(&mut Cursor::new(vec![1u8; 10]), 100, past).is_err());
    }

    #[test]
    fn dechunks_multi_chunk_body() {
        // Two chunks whose boundary lands mid-payload — the case a big
        // GetZoneGroupState response hits: interior chunk-size lines
        // must not leak into the reassembled XML.
        let body = b"c\r\n<ZoneGroupSt\r\n4\r\nate>\r\n0\r\n\r\n";
        assert_eq!(decode_maybe_chunked(body), b"<ZoneGroupState>");
    }

    #[test]
    fn plain_body_passes_through() {
        let body = b"<?xml version=\"1.0\"?><Envelope/>";
        assert_eq!(decode_maybe_chunked(body), body);
    }

    #[test]
    fn oversized_later_chunk_does_not_panic() {
        let body = b"1\r\nX\r\nffffffffffffffff\r\n";
        assert_eq!(decode_maybe_chunked(body), b"X");
    }
}
