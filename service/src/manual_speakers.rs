//! Speakers added by address instead of discovered.
//!
//! Multicast discovery (SSDP / mDNS) doesn't cross subnets, VLANs or many
//! mesh / guest-Wi-Fi setups, while a unicast connection to the speaker
//! still works. These entries let the user name the speaker directly:
//!
//!   * **AirPlay** — an IPv4 address or hostname plus the RTSP port
//!     (7000 for AirPlay 2 receivers, 5000 for many AirPlay 1 ones). With
//!     no TXT record to go on, the capabilities come from the receiver's
//!     plaintext `GET /info` answer (features, model, public key) when it
//!     gives one; otherwise it's treated as a plain AirPlay 1 receiver
//!     (ALAC, no encryption) — the one combination every RAOP receiver
//!     accepts.
//!   * **UPnP** — the device-description URL (the `LOCATION` an SSDP
//!     reply would have carried). It is fetched and parsed exactly like a
//!     discovered one at connect time.
//!
//! Manual entries have their own ids and are never merged with a
//! discovered device, even when both are the same hardware.

use plist::Value;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::airplay::AirPlayRenderer;

/// Default AirPlay port offered in the add dialog.
pub const DEFAULT_AIRPLAY_PORT: u16 = 7000;

/// Id prefix of a manual UPnP entry. Manual AirPlay entries keep the
/// `airplay:` prefix (so every AirPlay path treats them alike) with a
/// `manual-` device part that a discovered MAC (hex digits only) can never
/// produce.
const UPNP_ID_PREFIX: &str = "upnp-manual:";
const AIRPLAY_MANUAL_MAC_PREFIX: &str = "manual-";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualKind {
    AirPlay,
    Upnp,
}

/// One persisted manual entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualSpeaker {
    pub kind: ManualKind,
    /// User-chosen name; empty = show the address.
    #[serde(default)]
    pub name: String,
    /// AirPlay: IPv4 literal or hostname. UPnP: device-description URL.
    pub host: String,
    /// AirPlay RTSP port. Unused (0) for UPnP.
    #[serde(default)]
    pub port: u16,
}

impl ManualSpeaker {
    /// Stable id used by the speaker list, selection and per-speaker
    /// settings.
    pub fn id(&self) -> String {
        match self.kind {
            ManualKind::AirPlay => format!("airplay:{}", self.airplay_mac_id()),
            ManualKind::Upnp => format!("{}{}", UPNP_ID_PREFIX, self.host),
        }
    }

    /// The device part of an AirPlay entry's id (what a discovered
    /// receiver carries as its MAC).
    fn airplay_mac_id(&self) -> String {
        format!(
            "{}{}:{}",
            AIRPLAY_MANUAL_MAC_PREFIX,
            self.host.to_ascii_lowercase(),
            self.port
        )
    }

    /// Name for the speaker list: the user's, else the address.
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        match self.kind {
            ManualKind::AirPlay => self.host.clone(),
            ManualKind::Upnp => url::Url::parse(&self.host)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_string()))
                .unwrap_or_else(|| self.host.clone()),
        }
    }

    /// Address column text.
    pub fn address_label(&self) -> String {
        match self.kind {
            ManualKind::AirPlay => format!("{}:{}", self.host, self.port),
            ManualKind::Upnp => url::Url::parse(&self.host)
                .ok()
                .and_then(|u| {
                    let h = u.host_str()?.to_string();
                    Some(match u.port() {
                        Some(p) => format!("{}:{}", h, p),
                        None => h,
                    })
                })
                .unwrap_or_else(|| self.host.clone()),
        }
    }

    /// Two entries name the same endpoint (case-insensitive host).
    fn same_endpoint(&self, other: &ManualSpeaker) -> bool {
        self.kind == other.kind
            && self.port == other.port
            && self.host.eq_ignore_ascii_case(&other.host)
    }
}

/// True for ids produced by [`ManualSpeaker::id`].
pub fn is_manual_id(id: &str) -> bool {
    id.starts_with(UPNP_ID_PREFIX)
        || id
            .strip_prefix("airplay:")
            .map(|m| m.starts_with(AIRPLAY_MANUAL_MAC_PREFIX))
            .unwrap_or(false)
}

