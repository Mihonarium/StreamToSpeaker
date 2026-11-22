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
//! and an envelope-shaped XML body, sent with the app's own small client
//! (`device_http`).

use crate::device_http::{self, HttpError, Limits, Version};
use anyhow::{anyhow, Result};
use log::debug;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
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
    soap_post(control_url, AV_TRANSPORT, "SetAVTransportURI", &body, &TRANSPORT)?;
    Ok(())
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
    soap_post(control_url, AV_TRANSPORT, "Play", body, &TRANSPORT)?;
    Ok(())
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
    soap_post(control_url, AV_TRANSPORT, "Stop", body, &SHORT)?;
    Ok(())
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
    soap_post(control_url, RENDERING_CONTROL, "SetVolume", &body, &SHORT)?;
    Ok(())
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
    soap_post(control_url, RENDERING_CONTROL, "SetMute", &body, &SHORT)?;
    Ok(())
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
    let resp = soap_post(control_url, RENDERING_CONTROL, "GetVolume", body, &SHORT)?;
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
    let resp = soap_post(control_url, RENDERING_CONTROL, "GetMute", body, &SHORT)?;
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
    soap_post(control_url, GROUP_RENDERING_CONTROL, "SetGroupVolume", &body, &SHORT)?;
    Ok(())
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
    let resp = soap_post(control_url, GROUP_RENDERING_CONTROL, "GetGroupVolume", body, &SHORT)?;
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
    soap_post(control_url, GROUP_RENDERING_CONTROL, "SetGroupMute", &body, &SHORT)?;
    Ok(())
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
    let resp = soap_post(control_url, ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", body, &TOPOLOGY)?;
    zone_group_state_from_response(&resp)
}

/// The topology document a GetZoneGroupState response carries, escaped,
/// as the text of its `<ZoneGroupState>` element — decoded.
pub(crate) fn zone_group_state_from_response(resp: &str) -> Result<String> {
    let inner = crate::gena::find_tag_text(resp, "ZoneGroupState")
        .ok_or_else(|| anyhow!("ZoneGroupState missing in response"))?;
    Ok(crate::gena::decode_xml_text(&inner))
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

const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const GROUP_RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:GroupRenderingControl:1";
const ZONE_GROUP_TOPOLOGY: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:1";

/// Largest SOAP answer accepted for most actions.
const MAX_SOAP_RESPONSE_BYTES: usize = 4 << 20;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Volume and mute, which someone is waiting on, and the Stop sent before
/// connecting or when moving to another speaker, which is best-effort.
const SHORT: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    deadline: Duration::from_secs(5),
    max_bytes: MAX_SOAP_RESPONSE_BYTES,
};

/// SetAVTransportURI and Play. A renderer may fetch the stream before it
/// answers them, and UPnP gives a device 30 s to complete an action.
const TRANSPORT: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    deadline: Duration::from_secs(30),
    max_bytes: MAX_SOAP_RESPONSE_BYTES,
};

/// GetZoneGroupState: a large household's topology (escaped XML, well
/// under 1 MiB), supplied by an unauthenticated LAN peer, so bounded
/// before it is parsed or decoded.
const TOPOLOGY: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    deadline: Duration::from_secs(15),
    max_bytes: 1 << 20,
};

/// Why a UPnP action did not succeed.
#[derive(Debug)]
pub(crate) struct ActionError {
    /// The action's name, `SetAVTransportURI`.
    pub action: &'static str,
    pub failure: Failure,
}

#[derive(Debug)]
pub(crate) enum Failure {
    /// No answer to look at: the device couldn't be reached, didn't answer
    /// in time, or what came back couldn't be read.
    NoAnswer(HttpError),
    /// The device answered and refused, with this status (`HTTP 500
    /// Internal Server Error`).
    Refused { status: String },
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failure {
            Failure::NoAnswer(e) => write!(f, "{}: {}", self.action, e),
            Failure::Refused { status } => write!(f, "{} refused: {}", self.action, status),
        }
    }
}

impl std::error::Error for ActionError {}

