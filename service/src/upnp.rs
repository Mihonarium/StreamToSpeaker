//! UPnP SOAP client — minimal, just the actions we need.
//!
//! Actions:
//!   * AVTransport: SetAVTransportURI, Play, Stop
//!   * RenderingControl: SetVolume, SetMute, GetVolume, GetMute
//!   * GroupRenderingControl (Sonos): SetGroupVolume, GetGroupVolume,
//!     SetGroupMute — group-wide volume, addressed to the coordinator
//!   * ZoneGroupTopology (Sonos): GetZoneGroupState
//!
//! SOAP is just HTTP POST with a specific Content-Type + SOAPAction header
//! and an envelope-shaped XML body. We hand-roll it over TCP so we don't
//! pull in another HTTP client.

use anyhow::{anyhow, Context, Result};
use log::debug;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Same as the UPnP examples in swyh-rs: percent-encode only what HTTP
/// requires (we keep the SOAP envelope readable in logs).
const URI_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'#')
    .add(b'?')
    .add(b'{')
    .add(b'}');

/// Issue SetAVTransportURI to point the renderer at our HTTP stream URL.
pub fn set_av_transport_uri(
    control_url: &str,
    stream_uri: &str,
    didl_metadata: &str,
) -> Result<()> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID>
<CurrentURI>{uri}</CurrentURI>
<CurrentURIMetaData>{meta}</CurrentURIMetaData>
</u:SetAVTransportURI>
</s:Body>
</s:Envelope>"#,
        uri = xml_escape(stream_uri),
        meta = xml_escape(didl_metadata),
    );
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI",
        &body,
    )
    .map(|_| ())
}

pub fn play(control_url: &str) -> Result<()> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID>
<Speed>1</Speed>
</u:Play>
</s:Body>
</s:Envelope>"#;
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:AVTransport:1#Play",
        body,
    )
    .map(|_| ())
}

pub fn stop(control_url: &str) -> Result<()> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Stop xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID>
</u:Stop>
</s:Body>
</s:Envelope>"#;
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:AVTransport:1#Stop",
        body,
    )
    .map(|_| ())
}

/// Set volume (0..=100) on the Master channel.
pub fn set_volume(control_url: &str, level: u32) -> Result<()> {
    let level = level.min(100);
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:SetVolume xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
<InstanceID>0</InstanceID>
<Channel>Master</Channel>
<DesiredVolume>{level}</DesiredVolume>
</u:SetVolume>
</s:Body>
</s:Envelope>"#
    );
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:RenderingControl:1#SetVolume",
        &body,
    )
    .map(|_| ())
}

pub fn set_mute(control_url: &str, muted: bool) -> Result<()> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:SetMute xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
<InstanceID>0</InstanceID>
<Channel>Master</Channel>
<DesiredMute>{m}</DesiredMute>
</u:SetMute>
</s:Body>
</s:Envelope>"#,
        m = if muted { 1 } else { 0 }
    );
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:RenderingControl:1#SetMute",
        &body,
    )
    .map(|_| ())
}

pub fn get_volume(control_url: &str) -> Result<u32> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetVolume xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
<InstanceID>0</InstanceID>
<Channel>Master</Channel>
</u:GetVolume>
</s:Body>
</s:Envelope>"#;
    let resp = soap_post(
        control_url,
        "urn:schemas-upnp-org:service:RenderingControl:1#GetVolume",
        body,
    )?;
    // Parse out <CurrentVolume>NN</CurrentVolume>.
    extract_int_tag(&resp, "CurrentVolume")
        .ok_or_else(|| anyhow!("CurrentVolume missing in response"))
}

pub fn get_mute(control_url: &str) -> Result<bool> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetMute xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
<InstanceID>0</InstanceID>
<Channel>Master</Channel>
</u:GetMute>
</s:Body>
</s:Envelope>"#;
    let resp = soap_post(
        control_url,
        "urn:schemas-upnp-org:service:RenderingControl:1#GetMute",
        body,
    )?;
    let n = extract_int_tag(&resp, "CurrentMute")
        .ok_or_else(|| anyhow!("CurrentMute missing"))?;
    Ok(n != 0)
}

/// Set the Sonos group volume (0..=100) via GroupRenderingControl on the
/// group coordinator. Scales every member proportionally — the same
/// semantics as the group slider in the Sonos app. Returns 701 when
/// addressed to a non-coordinator, so callers must route to the
/// coordinator (ssdp::Renderer::group_volume_control_url does).
pub fn set_group_volume(control_url: &str, level: u32) -> Result<()> {
    let level = level.min(100);
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:SetGroupVolume xmlns:u="urn:schemas-upnp-org:service:GroupRenderingControl:1">
<InstanceID>0</InstanceID>
<DesiredVolume>{level}</DesiredVolume>
</u:SetGroupVolume>
</s:Body>
</s:Envelope>"#
    );
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:GroupRenderingControl:1#SetGroupVolume",
        &body,
    )
    .map(|_| ())
}

pub fn get_group_volume(control_url: &str) -> Result<u32> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetGroupVolume xmlns:u="urn:schemas-upnp-org:service:GroupRenderingControl:1">
<InstanceID>0</InstanceID>
</u:GetGroupVolume>
</s:Body>
</s:Envelope>"#;
    let resp = soap_post(
        control_url,
        "urn:schemas-upnp-org:service:GroupRenderingControl:1#GetGroupVolume",
        body,
    )?;
    extract_int_tag(&resp, "CurrentVolume")
        .ok_or_else(|| anyhow!("CurrentVolume missing in GetGroupVolume response"))
}

