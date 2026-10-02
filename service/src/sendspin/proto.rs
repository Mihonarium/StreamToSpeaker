//! Sendspin application messages, in the two wire dialects found in the
//! field.
//!
//! * [`Dialect::V9`] — aiosendspin 9.x, i.e. what Music Assistant 2.10
//!   ships (and the reference Python player): pairing methods
//!   `dynamic_pin` / `static_pin` / `pairing_psk` listed as an array of
//!   descriptors, `trust_level` in `client/hello`, `client_stream/*`,
//!   `static_delay_ms`, 9-byte binary audio header, hellos re-exchanged
//!   after a re-handshake, Noise message 1 without `psk_category`.
//! * [`Dialect::Spec`] — the current specification text (sendspin-cpp,
//!   newer ESPHome firmware): `dynamic_pairing_code` / `static_pairing_code`
//!   keyed in an object, `client-stream/*`, `output_delay_ms`, 13-byte
//!   audio header carrying `send_ahead`, `psk_category` in message 1.
//!
//! Detection: a client sees `psk_category` in Noise message 1 only from a
//! current-spec server; a server sees the client's dialect from the shape
//! of `supported_pair_methods` in `client/hello` (array vs object). The
//! unencrypted pre-pairing wire ("legacy", aiosendspin ≤ 6) is a third
//! shape on the server side only; it uses the V9 binary layout.

use serde_json::{json, Map, Value};

use super::channel::envelope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    V9,
    Spec,
}

pub const BINARY_AUDIO_CHUNK: u8 = 4;
pub const BINARY_SOURCE_CHUNK: u8 = 12;

/// Pairing methods, named per dialect on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PairMethod {
    PairingPsk,
    DynamicCode,
    StaticCode,
}

impl PairMethod {
    pub fn wire(self, d: Dialect) -> &'static str {
        match (self, d) {
            (PairMethod::PairingPsk, _) => "pairing_psk",
            (PairMethod::DynamicCode, Dialect::V9) => "dynamic_pin",
            (PairMethod::StaticCode, Dialect::V9) => "static_pin",
            (PairMethod::DynamicCode, Dialect::Spec) => "dynamic_pairing_code",
            (PairMethod::StaticCode, Dialect::Spec) => "static_pairing_code",
        }
    }

    /// Accepts either dialect's name.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "pairing_psk" => Some(PairMethod::PairingPsk),
            "dynamic_pin" | "dynamic_pairing_code" => Some(PairMethod::DynamicCode),
            "static_pin" | "static_pairing_code" => Some(PairMethod::StaticCode),
            _ => None,
        }
    }
}

/// One audio format (codec + PCM shape).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
}

impl AudioFormat {
    pub fn to_json(&self) -> Value {
        json!({
            "codec": self.codec,
            "sample_rate": self.sample_rate,
            "channels": self.channels,
            "bit_depth": self.bit_depth,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            codec: v.get("codec")?.as_str()?.to_string(),
            sample_rate: u32::try_from(v.get("sample_rate")?.as_u64()?).ok()?,
            channels: u16::try_from(v.get("channels")?.as_u64()?).ok()?,
            bit_depth: v.get("bit_depth").and_then(Value::as_u64).and_then(|b| u16::try_from(b).ok()).unwrap_or(16),
        })
    }
}

// ---------------------------------------------------------------------------
// Client role (we are a source talking to a server)
// ---------------------------------------------------------------------------

pub struct ClientHelloParams<'a> {
    pub name: &'a str,
    pub product_name: &'a str,
    pub manufacturer: &'a str,
    pub software_version: &'a str,
    pub paired: bool,
    /// Offer the dynamic code method (shown on our screen).
    pub offer_dynamic_code: bool,
    pub min_code_length: u32,
}