/// Validate the add-dialog fields into an entry. `existing` is checked
/// for duplicates. Messages are user-facing.
pub fn validate(
    kind: ManualKind,
    name: &str,
    host: &str,
    port: &str,
    existing: &[ManualSpeaker],
) -> Result<ManualSpeaker, String> {
    let host = host.trim();
    let entry = match kind {
        ManualKind::AirPlay => {
            validate_host(host)?;
            let port = port.trim();
            let port: u16 = if port.is_empty() {
                DEFAULT_AIRPLAY_PORT
            } else {
                port.parse()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| "The port must be a number from 1 to 65535.".to_string())?
            };
            ManualSpeaker {
                kind,
                name: name.trim().to_string(),
                host: host.to_string(),
                port,
            }
        }
        ManualKind::Upnp => {
            validate_description_url(host)?;
            ManualSpeaker {
                kind,
                name: name.trim().to_string(),
                host: host.to_string(),
                port: 0,
            }
        }
    };
    if existing.iter().any(|e| e.same_endpoint(&entry)) {
        return Err("That speaker is already in the list.".to_string());
    }
    Ok(entry)
}

/// IPv4 literal or DNS hostname. IPv6 is refused: the streaming paths
/// (SDP `IN IP4`, the UDP sockets) are IPv4-only.
fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("Enter the speaker's IP address or hostname.".to_string());
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return match ip {
            IpAddr::V6(_) => Err("IPv6 addresses aren't supported — use the speaker's IPv4 \
                                  address or hostname."
                .to_string()),
            IpAddr::V4(v4) if v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast() => {
                Err(format!("{} isn't a speaker address.", v4))
            }
            IpAddr::V4(_) => Ok(()),
        };
    }
    if host.contains(':') {
        return Err("Put the port in the Port field, not after the address.".to_string());
    }
    let valid = host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if !valid {
        return Err(format!("\"{}\" isn't a valid IP address or hostname.", host));
    }
    Ok(())
}

fn validate_description_url(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("Enter the speaker's device-description URL.".to_string());
    }
    let url = url::Url::parse(s).map_err(|_| {
        "Enter the full device-description URL, e.g. http://192.168.1.20:1400/xml/device_description.xml"
            .to_string()
    })?;
    if url.scheme() != "http" {
        return Err("Only http:// device-description URLs are supported.".to_string());
    }
    match url.host() {
        None => Err("The URL has no host.".to_string()),
        Some(url::Host::Ipv6(_)) => {
            Err("IPv6 addresses aren't supported — use the IPv4 address or hostname.".to_string())
        }
        Some(_) => Ok(()),
    }
}

/// Resolve a manual AirPlay host to the IPv4 address to connect to.
pub fn resolve_ipv4(host: &str, port: u16) -> Result<Ipv4Addr, String> {
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return Ok(v4);
    }
    // The OS resolver has no timeout of its own; give up after a few
    // seconds (the lookup thread finishes on its own and is discarded).
    let (tx, rx) = crossbeam_channel::bounded(1);
    let name = host.to_string();
    std::thread::Builder::new()
        .name("stream-to-speaker-resolve".into())
        .spawn(move || {
            let result = (name.as_str(), port).to_socket_addrs().map(|addrs| {
                addrs
                    .filter_map(|a| match a.ip() {
                        IpAddr::V4(v4) => Some(v4),
                        IpAddr::V6(_) => None,
                    })
                    .next()
            });
            let _ = tx.send(result);
        })
        .map_err(|e| format!("couldn't resolve {}: {}", host, e))?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(Some(v4))) => Ok(v4),
        Ok(Ok(None)) => Err(format!("{} has no IPv4 address", host)),
        Ok(Err(e)) => Err(format!("couldn't resolve {}: {}", host, e)),
        Err(_) => Err(format!("couldn't resolve {}: no answer within 5 s", host)),
    }
}

/// What a receiver's plaintext `GET /info` revealed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiverInfo {
    pub features: Option<u64>,
    pub model: Option<String>,
    pub name: Option<String>,
    /// HomeKit public key, hex.
    pub pk: Option<String>,
}

