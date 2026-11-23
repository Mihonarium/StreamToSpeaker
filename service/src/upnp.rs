//! UPnP SOAP client — minimal, just the actions we need.
//!
//! Actions:
//!   * AVTransport: SetAVTransportURI, Play, Stop
//!   * RenderingControl: SetVolume, SetMute, GetVolume
//!   * ConnectionManager: GetProtocolInfo
//!   * GroupRenderingControl (Sonos): SetGroupVolume, GetGroupVolume,
//!     SetGroupMute — group-wide volume, addressed to the coordinator
//!   * ZoneGroupTopology (Sonos): GetZoneGroupState
//!
//! SOAP is just HTTP POST with a specific Content-Type + SOAPAction header
//! and an envelope-shaped XML body, sent with the app's own small client
//! (`device_http`).

use crate::device_http::{self, HttpError, Limits, Version};
use crate::discovery_net::shown;
use crate::stream_profile::{parse_protocol_list, ProtocolInfo, StreamProfile};
use anyhow::{anyhow, Result};
use log::debug;
use std::time::Duration;

/// Issue SetAVTransportURI to point the renderer at our HTTP stream URL,
/// within `limits` (a connect gives it [`TRANSPORT`]).
pub(crate) fn set_av_transport_uri(
    control_url: &str,
    stream_uri: &str,
    didl_metadata: &str,
    limits: &Limits,
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
    soap_post(control_url, AV_TRANSPORT, "SetAVTransportURI", &body, limits)?;
    Ok(())
}

/// Play, within the short limit: what Resync sends, from a thread a
/// person is waiting on.
pub fn play(control_url: &str) -> Result<()> {
    play_within(control_url, &SHORT)
}

/// Play within `limits` (a connect gives it [`TRANSPORT`]).
pub(crate) fn play_within(control_url: &str, limits: &Limits) -> Result<()> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID>
<Speed>1</Speed>
</u:Play>
</s:Body>
</s:Envelope>"#;
    soap_post(control_url, AV_TRANSPORT, "Play", body, limits)?;
    Ok(())
}

pub fn stop(control_url: &str) -> Result<()> {
    stop_within(control_url, &SHORT)
}

/// Stop within `limits`, at most the short limit's.
pub(crate) fn stop_within(control_url: &str, limits: &Limits) -> Result<()> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Stop xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
<InstanceID>0</InstanceID>
</u:Stop>
</s:Body>
</s:Envelope>"#;
    soap_post(control_url, AV_TRANSPORT, "Stop", body, &limits.within(SHORT.deadline))?;
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

/// The types a renderer says it plays: the `Sink` list of its
/// ConnectionManager's GetProtocolInfo, within `limits` (at most
/// [`PROTOCOL_LIST`]'s).
pub(crate) fn get_protocol_info(control_url: &str, limits: &Limits) -> Result<Vec<ProtocolInfo>> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1">
</u:GetProtocolInfo>
</s:Body>
</s:Envelope>"#;
    let limits = limits.within(PROTOCOL_LIST.deadline);
    let resp = soap_post(control_url, CONNECTION_MANAGER, "GetProtocolInfo", body, &limits)?;
    let doc = roxmltree::Document::parse(resp.trim_start_matches('\u{feff}').trim_start())
        .map_err(|e| anyhow!("GetProtocolInfo answer isn't XML: {e}"))?;
    let sink = doc
        .descendants()
        .find(|n| n.tag_name().name() == "Sink")
        .ok_or_else(|| anyhow!("GetProtocolInfo answer has no Sink"))?;
    Ok(parse_protocol_list(sink.text().unwrap_or("")))
}