pub fn client_hello_source(d: Dialect, p: &ClientHelloParams<'_>) -> Value {
    let mut payload = Map::new();
    payload.insert("name".into(), json!(p.name));
    payload.insert(
        "device_info".into(),
        json!({
            "product_name": p.product_name,
            "manufacturer": p.manufacturer,
            "software_version": p.software_version,
        }),
    );
    payload.insert("supported_roles".into(), json!(["source@v1"]));
    payload.insert("source@v1_support".into(), json!({ "features": { "line_sense": true } }));
    payload.insert("unpaired_access".into(), json!({ "enabled": false }));
    match d {
        Dialect::V9 => {
            payload.insert("trust_level".into(), json!(if p.paired { "user" } else { "none" }));
            let mut methods = vec![json!({ "method": "pairing_psk" })];
            if p.offer_dynamic_code {
                methods.push(json!({
                    "method": "dynamic_pin",
                    "out_channels": ["display"],
                    "min_pin_length": p.min_code_length,
                }));
            }
            payload.insert("supported_pair_methods".into(), Value::Array(methods));
        }
        Dialect::Spec => {
            let mut methods = Map::new();
            methods.insert("pairing_psk".into(), json!({}));
            if p.offer_dynamic_code {
                methods.insert(
                    "dynamic_pairing_code".into(),
                    json!({ "out_channels": ["display"], "formats": ["digits"] }),
                );
            }
            payload.insert("supported_pair_methods".into(), Value::Object(methods));
        }
    }
    envelope("client/hello", Value::Object(payload))
}

pub fn client_time(client_transmitted_us: i64) -> Value {
    envelope("client/time", json!({ "client_transmitted": client_transmitted_us }))
}

/// `client/state` for a source: availability + optional signal presence.
pub fn client_state_source(available: bool, signal: Option<bool>) -> Value {
    let mut source = Map::new();
    if let Some(s) = signal {
        source.insert("signal".into(), json!(if s { "present" } else { "absent" }));
    }
    envelope("client/state", json!({ "available": available, "source": Value::Object(source) }))
}

pub fn client_stream_start(d: Dialect, fmt: &AudioFormat, codec_header_b64: Option<&str>) -> Value {
    let mut src = match fmt.to_json() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Some(h) = codec_header_b64 {
        src.insert("codec_header".into(), json!(h));
    }
    let ty = match d {
        Dialect::V9 => "client_stream/start",
        Dialect::Spec => "client-stream/start",
    };
    envelope(ty, json!({ "source": Value::Object(src) }))
}

pub fn client_stream_end(d: Dialect) -> Value {
    let ty = match d {
        Dialect::V9 => "client_stream/end",
        Dialect::Spec => "client-stream/end",
    };
    envelope(ty, json!({}))
}

pub fn client_goodbye(reason: &str) -> Value {
    envelope("client/goodbye", json!({ "reason": reason }))
}

/// Binary source chunk: `[12][timestamp i64 BE][audio]`.
pub fn source_chunk(timestamp_us: i64, audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + audio.len());
    out.push(BINARY_SOURCE_CHUNK);
    out.extend_from_slice(&timestamp_us.to_be_bytes());
    out.extend_from_slice(audio);
    out
}

/// A parsed `server/activate`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activation {
    pub playback: bool,
    pub pairing: bool,
    /// Unknown activity names (e.g. `management`); rejected for us.
    pub other_activities: Vec<String>,
    /// `None` = omitted (keep the previous set).
    pub active_roles: Option<Vec<String>>,
    pub pairing_method: Option<PairMethod>,
    /// V9 dynamic PIN length.
    pub pin_length: Option<u32>,
    /// Spec dynamic code emission format.
    pub format: Option<String>,
}

pub fn parse_activation(payload: &Value) -> Activation {
    let mut a = Activation::default();
    if let Some(list) = payload.get("activities").and_then(Value::as_array) {
        for act in list.iter().filter_map(Value::as_str) {
            match act {
                "playback" => a.playback = true,
                "pairing" => a.pairing = true,
                other => a.other_activities.push(other.to_string()),
            }
        }
    }
    a.active_roles = payload
        .get("active_roles")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).map(str::to_string).collect());
    if let Some(p) = payload.get("pairing") {
        a.pairing_method = p.get("method").and_then(Value::as_str).and_then(PairMethod::from_wire);
        a.pin_length = p.get("pin_length").and_then(Value::as_u64).map(|n| n as u32);
        a.format = p.get("format").and_then(Value::as_str).map(str::to_string);
    }
    a
}