/// Probe a manual AirPlay endpoint. `Err` when nothing accepts TCP on
/// `addr` (so the user gets "unreachable" instead of a protocol error);
/// `Ok(None)` when something listens but doesn't answer `/info` (an
/// AirPlay 1 receiver).
pub fn probe_info(addr: SocketAddr, timeout: Duration) -> Result<Option<ReceiverInfo>, String> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("can't reach {}: {}", addr, e))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = format!(
        "GET /info RTSP/1.0\r\nCSeq: 1\r\nX-Apple-ProtocolVersion: 1\r\nUser-Agent: {}\r\n\r\n",
        crate::PRODUCT_UA
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return Ok(None);
    }
    let deadline = Instant::now() + timeout;
    Ok(read_response(&mut stream, deadline).and_then(|(status, body)| {
        (status == 200).then(|| parse_info(&body)).flatten()
    }))
}

/// Largest `/info` response we read. Real ones are a few KB.
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Read one RTSP response: `(status, body)`. None on any malformation,
/// size overrun or timeout.
fn read_response(stream: &mut impl Read, deadline: Instant) -> Option<(u16, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_HEADER_BYTES || Instant::now() >= deadline {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = buf.split_off(header_end);
    while body.len() < content_length {
        if Instant::now() >= deadline {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Some((status, body))
}

/// Pull the fields we route on out of an `/info` plist (binary or XML).
fn parse_info(body: &[u8]) -> Option<ReceiverInfo> {
    let value: Value = plist::from_bytes(body).ok()?;
    let dict = value.as_dictionary()?;
    let string = |k: &str| {
        dict.get(k)
            .and_then(|v| v.as_string())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    let features = dict.get("features").and_then(|v| {
        v.as_unsigned_integer()
            .or_else(|| v.as_signed_integer().map(|i| i as u64))
    });
    let pk = dict
        .get("pk")
        .and_then(|v| v.as_data())
        .map(|d| d.iter().map(|b| format!("{:02x}", b)).collect::<String>());
    Some(ReceiverInfo {
        features,
        model: string("model"),
        name: string("name"),
        pk,
    })
}

/// Build the renderer record the AirPlay session code expects for a
/// manual entry. RAOP is always offered (ALAC, unencrypted — accepted by
/// every RAOP receiver); the AirPlay 2 path additionally when `/info`
/// answered, with its feature word deciding what the receiver supports.
pub fn airplay_renderer(
    entry: &ManualSpeaker,
    ip: Ipv4Addr,
    info: Option<&ReceiverInfo>,
) -> AirPlayRenderer {
    AirPlayRenderer {
        friendly_name: entry.display_name(),
        mac_id: entry.airplay_mac_id(),
        ip: IpAddr::V4(ip),
        port: entry.port,
        airplay_port: info.map(|_| entry.port),
        encryption_types: vec![0],
        codecs: vec![1],
        password_protected: false,
        encryption_key_required: false,
        features: info.and_then(|i| i.features),
        pk: info.and_then(|i| i.pk.clone()),
        model: info.and_then(|i| i.model.clone()),
        // No TXT record, so no stereo-pair grouping for manual entries.
        group: None,
        pair: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn airplay(host: &str, port: &str) -> Result<ManualSpeaker, String> {
        validate(ManualKind::AirPlay, "", host, port, &[])
    }

    #[test]
    fn airplay_host_validation() {
        assert_eq!(airplay("192.168.1.20", "").unwrap().port, DEFAULT_AIRPLAY_PORT);
        assert_eq!(airplay("kitchen.local", "5000").unwrap().port, 5000);
        assert!(airplay("", "7000").is_err());
        assert!(airplay("fe80::1", "7000").unwrap_err().contains("IPv6"));
        assert!(airplay("[::1]", "7000").unwrap_err().contains("IPv6"));
        assert!(airplay("192.168.1.20:7000", "").is_err());
        assert!(airplay("0.0.0.0", "").is_err());
        assert!(airplay("bad host", "").is_err());
        assert!(airplay("-x.local", "").is_err());
        assert!(airplay("192.168.1.20", "0").is_err());
        assert!(airplay("192.168.1.20", "70000").is_err());
        assert!(airplay("192.168.1.20", "abc").is_err());
    }

    #[test]
    fn duplicates_are_refused_case_insensitively() {
        let first = airplay("Kitchen.local", "7000").unwrap();
        let dup = validate(ManualKind::AirPlay, "x", "kitchen.LOCAL", "7000", &[first.clone()]);
        assert!(dup.is_err());
        // Same host, different port is a different endpoint.
        assert!(validate(ManualKind::AirPlay, "", "kitchen.local", "5000", &[first]).is_ok());
    }

    #[test]
    fn upnp_url_validation() {
        let ok = validate(
            ManualKind::Upnp,
            "",
            "http://192.168.1.30:1400/xml/device_description.xml",
            "",
            &[],
        )
        .unwrap();
        assert_eq!(ok.display_name(), "192.168.1.30");
        assert_eq!(ok.address_label(), "192.168.1.30:1400");
        assert!(validate(ManualKind::Upnp, "", "https://x/desc.xml", "", &[]).is_err());
        assert!(validate(ManualKind::Upnp, "", "http://[fe80::1]/d.xml", "", &[]).is_err());
        assert!(validate(ManualKind::Upnp, "", "192.168.1.30", "", &[]).is_err());
    }

    #[test]
    fn manual_ids_never_look_discovered() {
        let a = airplay("192.168.1.20", "7000").unwrap();
        assert_eq!(a.id(), "airplay:manual-192.168.1.20:7000");
        assert!(is_manual_id(&a.id()));
        assert!(!is_manual_id("airplay:AABBCCDDEEFF"));
        assert!(!is_manual_id("uuid:RINCON_1"));
        let u = validate(ManualKind::Upnp, "", "http://h/d.xml", "", &[]).unwrap();
        assert!(is_manual_id(&u.id()));
    }

    #[test]
    fn info_plist_fields_are_extracted() {
        let mut d = plist::Dictionary::new();
        d.insert("features".into(), Value::Integer(0x1C340_445F8A00u64.into()));
        d.insert("model".into(), Value::String("AudioAccessory5,1".into()));
        d.insert("name".into(), Value::String("Kitchen".into()));
        d.insert("pk".into(), Value::Data(vec![0xab, 0x01]));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(d)).unwrap();
        let info = parse_info(&body).unwrap();
        assert_eq!(info.features, Some(0x1C340_445F8A00));
        assert_eq!(info.model.as_deref(), Some("AudioAccessory5,1"));
        assert_eq!(info.pk.as_deref(), Some("ab01"));
        assert!(parse_info(b"not a plist").is_none());
    }

    #[test]
    fn response_reader_is_bounded() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let ok = b"RTSP/1.0 200 OK\r\nContent-Length: 3\r\nCSeq: 1\r\n\r\nabcdef";
        assert_eq!(
            read_response(&mut &ok[..], deadline),
            Some((200, b"abc".to_vec()))
        );
        let huge = b"RTSP/1.0 200 OK\r\nContent-Length: 99999999\r\n\r\n";
        assert_eq!(read_response(&mut &huge[..], deadline), None);
        let short = b"RTSP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nab";
        assert_eq!(read_response(&mut &short[..], deadline), None);
        assert_eq!(read_response(&mut &b"garbage"[..], deadline), None);
    }

    #[test]
    fn renderer_offers_ap2_only_when_info_answered() {
        let e = airplay("192.168.1.20", "7000").unwrap();
        let ip: Ipv4Addr = "192.168.1.20".parse().unwrap();
        let legacy = airplay_renderer(&e, ip, None);
        assert!(legacy.supports_legacy_raop());
        assert!(!legacy.supports_airplay2());
        assert_eq!(legacy.stable_id(), e.id());
        let info = ReceiverInfo {
            features: Some((1 << 9) | (1 << 48)),
            ..Default::default()
        };
        let ap2 = airplay_renderer(&e, ip, Some(&info));
        assert!(ap2.supports_airplay2());
        assert_eq!(ap2.airplay_port, Some(7000));
    }
}