/// Build the DIDL-Lite metadata blob for SetAVTransportURI, announcing
/// `stream_uri` with `profile`'s `protocolInfo` (for a WAV profile,
/// swyh-rs's `DLNA.ORG_PN=WAV;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=
/// 03700000…`, which Sonos takes).
///
/// Sonos will show "No Content" in the Now Playing card if only
/// `dc:title` is present — it expects at least a creator/artist to
/// treat the metadata as a real track. We populate both with the
/// product name so the card shows "Stream To Speaker" rather than a
/// blank placeholder.
pub fn didl_lite_metadata(stream_uri: &str, title: &str, initial_buffer_ms: u32, profile: StreamProfile) -> String {
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
<res protocolInfo="{info}">{uri}</res>
</item>
</DIDL-Lite>"#,
        title = title_e,
        info = xml_escape(&profile.protocol_info()),
        uri = xml_escape(stream_uri),
    )
}

// -----------------------------------------------------------------------------
// Low-level SOAP POST + helpers
// -----------------------------------------------------------------------------

const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const GROUP_RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:GroupRenderingControl:1";
const ZONE_GROUP_TOPOLOGY: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:1";
const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Largest SOAP answer accepted for most actions.
const MAX_SOAP_RESPONSE_BYTES: usize = 4 << 20;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Volume and mute, which someone is waiting on, Play and Stop on their
/// own (Resync, moving to another speaker), and the Stop sent before
/// connecting, which is best-effort.
pub(crate) const SHORT: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    deadline: Duration::from_secs(5),
    max_bytes: MAX_SOAP_RESPONSE_BYTES,
};

/// SetAVTransportURI and Play while connecting (`app::start_session`). A
/// renderer may fetch the stream before it answers them, and UPnP gives a
/// device 30 s to complete an action. Nothing else uses it: a connect runs
/// on a thread of its own, other calls may run where a person waits.
pub(crate) const TRANSPORT: Limits = Limits {
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

/// GetProtocolInfo: answered at once or not waited for, since what a
/// renderer lists only chooses among the app's own stream profiles, and
/// without it the original stream is offered first. Real lists are a few
/// KB.
pub(crate) const PROTOCOL_LIST: Limits = Limits {
    connect: Duration::from_secs(3),
    deadline: Duration::from_secs(3),
    max_bytes: 64 * 1024,
};

/// How much of a refusal's body is read for its fault (see
/// [`Fault::parse`]).
const FAULT_BYTES: usize = 64 * 1024;

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
    /// Internal Server Error`) and the UPnP error it gave, if any.
    Refused { status: String, fault: Option<Fault> },
}

/// The UPnP error a refusal carries ([UDA], the UPnP Device Architecture,
/// 3.2.5: `<UPnPError>` with `<errorCode>` and `<errorDescription>`). The
/// description is the device's own text, shown only through
/// [`Fault::describe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fault {
    pub code: Option<u32>,
    pub description: Option<String>,
}

impl Fault {
    /// The fault in a refusal's body, if the body is a SOAP fault. Only
    /// the first [`FAULT_BYTES`] are read: a real fault is well under 1 KB,
    /// and a device answering every refused key press with megabytes of
    /// markup should cost nothing to parse. A body the XML parser refuses
    /// (a control character in the description, say) still has its two
    /// elements read as text.
    fn parse(body: &[u8]) -> Option<Fault> {
        let text = String::from_utf8_lossy(&body[..body.len().min(FAULT_BYTES)]);
        let text = text.trim_start_matches('\u{feff}').trim_start();
        let (code, description) = match roxmltree::Document::parse(text) {
            Ok(doc) => {
                let err = doc.descendants().find(|n| n.tag_name().name() == "UPnPError")?;
                let child = |name: &str| {
                    err.children().find(|n| n.tag_name().name() == name).and_then(|n| n.text()).map(str::to_string)
                };
                (child("errorCode"), child("errorDescription"))
            }
            Err(_) => {
                let child = |name: &str| crate::gena::find_tag_text(text, name).map(|raw| decode_entities(&raw));
                (child("errorCode"), child("errorDescription"))
            }
        };
        let code = code.and_then(|c| c.trim().parse::<u32>().ok());
        let description = description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        (code.is_some() || description.is_some()).then_some(Fault { code, description })
    }

