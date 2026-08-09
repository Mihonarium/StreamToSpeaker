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
}

/// Shared discovery state.  The main loop owns one; the SSDP thread
/// updates it.
#[derive(Default)]
pub struct DiscoveryState {
    inner: Mutex<Vec<Renderer>>,
}

impl DiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn renderers(&self) -> Vec<Renderer> {
        self.inner.lock().unwrap().clone()
    }

    /// Set the renderer list, deduping by control URL.
    pub fn replace(&self, mut new: Vec<Renderer>) {
        new.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        new.dedup_by(|a, b| a.av_transport_control_url == b.av_transport_control_url);
        *self.inner.lock().unwrap() = new;
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

    pub fn first(&self) -> Option<Renderer> {
        self.inner.lock().unwrap().first().cloned()
    }

    /// Match by stable id (UDN if available, otherwise IP literal).
    /// Used by the `/api/select` HTTP endpoint.
    pub fn find_by_id(&self, id: &str) -> Option<Renderer> {
        let inner = self.inner.lock().unwrap();
        for r in inner.iter() {
            if let Some(udn) = &r.udn {
                if udn == id {
                    return Some(r.clone());
                }
            }
            if r.ip.to_string() == id {
                return Some(r.clone());
            }
        }
        None
    }
}

impl Renderer {
    /// Stable identifier suitable for `find_by_id`. UDN if the device
    /// advertises one, IP literal as fallback.
    pub fn stable_id(&self) -> String {
        self.udn.clone().unwrap_or_else(|| self.ip.to_string())
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
        if !self.is_group() {
            return self.friendly_name.clone();
        }
        let base = self.zone_name.as_deref().unwrap_or(&self.friendly_name);
        if self.group_members.len() == 1 {
            format!("{} + {}", base, self.group_members[0])
        } else {
            format!("{} + {}", base, self.group_members.len())
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

/// Spawn the SSDP discovery background thread.  Runs an initial discovery
/// immediately and then every `interval` afterwards.
/// `iface` is the local IPv4 to send multicast from; pass None to let the
/// OS pick (works on single-interface machines, but on multihomed hosts
/// with VPN / virtualization adapters Windows often picks the wrong one
/// and the speaker never sees our M-SEARCH).
pub fn spawn_discovery(state: Arc<DiscoveryState>, interval: Duration, iface: Option<Ipv4Addr>) {
    thread::Builder::new()
        .name("stream-to-speaker-ssdp".to_string())
        .spawn(move || loop {
            match discover_once(Duration::from_secs(3), iface) {
                Ok(found) => {
                    info!("SSDP discovery: {} renderer(s) found", found.len());
                    state.replace(found);
                }
                Err(e) => warn!("SSDP discovery failed: {}", e),
            }
            thread::sleep(interval);
        })
        .expect("spawning SSDP thread");
}

/// One-shot discovery. Returns a deduped list of renderers.
/// `iface` selects the multicast egress interface.
pub fn discover_once(timeout: Duration, iface: Option<Ipv4Addr>) -> Result<Vec<Renderer>> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;

    // Bind to the chosen interface (or 0.0.0.0 if unspecified). Binding
    // to a specific IP ensures the kernel uses *that* interface for both
    // sending the M-SEARCH and receiving the unicast responses Sonos sends
    // back. On a multihomed host this is essential — otherwise Windows
    // can pick a VPN adapter or virtualization NIC and the multicast
    // vanishes into the void.
    let bind_ip = iface.unwrap_or(Ipv4Addr::UNSPECIFIED);
    sock.bind(&SocketAddr::new(IpAddr::V4(bind_ip), 0).into())?;
    sock.set_multicast_ttl_v4(2)?;
    if let Some(ip) = iface {
        // Explicitly select multicast egress interface as well; bind alone
        // isn't always sufficient on Windows.
        sock.set_multicast_if_v4(&ip)?;
        debug!("SSDP socket bound to interface {}", ip);
    } else {
        debug!("SSDP socket bound to 0.0.0.0 (OS picks interface)");
    }

    let udp: UdpSocket = sock.into();

    // Send one M-SEARCH per search target.
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
            .context("sending SSDP M-SEARCH")?;
    }

    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    let mut locations: Vec<String> = Vec::new();

    while Instant::now() < deadline {
        match udp.recv_from(&mut buf) {
            Ok((n, _peer)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(loc) = extract_header(&text, "LOCATION") {
                    if !locations.iter().any(|l| l == &loc) {
                        debug!("SSDP response LOCATION={}", loc);
                        locations.push(loc);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                debug!("SSDP recv error: {}", e);
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
    let ip: IpAddr = match url.host_str() {
        Some(h) => match h.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => {
                use std::net::ToSocketAddrs;
                let port = url.port().unwrap_or(80);
                (h, port)
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut it| it.next())
                    .map(|sa| sa.ip())
                    .ok_or_else(|| anyhow!("could not resolve host {:?}", h))?
            }
        },
        None => return Err(anyhow!("no host in location {}", location)),
    };

    parse_device_description(&xml, &base, ip)
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
    let path = if url.path().is_empty() { "/" } else { url.path() };

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
    stream.read_to_end(&mut buf)?;

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
        if size == 0 || p + size > body.len() {
            break;
        }
        out.extend_from_slice(&body[p..p + size]);
        p += size + 2; // skip chunk + trailing \r\n
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
}