pub fn set_group_mute(control_url: &str, muted: bool) -> Result<()> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:SetGroupMute xmlns:u="urn:schemas-upnp-org:service:GroupRenderingControl:1">
<InstanceID>0</InstanceID>
<DesiredMute>{m}</DesiredMute>
</u:SetGroupMute>
</s:Body>
</s:Envelope>"#,
        m = if muted { 1 } else { 0 }
    );
    soap_post(
        control_url,
        "urn:schemas-upnp-org:service:GroupRenderingControl:1#SetGroupMute",
        &body,
    )
    .map(|_| ())
}

/// Fetch the entity-decoded ZoneGroupState XML from a Sonos device's
/// ZoneGroupTopology service. Parsing lives in `sonos.rs`.
pub fn get_zone_group_state(control_url: &str) -> Result<String> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetZoneGroupState xmlns:u="urn:schemas-upnp-org:service:ZoneGroupTopology:1">
</u:GetZoneGroupState>
</s:Body>
</s:Envelope>"#;
    let resp = soap_post(
        control_url,
        "urn:schemas-upnp-org:service:ZoneGroupTopology:1#GetZoneGroupState",
        body,
    )?;
    let inner = crate::gena::find_tag_text(&resp, "ZoneGroupState")
        .ok_or_else(|| anyhow!("ZoneGroupState missing in response"))?;
    Ok(crate::gena::xml_unescape(&inner))
}

/// Build the DIDL-Lite metadata blob for SetAVTransportURI.
///
/// WAV variant — Sonos's preferred input. `audio/wav` MIME type and
/// `DLNA.ORG_PN=WAV`. swyh-rs uses
/// `DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=03700000…` for WAV; we
/// match that. The stream body begins with a 44-byte RIFF/WAVE header
/// generated by http_server.rs's wav_header_streaming().
///
/// Sonos will show "No Content" in the Now Playing card if only
/// `dc:title` is present — it expects at least a creator/artist to
/// treat the metadata as a real track. We populate both with the
/// product name so the card shows "Stream To Speaker" rather than a
/// blank placeholder.
pub fn didl_lite_metadata(stream_uri: &str, title: &str, initial_buffer_ms: u32) -> String {
    let _ = initial_buffer_ms;  // reserved for future "x-sonos-buffer-ms" hint
    let title_e = xml_escape(title);
    format!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
<item id="1" parentID="0" restricted="0">
<dc:title>{title}</dc:title>
<dc:creator>{title}</dc:creator>
<upnp:artist>{title}</upnp:artist>
<upnp:album>Live audio from this PC</upnp:album>
<upnp:class>object.item.audioItem.musicTrack</upnp:class>
<res protocolInfo="http-get:*:audio/wav:DLNA.ORG_PN=WAV;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=03700000000000000000000000000000">{uri}</res>
</item>
</DIDL-Lite>"#,
        title = title_e,
        uri = xml_escape(stream_uri),
    )
}

/// Encode our http://host:port/stream.raw URI for inclusion inside SOAP.
pub fn encode_stream_uri(stream_uri: &str) -> String {
    utf8_percent_encode(stream_uri, URI_ENCODE_SET).to_string()
}

// -----------------------------------------------------------------------------
// Low-level SOAP POST + helpers
// -----------------------------------------------------------------------------

fn soap_post(control_url: &str, soap_action: &str, body: &str) -> Result<String> {
    let url = url::Url::parse(control_url).context("parsing control URL")?;
    let host = url.host_str().ok_or_else(|| anyhow!("missing host"))?;
    let port = url.port().unwrap_or(80);
    let path = if url.path().is_empty() { "/" } else { url.path() };
    let path_q = if let Some(q) = url.query() {
        format!("{}?{}", path, q)
    } else {
        path.to_string()
    };

    let addr: SocketAddr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("no addrs for {}:{}", host, port))?;

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let body_bytes = body.as_bytes();
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         Content-Length: {clen}\r\n\
         SOAPAction: \"{action}\"\r\n\
         Connection: close\r\n\
         User-Agent: stream-to-speaker/0.1\r\n\
         \r\n",
        path = path_q,
        host = host,
        port = port,
        clen = body_bytes.len(),
        action = soap_action,
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(body_bytes)?;
    stream.flush()?;

    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf)?;

    // Split head/body on bytes, then peel chunked transfer-encoding if
    // present. Sonos chunks HTTP/1.1 responses (same behavior ssdp.rs's
    // http_get dodges by speaking HTTP/1.0); small SOAP replies fit one
    // chunk and parsed fine by accident, but GetZoneGroupState bodies
    // run tens of KB and interior chunk-size lines would corrupt the
    // escaped XML. decode_maybe_chunked is a no-op for plain bodies.
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..split]);
    let body_part = crate::ssdp::decode_maybe_chunked(&buf[split..]);

    let status_line = head.lines().next().unwrap_or("");
    debug!("SOAP {} -> {}", soap_action, status_line);
    if !status_line.contains(" 200 ") {
        return Err(anyhow!("SOAP {} failed: {}", soap_action, status_line));
    }
    Ok(String::from_utf8_lossy(&body_part).into_owned())
}

/// Minimal XML attribute / text escape.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Pull `<Tag>123</Tag>` out of a SOAP response body. Returns None if not
/// found or unparseable.
fn extract_int_tag(xml: &str, tag: &str) -> Option<u32> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = xml.find(&open)?;
    let after = &xml[i + open.len()..];
    let j = after.find(&close)?;
    after[..j].trim().parse::<u32>().ok()
}