// ---------------------------------------------------------------------------
// Server role (we are the server talking to a player)
// ---------------------------------------------------------------------------

pub fn server_hello(name: &str) -> Value {
    envelope("server/hello", json!({ "name": name }))
}

/// Pre-encryption wire: `server/hello` carries identity + roles directly.
pub fn legacy_server_hello(server_id: &str, name: &str, active_roles: &[String]) -> Value {
    envelope(
        "server/hello",
        json!({
            "server_id": server_id,
            "name": name,
            "version": 1,
            "active_roles": active_roles,
            "connection_reason": "playback",
        }),
    )
}

pub fn server_activate(d: Dialect, playback: bool, pairing: Option<(PairMethod, Option<u32>)>, active_roles: Option<&[String]>) -> Value {
    let mut activities = Vec::new();
    if playback {
        activities.push("playback");
    }
    if pairing.is_some() {
        activities.push("pairing");
    }
    let mut p = Map::new();
    p.insert("activities".into(), json!(activities));
    if let Some(r) = active_roles {
        p.insert("active_roles".into(), json!(r));
    }
    if let Some((m, pin_length)) = pairing {
        let mut obj = Map::new();
        obj.insert("method".into(), json!(m.wire(d)));
        if m == PairMethod::DynamicCode {
            match d {
                Dialect::V9 => {
                    obj.insert("pin_length".into(), json!(pin_length.unwrap_or(6)));
                }
                Dialect::Spec => {
                    obj.insert("format".into(), json!("digits"));
                }
            }
        }
        p.insert("pairing".into(), Value::Object(obj));
    }
    envelope("server/activate", Value::Object(p))
}

pub fn server_time(client_transmitted: i64, server_received: i64, server_transmitted: i64) -> Value {
    envelope(
        "server/time",
        json!({
            "client_transmitted": client_transmitted,
            "server_received": server_received,
            "server_transmitted": server_transmitted,
        }),
    )
}

pub fn stream_start_player(fmt: &AudioFormat, codec_header_b64: Option<&str>, server_transmitted: i64) -> Value {
    let mut player = match fmt.to_json() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Some(h) = codec_header_b64 {
        player.insert("codec_header".into(), json!(h));
    }
    envelope(
        "stream/start",
        json!({ "server_transmitted": server_transmitted, "player": Value::Object(player) }),
    )
}

pub fn stream_end_player(server_transmitted: i64) -> Value {
    envelope("stream/end", json!({ "server_transmitted": server_transmitted, "roles": ["player"] }))
}

pub fn group_update(playing: bool, group_id: &str, group_name: &str) -> Value {
    envelope(
        "group/update",
        json!({
            "playback_state": if playing { "playing" } else { "stopped" },
            "group_id": group_id,
            "group_name": group_name,
        }),
    )
}

pub fn player_command_volume(volume: u32) -> Value {
    envelope("server/command", json!({ "player": { "command": "volume", "volume": volume.min(100) } }))
}

pub fn player_command_mute(muted: bool) -> Value {
    envelope("server/command", json!({ "player": { "command": "mute", "mute": muted } }))
}

/// `server/state` metadata (title/artist/album only — what the OS media
/// session gives us). An empty field is sent as `null` on V9 (where an
/// omitted field means "unchanged") and omitted on Spec (where every
/// message carries the full state, so absent means "none").
pub fn server_state_metadata(d: Dialect, timestamp_us: i64, title: Option<&str>, artist: Option<&str>, album: Option<&str>) -> Value {
    let mut m = Map::new();
    m.insert("timestamp".into(), json!(timestamp_us));
    for (k, v) in [("title", title), ("artist", artist), ("album", album)] {
        match (v.filter(|s| !s.is_empty()), d) {
            (Some(s), _) => {
                m.insert(k.into(), json!(s));
            }
            (None, Dialect::V9) => {
                m.insert(k.into(), Value::Null);
            }
            (None, Dialect::Spec) => {}
        }
    }
    envelope("server/state", json!({ "metadata": Value::Object(m) }))
}