    /// `UPnP error 714 "Illegal MIME-type"`, or what of it there is. The
    /// description is the device's text: quoted, escaped and bounded by
    /// `discovery_net::shown`, as all device text is.
    fn describe(&self, separator: &str) -> String {
        match (self.code, &self.description) {
            (Some(code), Some(d)) => format!("UPnP error {code}{separator}{}", shown(d)),
            (Some(code), None) => format!("UPnP error {code}"),
            (None, Some(d)) => format!("UPnP error {}", shown(d)),
            (None, None) => "UPnP error".to_string(),
        }
    }
}

/// The character data of text an XML parser refused: the predefined
/// entities and character references decoded, everything else as it is.
fn decode_entities(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let decoded = match name {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl ActionError {
    /// The UPnP error code the device refused with, if it gave one.
    pub fn fault_code(&self) -> Option<u32> {
        match &self.failure {
            Failure::Refused { fault: Some(f), .. } => f.code,
            _ => None,
        }
    }

    /// What the person is told when connecting to `speaker` failed this
    /// way: what happened, the likely cause and what to do, then what the
    /// speaker itself said. `has_airplay_entry`: the same speaker is also
    /// in the list as an AirPlay entry, which may work where UPnP didn't.
    /// `formats_tried`: how many stream profiles it was offered.
    pub fn for_person(&self, speaker: &str, has_airplay_entry: bool, formats_tried: usize) -> String {
        let airplay = if has_airplay_entry { " Its AirPlay entry may work." } else { "" };
        match &self.failure {
            Failure::NoAnswer(HttpError::Connect(_)) => format!(
                "{speaker} can't be reached. Check that it's switched on and connected to the same network as this PC."
            ),
            Failure::NoAnswer(HttpError::NoAnswer(d)) => {
                format!("{speaker} didn't answer within {}. Try again in a moment.", seconds(*d))
            }
            Failure::NoAnswer(HttpError::BadUrl(why)) => {
                format!("{speaker} gave an address for its controls that Stream To Speaker can't use: {why}.")
            }
            Failure::NoAnswer(HttpError::Closed) => format!(
                "{speaker} closed the connection without answering. Try again in a moment; if it keeps happening, \
                 restart {speaker}."
            ),
            Failure::NoAnswer(_) => format!(
                "{speaker} sent an answer Stream To Speaker couldn't read. Try again in a moment; if it keeps \
                 happening, restart {speaker}."
            ),
            Failure::Refused { status, fault } => {
                let what = match self.fault_code() {
                    Some(714) if formats_tried > 1 => format!(
                        "{speaker} didn't take the stream in any of the {formats_tried} ways Stream To Speaker \
                         offered it.{airplay}"
                    ),
                    Some(714) => format!("{speaker} doesn't accept the audio format Stream To Speaker sent it.{airplay}"),
                    Some(716) => format!(
                        "{speaker} couldn't fetch the audio from this PC. Windows Firewall may be blocking it: \
                         allow Stream To Speaker on this network."
                    ),
                    // Busy (701, 715), locked (705), not ready (718): the
                    // transport's state, not the stream's.
                    Some(701 | 705 | 715 | 718) => {
                        format!("{speaker} isn't ready to take the stream. Try again in a moment.")
                    }
                    _ => format!("{speaker} refused the stream.{airplay}"),
                };
                let answered = match fault {
                    Some(f) => f.describe(", "),
                    None => status.clone(),
                };
                format!("{what} ({speaker} answered: {answered}.)")
            }
        }
    }
}

/// `d` for a message: "30 seconds", "1 second", "1.5 seconds".
pub(crate) fn seconds(d: Duration) -> String {
    let ms = d.as_millis();
    match ms {
        1000 => "1 second".to_string(),
        _ if ms % 1000 == 0 => format!("{} seconds", ms / 1000),
        _ => format!("{:.1} seconds", d.as_secs_f32()),
    }
}

/// The UPnP error code an action refused with, when `e` is such a refusal.
pub(crate) fn fault_code(e: &anyhow::Error) -> Option<u32> {
    e.downcast_ref::<ActionError>().and_then(ActionError::fault_code)
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failure {
            Failure::NoAnswer(e) => write!(f, "{}: {}", self.action, e),
            Failure::Refused { status, fault: None } => write!(f, "{} refused: {}", self.action, status),
            Failure::Refused { status, fault: Some(fault) } => {
                write!(f, "{} refused: {}, {}", self.action, status, fault.describe(" "))
            }
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
        return Err(fail(Failure::Refused { status: resp.status_text(), fault: Fault::parse(&resp.body) }));
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

    /// The metadata every UPnP speaker was given before stream profiles,
    /// and every Sonos still is: byte for byte.
    const LEGACY_DIDL: &str = concat!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#,
        "\n",
        r#"<item id="1" parentID="0" restricted="0">"#,
        "\n<dc:title>Stream &amp; Speaker</dc:title>\n<dc:creator>Stream &amp; Speaker</dc:creator>\n",
        "<upnp:artist>Stream &amp; Speaker</upnp:artist>\n<upnp:album>Live audio from this PC</upnp:album>\n",
        "<upnp:class>object.item.audioItem.musicTrack</upnp:class>\n",
        r#"<res protocolInfo="http-get:*:audio/wav:DLNA.ORG_PN=WAV;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=03700000000000000000000000000000">http://192.168.1.5:5901/stream.raw?a=1&amp;b=2</res>"#,
        "\n</item>\n</DIDL-Lite>",
    );

    #[test]
    fn the_legacy_metadata_keeps_its_bytes() {
        let didl = didl_lite_metadata("http://192.168.1.5:5901/stream.raw?a=1&b=2", "Stream & Speaker", 0, StreamProfile::Legacy);
        assert_eq!(didl, LEGACY_DIDL);
    }

    #[test]
    fn the_metadata_names_the_profiles_own_type() {
        use crate::stream_profile::ALL;
        for p in ALL {
            let didl = didl_lite_metadata("http://192.168.1.5:5901/x", "T", 0, p);
            let doc = roxmltree::Document::parse(&didl).unwrap();
            let res = doc.descendants().find(|n| n.tag_name().name() == "res").unwrap();
            assert_eq!(res.attribute("protocolInfo"), Some(p.protocol_info().as_str()), "{p:?}");
            assert_eq!(res.text(), Some("http://192.168.1.5:5901/x"));
        }
        let l16 = didl_lite_metadata("http://h/l16/stream.raw", "T", 0, StreamProfile::L16);
        assert!(l16.contains(r#"protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM""#), "{l16}");
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
        set_av_transport_uri(&fake.url("/AVTransport/Control"), "http://127.0.0.1:1/stream.raw", "", &TRANSPORT)
            .expect("a slow answer is still an answer");
    }

    #[test]
    fn play_on_its_own_gives_up_at_the_short_limit() {
        // Resync sends Play from the window's thread: a speaker that never
        // answers must not hold it for the connect's long limit.
        let fake = FakeRenderer::start(|_| Reply::silent());
        let start = Instant::now();
        let e = play(&fake.url("/AVTransport/Control")).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(15), "{:?}", start.elapsed());
        assert!(format!("{e:#}").contains("no answer within 5 s"), "{e:#}");
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

    // ---- refusals ------------------------------------------------------

    /// What SetAVTransportURI fails with when the renderer answers `reply`.
    fn refusal(reply: Reply) -> anyhow::Error {
        let fake = FakeRenderer::start(move |_| reply.clone());
        set_av_transport_uri(&fake.url("/AVTransport/Control"), "http://127.0.0.1:1/stream.raw", "", &TRANSPORT)
            .expect_err("refused")
    }

    fn action_error(e: &anyhow::Error) -> &ActionError {
        e.downcast_ref::<ActionError>().expect("an ActionError")
    }

    #[test]
    fn a_refusal_carries_the_renderers_error_code_and_text() {
        let e = refusal(Reply::soap_fault(714, "Illegal MIME-type"));
        assert_eq!(action_error(&e).fault_code(), Some(714));
        let line = format!("{e:#}");
        assert_eq!(
            line,
            "SetAVTransportURI refused: HTTP 500 \"Internal Server Error\", UPnP error 714 \"Illegal MIME-type\""
        );
    }

    #[test]
    fn the_renderers_own_text_is_bounded_and_escaped() {
        let hostile = format!("Evil\u{1b}[2J\r\nnext \"line\"\u{202e}{}", "A".repeat(10_000));
        let e = refusal(Reply::soap_fault(402, &hostile));
        let line = format!("{e:#}");
        let unsafe_char = |c: char| c.is_control() || c == '\u{202e}';
        assert!(!line.chars().any(unsafe_char), "{line:?}");
        assert!(line.chars().count() < 330, "{} chars: {line}", line.chars().count());
        assert!(line.contains(r#"UPnP error 402 "Evil\u{1b}[2J\r\nnext \"line\"\u{202e}AAAA"#), "{line}");
        assert!(line.ends_with("…\""), "{line}");
        let told = action_error(&e).for_person("Kitchen", false, 1);
        assert!(!told.chars().any(unsafe_char) && told.chars().count() < 360, "{told}");
    }

    #[test]
    fn a_fault_is_read_from_the_start_of_the_answer_only() {
        let padded = |pad: usize| {
            format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>{}<s:Fault><detail>\
                 <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>714</errorCode>\
                 <errorDescription>Illegal MIME-type</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
                "<x/>".repeat(pad)
            )
        };
        // Up front, with a large tail after it: read.
        let early = format!("{}{}", padded(0), " ".repeat(1 << 20));
        assert_eq!(Fault::parse(early.as_bytes()).and_then(|f| f.code), Some(714));
        // Past the first 64 KiB: not looked for.
        let late = padded(20_000);
        assert_eq!(Fault::parse(late.as_bytes()), None);
    }

    #[test]
    fn a_refusal_without_a_fault_names_the_status() {
        for body in ["<html><body>Oops</body></html>", "", "<s:Envelope><s:Body><s:Fault/></s:Body></s:Envelope>"] {
            let e = refusal(Reply::status(500, "Internal Server Error", &[], body));
            assert_eq!(action_error(&e).fault_code(), None, "{body}");
            assert_eq!(format!("{e:#}"), "SetAVTransportURI refused: HTTP 500 \"Internal Server Error\"");
        }
    }

    #[test]
    fn the_person_is_told_what_happened_and_what_to_do() {
        let told = |reply: Reply, airplay: bool| action_error(&refusal(reply)).for_person("Kitchen", airplay, 1);
        assert_eq!(
            told(Reply::soap_fault(714, "Illegal MIME-type"), true),
            "Kitchen doesn't accept the audio format Stream To Speaker sent it. Its AirPlay entry may work. \
             (Kitchen answered: UPnP error 714, \"Illegal MIME-type\".)"
        );
        assert_eq!(
            told(Reply::soap_fault(714, "Illegal MIME-type"), false),
            "Kitchen doesn't accept the audio format Stream To Speaker sent it. \
             (Kitchen answered: UPnP error 714, \"Illegal MIME-type\".)"
        );
        assert_eq!(
            told(Reply::soap_fault(716, "Resource not found"), false),
            "Kitchen couldn't fetch the audio from this PC. Windows Firewall may be blocking it: allow \
             Stream To Speaker on this network. (Kitchen answered: UPnP error 716, \"Resource not found\".)"
        );
        for code in [701, 705, 715, 718] {
            assert_eq!(
                told(Reply::soap_fault(code, "Busy"), false),
                format!(
                    "Kitchen isn't ready to take the stream. Try again in a moment. \
                     (Kitchen answered: UPnP error {code}, \"Busy\".)"
                )
            );
        }
        assert_eq!(
            told(Reply::soap_fault(402, ""), true),
            "Kitchen refused the stream. Its AirPlay entry may work. (Kitchen answered: UPnP error 402.)"
        );
        assert_eq!(
            told(Reply::status(503, "Service Unavailable", &[], ""), false),
            "Kitchen refused the stream. (Kitchen answered: HTTP 503 \"Service Unavailable\".)"
        );
        // Offered several ways: counted as ways, not as formats.
        assert_eq!(
            action_error(&refusal(Reply::soap_fault(714, "Illegal MIME-type"))).for_person("Kitchen", false, 3),
            "Kitchen didn't take the stream in any of the 3 ways Stream To Speaker offered it. \
             (Kitchen answered: UPnP error 714, \"Illegal MIME-type\".)"
        );
    }

    #[test]
    fn an_answer_that_never_came_or_couldnt_be_read_says_what_to_try() {
        let told = |reply: Reply| action_error(&refusal(reply)).for_person("Kitchen", false, 1);
        assert_eq!(
            told(Reply::raw("")),
            "Kitchen closed the connection without answering. Try again in a moment; if it keeps happening, \
             restart Kitchen."
        );
        assert_eq!(
            told(Reply::raw("SIP/2.0 200 OK\r\n\r\n")),
            "Kitchen sent an answer Stream To Speaker couldn't read. Try again in a moment; if it keeps \
             happening, restart Kitchen."
        );
    }

    #[test]
    fn no_answer_and_no_connection_are_told_apart() {
        let fake = FakeRenderer::start(|_| Reply::silent());
        let quick = Limits { deadline: Duration::from_millis(200), ..TRANSPORT };
        let e = soap_post(&fake.url("/c"), AV_TRANSPORT, "Play", "", &quick).unwrap_err();
        assert_eq!(e.fault_code(), None);
        assert_eq!(e.for_person("Kitchen", true, 1), "Kitchen didn't answer within 0.2 seconds. Try again in a moment.");

        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = play(&format!("http://127.0.0.1:{port}/c")).unwrap_err();
        assert_eq!(
            action_error(&e).for_person("Kitchen", true, 1),
            "Kitchen can't be reached. Check that it's switched on and connected to the same network as this PC."
        );
    }

    #[test]
    fn the_types_a_renderer_plays_come_from_its_sink_list() {
        use crate::fake_renderer::CONNECTION_MANAGER;
        use crate::stream_profile::ProtocolInfo;
        let fake = FakeRenderer::start(|_| {
            Reply::soap_ok(
                CONNECTION_MANAGER,
                "GetProtocolInfo",
                &[("Source", "http-get:*:audio/mpeg:*"), ("Sink", "http-get:*:audio/wav:*,http-get:*:audio/L16;rate=44100;channels=2:*")],
            )
        });
        let sinks = get_protocol_info(&fake.url("/ConnectionManager/Control"), &PROTOCOL_LIST).unwrap();
        let info = |f: &str| ProtocolInfo { protocol: "http-get".into(), content_format: f.into() };
        assert_eq!(sinks, [info("audio/wav"), info("audio/L16;rate=44100;channels=2")]);
        assert_eq!(fake.soap_actions(), ["GetProtocolInfo"]);
        // A list too large for any real device is refused, not read.
        let big = format!("http-get:*:audio/wav:*,{}", "x".repeat(70 * 1024));
        let fake = FakeRenderer::start(move |_| Reply::soap_ok(CONNECTION_MANAGER, "GetProtocolInfo", &[("Sink", &big)]));
        assert!(get_protocol_info(&fake.url("/ConnectionManager/Control"), &PROTOCOL_LIST).is_err());
        // No Sink in the answer.
        let fake = FakeRenderer::start(|_| Reply::soap_ok(CONNECTION_MANAGER, "GetProtocolInfo", &[]));
        assert!(get_protocol_info(&fake.url("/ConnectionManager/Control"), &PROTOCOL_LIST).is_err());
    }

    #[test]
    fn a_renderer_slow_to_list_its_types_is_not_waited_for_long() {
        let fake = FakeRenderer::start(|_| Reply::silent());
        let start = Instant::now();
        assert!(get_protocol_info(&fake.url("/ConnectionManager/Control"), &PROTOCOL_LIST).is_err());
        assert!(start.elapsed() < Duration::from_secs(15), "{:?}", start.elapsed());
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