fn soap_post(
    control_url: &str,
    service: &str,
    action: &'static str,
    body: &str,
    limits: &Limits,
) -> Result<String, ActionError> {
    let soap_action = format!("\"{service}#{action}\"");
    let content_length = body.len().to_string();
    let headers = [
        ("Content-Type", "text/xml; charset=\"utf-8\""),
        ("Content-Length", content_length.as_str()),
        ("SOAPAction", soap_action.as_str()),
        ("Connection", "close"),
        ("User-Agent", device_http::USER_AGENT),
    ];
    let request = device_http::Request {
        method: "POST",
        url: control_url,
        version: Version::Http11,
        headers: &headers,
        body: body.as_bytes(),
    };
    let fail = |failure| ActionError { action, failure };
    let resp = device_http::exchange(&request, limits).map_err(|e| fail(Failure::NoAnswer(e)))?;
    debug!("SOAP {}#{} -> {}", service, action, resp.status_text());
    if resp.status != 200 {
        return Err(fail(Failure::Refused { status: resp.status_text() }));
    }
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A GetZoneGroupState response as a Sonos sends it: the topology is
    /// the escaped text of `<ZoneGroupState>`, non-ASCII as plain UTF-8,
    /// and a value's own `&` escaped twice.
    const ZGS_RESPONSE: &str = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">"#,
        r#"<s:Body><u:GetZoneGroupStateResponse xmlns:u="urn:schemas-upnp-org:service:ZoneGroupTopology:1">"#,
        r#"<ZoneGroupState>&lt;ZoneGroupState&gt;&lt;ZoneGroups&gt;"#,
        r#"&lt;ZoneGroup Coordinator=&quot;RINCON_AAA&quot; ID=&quot;RINCON_AAA:1&quot;&gt;"#,
        r#"&lt;ZoneGroupMember UUID=&quot;RINCON_AAA&quot; Location=&quot;http://192.168.1.10:1400/xml/device_description.xml&quot; ZoneName=&quot;Wohnzimmer&quot;/&gt;"#,
        r#"&lt;ZoneGroupMember UUID=&quot;RINCON_BBB&quot; Location=&quot;http://192.168.1.11:1400/xml/device_description.xml&quot; ZoneName=&quot;Küche&quot;/&gt;"#,
        r#"&lt;/ZoneGroup&gt;"#,
        r#"&lt;ZoneGroup Coordinator=&quot;RINCON_CCC&quot; ID=&quot;RINCON_CCC:2&quot;&gt;"#,
        r#"&lt;ZoneGroupMember UUID=&quot;RINCON_CCC&quot; Location=&quot;http://192.168.1.12:1400/xml/device_description.xml&quot; ZoneName=&quot;Emma’s Room&quot;/&gt;"#,
        r#"&lt;ZoneGroupMember UUID=&quot;RINCON_DDD&quot; Location=&quot;http://192.168.1.13:1400/xml/device_description.xml&quot; ZoneName=&quot;Tom &amp;amp; Jerry&quot;/&gt;"#,
        r#"&lt;/ZoneGroup&gt;"#,
        r#"&lt;/ZoneGroups&gt;&lt;/ZoneGroupState&gt;</ZoneGroupState>"#,
        r#"</u:GetZoneGroupStateResponse></s:Body></s:Envelope>"#,
    );

    #[test]
    fn zone_group_state_is_the_decoded_text_of_the_response() {
        let zgs = zone_group_state_from_response(ZGS_RESPONSE).unwrap();
        assert!(zgs.starts_with(r#"<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator="RINCON_AAA""#), "{zgs}");
        assert!(zgs.contains(r#"ZoneName="Küche""#), "{zgs}");
        assert!(zgs.contains(r#"ZoneName="Emma’s Room""#), "{zgs}");
        // The inner document's own escape is left for its parser.
        assert!(zgs.contains(r#"ZoneName="Tom &amp; Jerry""#), "{zgs}");
    }

    #[test]
    fn zone_group_state_missing_is_an_error() {
        assert!(zone_group_state_from_response("<s:Envelope/>").is_err());
    }

    // ---- the SOAP client against a renderer -----------------------------

    use crate::fake_renderer::{FakeRenderer, Reply};
    use std::time::Instant;

    const RC: &str = RENDERING_CONTROL;

    fn volume_body(level: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:GetVolumeResponse xmlns:u=\"{RC}\"><CurrentVolume>{level}</CurrentVolume></u:GetVolumeResponse>\
             </s:Body></s:Envelope>"
        )
    }

    #[test]
    fn an_answer_on_a_connection_left_open_is_complete_at_its_length() {
        // The device ignores `Connection: close`; its answer is whole.
        let fake = FakeRenderer::start(|_| Reply::soap_ok(AV_TRANSPORT, "Stop", &[]).then_hold_open());
        let start = Instant::now();
        stop(&fake.url("/AVTransport/Control")).expect("a complete answer is success");
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
    }

    #[test]
    fn set_av_transport_uri_waits_for_a_slow_renderer() {
        // UPnP gives a device 30 s to answer an action; this one takes 6.
        let fake = FakeRenderer::start(|_| {
            Reply::soap_ok(AV_TRANSPORT, "SetAVTransportURI", &[]).after(Duration::from_secs(6))
        });
        set_av_transport_uri(&fake.url("/AVTransport/Control"), "http://127.0.0.1:1/stream.raw", "")
            .expect("a slow answer is still an answer");
    }

    #[test]
    fn a_status_line_without_a_reason_phrase_is_success() {
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200\r\nContent-Length: 0\r\n\r\n"));
        stop(&fake.url("/AVTransport/Control")).expect("200 without a reason phrase");
    }

    #[test]
    fn a_chunked_answer_is_read_by_its_chunks() {
        // Sonos chunks HTTP/1.1 answers; this one also comes in pieces and
        // leaves the connection open.
        let body = volume_body("37");
        let (a, b) = (body[..40].to_string(), body[40..].to_string());
        let fake = FakeRenderer::start(move |_| {
            Reply::raw(format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/xml\r\n\r\n{:x}\r\n{a}\r\n",
                a.len()
            ))
            .then(Duration::from_millis(100), format!("{:x};ext=1\r\n{b}\r\n0\r\nX-Trailer: 1\r\n\r\n", b.len()))
            .then_hold_open()
        });
        assert_eq!(get_volume(&fake.url("/RenderingControl/Control")).unwrap(), 37);
    }

    #[test]
    fn a_body_that_looks_like_a_chunk_is_not_taken_for_one() {
        // Chunked or not is the header's to say, not the body's.
        let body = format!("abc\r\n{}", volume_body("37"));
        let fake = FakeRenderer::start(move |_| Reply::ok(&body));
        assert_eq!(get_volume(&fake.url("/RenderingControl/Control")).unwrap(), 37);
    }

    #[test]
    fn header_lines_ending_in_a_bare_line_feed_are_read() {
        let body = volume_body("37");
        let fake = FakeRenderer::start(move |_| {
            Reply::raw(format!("HTTP/1.1 200 OK\nContent-Type: text/xml\nContent-Length: {}\n\n{body}", body.len()))
        });
        assert_eq!(get_volume(&fake.url("/RenderingControl/Control")).unwrap(), 37);
    }

    #[test]
    fn an_oversized_topology_answer_is_refused() {
        let big = format!("<ZoneGroupState>{}</ZoneGroupState>", "x".repeat(1100 * 1024));
        let fake = FakeRenderer::start(move |_| Reply::ok(&big));
        assert!(get_zone_group_state(&fake.url("/ZoneGroupTopology/Control")).is_err());
    }

    #[test]
    fn soap_requests_keep_their_shape() {
        // What renderers that work today receive.
        let fake = FakeRenderer::start(|_| Reply::soap_ok(AV_TRANSPORT, "Play", &[]));
        play(&fake.url("/MediaRenderer/AVTransport/Control")).unwrap();
        let req = fake.next_request(Duration::from_secs(5)).unwrap();
        let port = fake.addr().port();
        let body = req.body_text();
        let head = String::from_utf8_lossy(&req.raw[..req.raw.len() - req.body.len()]).into_owned();
        assert_eq!(
            head,
            format!(
                "POST /MediaRenderer/AVTransport/Control HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                 Content-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\n\
                 SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#Play\"\r\nConnection: close\r\n\
                 User-Agent: stream-to-speaker/0.1\r\n\r\n",
                body.len()
            )
        );
        assert_eq!(req.soap_arg("Speed").as_deref(), Some("1"));
    }
}