/// Binary audio chunk for a player. V9: `[4][ts i64]`; Spec adds
/// `[send_ahead u32]` (µs from transmission to `timestamp`, saturating).
pub fn audio_chunk(d: Dialect, timestamp_us: i64, now_us: i64, audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + audio.len());
    out.push(BINARY_AUDIO_CHUNK);
    out.extend_from_slice(&timestamp_us.to_be_bytes());
    if d == Dialect::Spec {
        let ahead = timestamp_us.saturating_sub(now_us);
        let ahead = if ahead <= 0 { 0 } else { ahead.min(u32::MAX as i64) as u32 };
        out.extend_from_slice(&ahead.to_be_bytes());
    }
    out.extend_from_slice(audio);
    out
}

/// What a player told us in `client/hello`.
#[derive(Clone, Debug, Default)]
pub struct PlayerHello {
    pub dialect: Option<Dialect>,
    pub name: String,
    pub client_id: Option<String>,
    pub roles: Vec<String>,
    pub formats: Vec<AudioFormat>,
    pub buffer_capacity: u64,
    /// `volume` / `mute` settable (V9 lists these in the hello).
    pub hello_commands: Vec<String>,
    pub pair_methods: Vec<PairMethod>,
    pub unpaired_access: bool,
    pub product_name: Option<String>,
    pub manufacturer: Option<String>,
}

pub fn parse_player_hello(payload: &Value) -> PlayerHello {
    let mut h = PlayerHello {
        name: payload.get("name").and_then(Value::as_str).unwrap_or("Sendspin player").to_string(),
        client_id: payload.get("client_id").and_then(Value::as_str).map(str::to_string),
        ..Default::default()
    };
    h.roles = payload
        .get("supported_roles")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let support = payload.get("player@v1_support").or_else(|| payload.get("player_support"));
    if let Some(s) = support {
        h.formats = s
            .get("supported_formats")
            .and_then(Value::as_array)
            .map(|f| f.iter().filter_map(AudioFormat::from_json).collect())
            .unwrap_or_default();
        h.buffer_capacity = s.get("buffer_capacity").and_then(Value::as_u64).unwrap_or(0);
        h.hello_commands = s
            .get("supported_commands")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
    }
    match payload.get("supported_pair_methods") {
        Some(Value::Array(list)) => {
            h.dialect = Some(Dialect::V9);
            h.pair_methods = list
                .iter()
                .filter_map(|d| d.get("method").and_then(Value::as_str))
                .filter_map(PairMethod::from_wire)
                .collect();
        }
        Some(Value::Object(map)) => {
            h.dialect = Some(Dialect::Spec);
            h.pair_methods = map.keys().filter_map(|k| PairMethod::from_wire(k)).collect();
        }
        _ => {}
    }
    if h.dialect.is_none() && payload.get("trust_level").is_some() {
        h.dialect = Some(Dialect::V9);
    }
    h.unpaired_access = payload
        .get("unpaired_access")
        .and_then(|u| u.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(di) = payload.get("device_info") {
        h.product_name = di.get("product_name").and_then(Value::as_str).map(str::to_string);
        h.manufacturer = di.get("manufacturer").and_then(Value::as_str).map(str::to_string);
    }
    h
}

/// Player fields of a `client/state` (any dialect); `None` = not present.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerState {
    pub available: Option<bool>,
    pub volume: Option<u32>,
    pub muted: Option<bool>,
    pub output_delay_ms: Option<u32>,
    pub required_lead_time_ms: Option<u32>,
    pub min_buffer_ms: Option<u32>,
    pub state_commands: Option<Vec<String>>,
}

pub fn parse_player_state(payload: &Value) -> PlayerState {
    let mut s = PlayerState::default();
    s.available = payload.get("available").and_then(Value::as_bool);
    if s.available.is_none() {
        // Older clients: `state: "synchronized"` (client level or in player).
        let st = payload
            .get("state")
            .or_else(|| payload.get("player").and_then(|p| p.get("state")))
            .and_then(Value::as_str);
        if let Some(st) = st {
            s.available = Some(st == "synchronized");
        }
    }
    if let Some(p) = payload.get("player") {
        let u = |k: &str| p.get(k).and_then(Value::as_u64).map(|v| v as u32);
        s.volume = u("volume");
        s.muted = p.get("muted").and_then(Value::as_bool);
        s.output_delay_ms = u("output_delay_ms").or_else(|| u("static_delay_ms"));
        s.required_lead_time_ms = u("required_lead_time_ms");
        s.min_buffer_ms = u("min_buffer_ms");
        s.state_commands = p
            .get("supported_commands")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(Value::as_str).map(str::to_string).collect());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ClientHelloParams<'static> {
        ClientHelloParams {
            name: "PC",
            product_name: "Stream To Speaker",
            manufacturer: "Stream To Speaker",
            software_version: "0.1",
            paired: false,
            offer_dynamic_code: true,
            min_code_length: 6,
        }
    }

    #[test]
    fn source_hello_shapes_per_dialect() {
        let v9 = client_hello_source(Dialect::V9, &params());
        let p = &v9["payload"];
        assert_eq!(v9["type"], "client/hello");
        assert_eq!(p["supported_roles"], json!(["source@v1"]));
        assert_eq!(p["trust_level"], "none");
        assert!(p["supported_pair_methods"].is_array());
        assert_eq!(p["supported_pair_methods"][1]["method"], "dynamic_pin");
        assert_eq!(p["source@v1_support"]["features"]["line_sense"], true);
        let spec = client_hello_source(Dialect::Spec, &params());
        let p = &spec["payload"];
        assert!(p.get("trust_level").is_none());
        assert!(p["supported_pair_methods"]["dynamic_pairing_code"].is_object());
        // Server side recognises both shapes.
        assert_eq!(parse_player_hello(&v9["payload"]).dialect, Some(Dialect::V9));
        assert_eq!(parse_player_hello(&spec["payload"]).dialect, Some(Dialect::Spec));
        assert_eq!(
            parse_player_hello(&spec["payload"]).pair_methods.contains(&PairMethod::DynamicCode),
            true
        );
    }

    #[test]
    fn stream_message_names_per_dialect() {
        let f = AudioFormat { codec: "pcm".into(), sample_rate: 44100, channels: 2, bit_depth: 16 };
        assert_eq!(client_stream_start(Dialect::V9, &f, None)["type"], "client_stream/start");
        assert_eq!(client_stream_start(Dialect::Spec, &f, None)["type"], "client-stream/start");
        assert_eq!(client_stream_end(Dialect::V9)["type"], "client_stream/end");
        let s = client_stream_start(Dialect::V9, &f, Some("AAAA"));
        assert_eq!(s["payload"]["source"]["sample_rate"], 44100);
        assert_eq!(s["payload"]["source"]["codec_header"], "AAAA");
    }

    #[test]
    fn binary_layouts() {
        let c = source_chunk(0x0102030405060708, &[9, 9]);
        assert_eq!(c, vec![12, 1, 2, 3, 4, 5, 6, 7, 8, 9, 9]);
        let a = audio_chunk(Dialect::V9, 1_000_000, 0, &[1]);
        assert_eq!(a.len(), 1 + 8 + 1);
        let a = audio_chunk(Dialect::Spec, 1_000_000, 400_000, &[1]);
        assert_eq!(a.len(), 1 + 8 + 4 + 1);
        assert_eq!(u32::from_be_bytes(a[9..13].try_into().unwrap()), 600_000);
        // Late chunk: send_ahead saturates at 0.
        let a = audio_chunk(Dialect::Spec, 10, 20, &[]);
        assert_eq!(&a[9..13], &[0, 0, 0, 0]);
    }

    #[test]
    fn activation_parsing() {
        let a = parse_activation(&json!({
            "activities": ["pairing"],
            "active_roles": [],
            "pairing": {"method": "dynamic_pin", "pin_length": 6}
        }));
        assert!(a.pairing && !a.playback);
        assert_eq!(a.pairing_method, Some(PairMethod::DynamicCode));
        assert_eq!(a.pin_length, Some(6));
        assert_eq!(a.active_roles, Some(vec![]));
        let b = parse_activation(&json!({"activities": ["playback", "management"]}));
        assert!(b.playback);
        assert_eq!(b.other_activities, vec!["management".to_string()]);
        assert_eq!(b.active_roles, None);
    }

    #[test]
    fn server_activate_shapes() {
        let v = server_activate(Dialect::V9, false, Some((PairMethod::DynamicCode, Some(6))), Some(&[]));
        assert_eq!(v["payload"]["pairing"]["method"], "dynamic_pin");
        assert_eq!(v["payload"]["pairing"]["pin_length"], 6);
        let v = server_activate(Dialect::Spec, false, Some((PairMethod::DynamicCode, None)), Some(&[]));
        assert_eq!(v["payload"]["pairing"]["method"], "dynamic_pairing_code");
        assert_eq!(v["payload"]["pairing"]["format"], "digits");
        let roles = vec!["player@v1".to_string()];
        let v = server_activate(Dialect::V9, true, None, Some(&roles));
        assert_eq!(v["payload"]["activities"], json!(["playback"]));
        assert!(v["payload"].get("pairing").is_none());
    }

    #[test]
    fn player_state_both_dialects() {
        let v9 = parse_player_state(&json!({
            "available": true,
            "player": {"volume": 40, "muted": false, "static_delay_ms": 20, "required_lead_time_ms": 250, "min_buffer_ms": 1000}
        }));
        assert_eq!(v9.output_delay_ms, Some(20));
        assert_eq!(v9.volume, Some(40));
        let spec = parse_player_state(&json!({
            "available": false,
            "player": {"output_delay_ms": 5, "required_lead_time_ms": 0, "min_buffer_ms": 300, "supported_commands": ["volume"]}
        }));
        assert_eq!(spec.available, Some(false));
        assert_eq!(spec.output_delay_ms, Some(5));
        assert_eq!(spec.state_commands, Some(vec!["volume".to_string()]));
        let legacy = parse_player_state(&json!({"player": {"state": "synchronized", "volume": 10}}));
        assert_eq!(legacy.available, Some(true));
    }

    #[test]
    fn metadata_clears_per_dialect() {
        let v9 = server_state_metadata(Dialect::V9, 5, Some("T"), None, Some(""));
        assert_eq!(v9["payload"]["metadata"]["title"], "T");
        assert!(v9["payload"]["metadata"]["artist"].is_null());
        assert!(v9["payload"]["metadata"].get("artist").is_some());
        let spec = server_state_metadata(Dialect::Spec, 5, Some("T"), None, None);
        assert!(spec["payload"]["metadata"].get("artist").is_none());
        assert_eq!(spec["payload"]["metadata"]["timestamp"], 5);
    }

    #[test]
    fn player_hello_formats() {
        let h = parse_player_hello(&json!({
            "name": "Kitchen",
            "supported_roles": ["player@v1", "metadata@v1"],
            "player@v1_support": {
                "supported_formats": [
                    {"codec": "flac", "channels": 2, "sample_rate": 48000, "bit_depth": 16},
                    {"codec": "pcm", "channels": 2, "sample_rate": 44100, "bit_depth": 16}
                ],
                "buffer_capacity": 1000000,
                "supported_commands": ["volume", "mute"]
            },
            "supported_pair_methods": [{"method": "pairing_psk"}],
            "unpaired_access": {"enabled": true}
        }));
        assert_eq!(h.formats.len(), 2);
        assert_eq!(h.formats[1].sample_rate, 44100);
        assert!(h.unpaired_access);
        assert_eq!(h.hello_commands, vec!["volume", "mute"]);
    }
}
