//! Connection bring-up: the cleartext `client/init` / `server/init`
//! exchange, the two `noise/handshake` messages, and the in-band
//! re-handshake used after pairing.
//!
//! ```text
//! client → server  client/init      {client_id, version: 1, suite}   (text)
//! server → client  server/init      {server_id, version: 1}          (text)
//! server → client  noise/handshake  {data: b64url(msg1)}             (text)
//! client → server  noise/handshake  {data: b64url(msg2)}             (text)
//! … transport mode: binary Noise frames …
//! ```
//!
//! The prologue is the exact bytes of `client/init` followed by the exact
//! bytes of `server/init` as they crossed the wire. Message 1's payload is
//! `{"psk_id": …}` (+ `"psk_category"` on current-spec servers); message 2's
//! is the literal `{}`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::channel::{self, envelope, msg_type, payload, ChannelReader, ChannelWriter, Incoming};
use super::keys::{self, b64url, b64url_decode, peer_id_to_key, PskCategory};
use super::noise::{x25519_public, Handshake, Suite};
use super::ws::{WsMessage, WsReader, WsWriter};

pub const PROTOCOL_VERSION: i64 = 1;

/// A PSK chosen for (or matched by) a handshake.
#[derive(Clone, Debug)]
pub struct ResolvedPsk {
    pub psk: [u8; 32],
    pub category: PskCategory,
    pub psk_id: String,
}

impl ResolvedPsk {
    pub fn new(psk: [u8; 32], category: PskCategory) -> Self {
        Self {
            psk_id: keys::psk_id(&psk),
            psk,
            category,
        }
    }

    pub fn sentinel() -> Self {
        Self::new(keys::sentinel_psk(), PskCategory::Sentinel)
    }
}

/// What a client learns from a completed handshake.
pub struct ClientHandshake {
    pub reader: ChannelReader,
    pub writer: Arc<ChannelWriter>,
    pub server_id: String,
    pub psk: ResolvedPsk,
    pub handshake_hash: [u8; 32],
    pub suite: Suite,
    /// The server sent `psk_category` in message 1 (current spec text);
    /// absent on aiosendspin 9.x servers (Music Assistant 2.10).
    pub server_sends_psk_category: bool,
    /// No candidate PSK matched and we completed with the Sentinel.
    pub fell_back_to_sentinel: bool,
}

/// Look up a candidate PSK by `psk_id` (and the declared category when the
/// server sent one). Returns the PSK plus, for long-term records, the
/// `server_id` it is bound to.
pub type PskResolver<'a> = dyn Fn(&str, Option<PskCategory>) -> Option<(ResolvedPsk, Option<String>)> + 'a;

fn recv_text(r: &mut WsReader, what: &str) -> Result<String> {
    match r.recv().with_context(|| format!("awaiting {}", what))? {
        WsMessage::Text(t) => Ok(t),
        WsMessage::Binary(_) => bail!("expected {} (text), got a binary message", what),
        WsMessage::Close => bail!("connection closed while awaiting {}", what),
    }
}

fn handshake_text(noise_bytes: &[u8]) -> String {
    envelope("noise/handshake", json!({ "data": b64url(noise_bytes) })).to_string()
}

fn parse_handshake_data(v: &Value) -> Result<Vec<u8>> {
    if msg_type(v) != "noise/handshake" {
        bail!("expected noise/handshake, got {:?}", msg_type(v));
    }
    let data = payload(v)
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("noise/handshake without data"))?;
    b64url_decode(data).context("noise/handshake data is not base64url")
}

/// Run the client (Noise responder) side over a fresh WebSocket.
pub fn client_handshake(
    mut ws_reader: WsReader,
    ws_writer: Arc<WsWriter>,
    client_private: &[u8; 32],
    suite: Suite,
    resolver: &PskResolver<'_>,
    timeout: Duration,
) -> Result<ClientHandshake> {
    ws_reader.set_read_timeout(Some(timeout))?;
    let client_id = b64url(&x25519_public(client_private));
    let client_init = envelope(
        "client/init",
        json!({ "client_id": client_id, "version": PROTOCOL_VERSION, "suite": suite.wire_name() }),
    )
    .to_string();
    ws_writer.send_text(&client_init)?;

    let server_init = recv_text(&mut ws_reader, "server/init")?;
    let si: Value = serde_json::from_str(&server_init).context("server/init is not JSON")?;
    match msg_type(&si) {
        "server/init" => {}
        "server/error" => bail!(
            "server rejected the connection: {}",
            payload(&si).get("reason").and_then(Value::as_str).unwrap_or("unknown")
        ),
        other => bail!("expected server/init, got {:?}", other),
    }
    let version = payload(&si).get("version").and_then(Value::as_i64).unwrap_or(0);
    if version != PROTOCOL_VERSION {
        bail!("server speaks Sendspin core version {}, we speak {}", version, PROTOCOL_VERSION);
    }
    let server_id = payload(&si)
        .get("server_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("server/init without server_id"))?
        .to_string();
    let server_pub = peer_id_to_key(&server_id).context("server_id")?;

    let mut prologue = client_init.clone().into_bytes();
    prologue.extend_from_slice(server_init.as_bytes());
    let mut hs = Handshake::responder(suite, client_private, &server_pub, &prologue);

    let hs1_text = recv_text(&mut ws_reader, "Noise message 1")?;
    let hs1: Value = serde_json::from_str(&hs1_text).context("Noise message 1 is not JSON")?;
    let msg1 = parse_handshake_data(&hs1)?;
    let p1 = hs.read_message_1(&msg1).context("Noise message 1")?;
    let (psk_id, category) = parse_msg1_payload(&p1)?;
    let server_sends_psk_category = category.is_some();

    let (psk, fell_back) = match resolve(&psk_id, category, resolver) {
        Some((psk, bound)) => {
            if let Some(b) = bound {
                if b != server_id {
                    bail!("PSK is bound to a different server (misbinding)");
                }
            }
            (psk, false)
        }
        None => (ResolvedPsk::sentinel(), true),
    };
    hs.set_psk(&psk.psk);
    let msg2 = hs.write_message_2(b"{}")?;
    ws_writer.send_text(&handshake_text(&msg2))?;
    let h = hs.handshake_hash();
    let transport = hs.into_transport()?;
    ws_reader.set_read_timeout(None)?;
    let (send, recv) = transport.split();
    let (reader, writer) = channel::channel(ws_reader, ws_writer, Some((send, recv)));
    Ok(ClientHandshake {
        reader,
        writer,
        server_id,
        psk,
        handshake_hash: h,
        suite,
        server_sends_psk_category,
        fell_back_to_sentinel: fell_back,
    })
}

/// Match a `psk_id`: the Sentinel by its published id, else via the
/// resolver.
fn resolve(psk_id: &str, category: Option<PskCategory>, resolver: &PskResolver<'_>) -> Option<(ResolvedPsk, Option<String>)> {
    let sentinel = ResolvedPsk::sentinel();
    if psk_id == sentinel.psk_id && category.map(|c| c == PskCategory::Sentinel).unwrap_or(true) {
        return Some((sentinel, None));
    }
    resolver(psk_id, category)
}

fn parse_msg1_payload(p: &[u8]) -> Result<(String, Option<PskCategory>)> {
    let v: Value = serde_json::from_slice(p).context("Noise message 1 payload is not JSON")?;
    let psk_id = v
        .get("psk_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Noise message 1 payload without psk_id"))?
        .to_string();
    let category = match v.get("psk_category") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(PskCategory::from_wire(s).ok_or_else(|| anyhow!("unknown psk_category {:?}", s))?),
        Some(_) => bail!("psk_category is not a string"),
    };
    Ok((psk_id, category))
}

/// The first frame a player sends after we (the server) connect.
pub enum FirstFrame {
    /// `client/init`: run the Noise handshake.
    Noise { raw: String, client_id: String, suite: Suite },
    /// `client/hello` on the pre-encryption wire (older players).
    LegacyHello { value: Value },
    /// `client/init` we cannot serve (version/suite); `reason` was sent.
    Refused(String),
}

pub fn read_first_frame(ws_reader: &mut WsReader, ws_writer: &WsWriter, timeout: Duration) -> Result<FirstFrame> {
    ws_reader.set_read_timeout(Some(timeout))?;
    let raw = recv_text(ws_reader, "the player's first message")?;
    let v: Value = serde_json::from_str(&raw).context("first message is not JSON")?;
    match msg_type(&v) {
        "client/init" => {
            let p = payload(&v);
            let version = p.get("version").and_then(Value::as_i64);
            if version != Some(PROTOCOL_VERSION) {
                let _ = ws_writer.send_text(&envelope("server/error", json!({"reason": "unsupported_version"})).to_string());
                return Ok(FirstFrame::Refused(format!("unsupported core version {:?}", version)));
            }
            let Some(suite) = p.get("suite").and_then(Value::as_str).and_then(Suite::from_wire) else {
                let _ = ws_writer.send_text(&envelope("server/error", json!({"reason": "unsupported_suite"})).to_string());
                return Ok(FirstFrame::Refused("unsupported cipher suite".into()));
            };
            let Some(client_id) = p.get("client_id").and_then(Value::as_str).map(str::to_string) else {
                let _ = ws_writer.send_text(&envelope("server/error", json!({"reason": "malformed"})).to_string());
                return Ok(FirstFrame::Refused("client/init without client_id".into()));
            };
            Ok(FirstFrame::Noise { raw, client_id, suite })
        }
        "client/hello" => {
            ws_reader.set_read_timeout(None)?;
            Ok(FirstFrame::LegacyHello { value: v })
        }
        other => bail!("unexpected first message {:?}", other),
    }
}

pub struct ServerHandshake {
    pub reader: ChannelReader,
    pub writer: Arc<ChannelWriter>,
    pub psk: ResolvedPsk,
    pub handshake_hash: [u8; 32],
    pub suite: Suite,
    /// Message 2 only verified under the Sentinel although we referenced
    /// another PSK: the client lost (or never had) that credential.
    pub credential_mismatch: bool,
}

/// Run the server (Noise initiator) side after `client/init` arrived.
#[allow(clippy::too_many_arguments)]
pub fn server_handshake(
    mut ws_reader: WsReader,
    ws_writer: Arc<WsWriter>,
    client_init_raw: &str,
    client_id: &str,
    suite: Suite,
    server_private: &[u8; 32],
    psk: ResolvedPsk,
    timeout: Duration,
) -> Result<ServerHandshake> {
    let client_pub = peer_id_to_key(client_id).context("client_id")?;
    let server_id = b64url(&x25519_public(server_private));
    let server_init = envelope("server/init", json!({ "server_id": server_id, "version": PROTOCOL_VERSION })).to_string();
    let mut prologue = client_init_raw.as_bytes().to_vec();
    prologue.extend_from_slice(server_init.as_bytes());
    let mut hs = Handshake::initiator(suite, server_private, &client_pub, &prologue, &psk.psk);
    let p1 = json!({ "psk_id": psk.psk_id, "psk_category": psk.category.wire() }).to_string();
    let msg1 = hs.write_message_1(p1.as_bytes())?;
    ws_writer.send_text(&server_init)?;
    ws_writer.send_text(&handshake_text(&msg1))?;

    ws_reader.set_read_timeout(Some(timeout))?;
    let hs2_text = recv_text(&mut ws_reader, "Noise message 2")?;
    let hs2: Value = serde_json::from_str(&hs2_text).context("Noise message 2 is not JSON")?;
    let msg2 = parse_handshake_data(&hs2)?;
    let fallback_state = (psk.category != PskCategory::Sentinel).then(|| hs.clone());
    let (hs, psk, mismatch) = match hs.read_message_2(&msg2) {
        Ok(_) => (hs, psk, false),
        Err(e) => {
            let Some(mut alt) = fallback_state else {
                return Err(e.context("Noise message 2"));
            };
            let sentinel = ResolvedPsk::sentinel();
            alt.set_psk(&sentinel.psk);
            alt.read_message_2(&msg2).context("Noise message 2 (also under the Sentinel PSK)")?;
            (alt, sentinel, true)
        }
    };
    let h = hs.handshake_hash();
    let transport = hs.into_transport()?;
    ws_reader.set_read_timeout(None)?;
    let (send, recv) = transport.split();
    let (reader, writer) = channel::channel(ws_reader, ws_writer, Some((send, recv)));
    Ok(ServerHandshake {
        reader,
        writer,
        psk,
        handshake_hash: h,
        suite,
        credential_mismatch: mismatch,
    })
}

/// Client side of an in-band re-handshake, triggered by an encrypted
/// `noise/handshake` (message 1) under the current keys. Message 2 goes
/// out under the old keys; both directions then switch.
#[allow(clippy::too_many_arguments)]
pub fn client_rehandshake(
    reader: &mut ChannelReader,
    writer: &ChannelWriter,
    msg1_message: &Value,
    previous_hash: &[u8; 32],
    server_id: &str,
    client_private: &[u8; 32],
    suite: Suite,
    resolver: &PskResolver<'_>,
) -> Result<(ResolvedPsk, [u8; 32])> {
    let server_pub = peer_id_to_key(server_id)?;
    let mut hs = Handshake::responder(suite, client_private, &server_pub, previous_hash);
    let msg1 = parse_handshake_data(msg1_message)?;
    let p1 = hs.read_message_1(&msg1).context("re-handshake message 1")?;
    let (psk_id, category) = parse_msg1_payload(&p1)?;
    // No Sentinel fallback on a re-handshake miss: fail.
    let (psk, bound) = resolve(&psk_id, category, resolver).ok_or_else(|| anyhow!("re-handshake references an unknown PSK"))?;
    if let Some(b) = bound {
        if b != server_id {
            bail!("PSK is bound to a different server (misbinding)");
        }
    }
    hs.set_psk(&psk.psk);
    let msg2 = hs.write_message_2(b"{}")?;
    writer.send_json_str(&handshake_text(&msg2))?;
    let h = hs.handshake_hash();
    let (send, recv) = hs.into_transport()?.split();
    writer.swap_cipher(send);
    reader.swap_cipher(recv);
    Ok((psk, h))
}

/// Server side of an in-band re-handshake to `psk`. Application messages
/// the client sent under the old keys before message 2 are discarded.
pub fn server_rehandshake(
    reader: &mut ChannelReader,
    writer: &ChannelWriter,
    previous_hash: &[u8; 32],
    client_id: &str,
    server_private: &[u8; 32],
    suite: Suite,
    psk: &ResolvedPsk,
) -> Result<[u8; 32]> {
    let client_pub = peer_id_to_key(client_id)?;
    let mut hs = Handshake::initiator(suite, server_private, &client_pub, previous_hash, &psk.psk);
    let p1 = json!({ "psk_id": psk.psk_id, "psk_category": psk.category.wire() }).to_string();
    let msg1 = hs.write_message_1(p1.as_bytes())?;
    writer.send_json_str(&handshake_text(&msg1))?;
    loop {
        match reader.recv()? {
            Incoming::Json { value, .. } if msg_type(&value) == "noise/handshake" => {
                let msg2 = parse_handshake_data(&value)?;
                hs.read_message_2(&msg2).context("re-handshake message 2")?;
                break;
            }
            Incoming::Closed => bail!("connection closed during re-handshake"),
            _ => continue, // old-key application traffic: discard
        }
    }
    let h = hs.handshake_hash();
    let (send, recv) = hs.into_transport()?.split();
    writer.swap_cipher(send);
    reader.swap_cipher(recv);
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sendspin::noise::generate_private_key;
    use crate::sendspin::ws::{self, Accepted};
    use std::net::TcpListener;

    /// Full loopback: our client handshake against our server handshake,
    /// then an encrypted JSON + binary exchange and a re-handshake to a
    /// new PSK (the post-pairing promotion).
    #[test]
    fn client_and_server_handshake_loopback_with_rehandshake() {
        let client_priv = generate_private_key();
        let server_priv = generate_private_key();
        let server_id = b64url(&x25519_public(&server_priv));
        let lt_psk = [0x42u8; 32];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // The "player"/source side listens (server-initiated connection).
        let client_thread = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let Accepted::WebSocket(r, w) = ws::accept(s, "/sendspin", Duration::from_secs(5)).unwrap() else {
                panic!()
            };
            let resolver = |id: &str, _c: Option<PskCategory>| {
                let lt = ResolvedPsk::new(lt_psk, PskCategory::LongTerm);
                (id == lt.psk_id).then(|| (lt, None))
            };
            let mut hs = client_handshake(r, w, &client_priv, Suite::AesGcm, &resolver, Duration::from_secs(5)).unwrap();
            assert_eq!(hs.psk.category, PskCategory::Sentinel);
            assert!(hs.server_sends_psk_category);
            // Echo one JSON message.
            match hs.reader.recv().unwrap() {
                Incoming::Json { value, .. } => assert_eq!(msg_type(&value), "server/hello"),
                other => panic!("{:?}", other),
            }
            hs.writer.send_binary(&[12, 1, 2, 3]).unwrap();
            // Re-handshake to the long-term PSK.
            let m1 = match hs.reader.recv().unwrap() {
                Incoming::Json { value, .. } => value,
                other => panic!("{:?}", other),
            };
            let (psk, _h) = client_rehandshake(
                &mut hs.reader,
                &hs.writer,
                &m1,
                &hs.handshake_hash,
                &hs.server_id,
                &client_priv,
                hs.suite,
                &resolver,
            )
            .unwrap();
            assert_eq!(psk.category, PskCategory::LongTerm);
            match hs.reader.recv().unwrap() {
                Incoming::Json { value, .. } => assert_eq!(msg_type(&value), "server/activate"),
                other => panic!("{:?}", other),
            }
            hs.writer.send_json(&envelope("client/state", json!({"available": true}))).unwrap();
        });

        let (mut r, w) = ws::connect(addr, "127.0.0.1", "/sendspin", Duration::from_secs(5)).unwrap();
        let FirstFrame::Noise { raw, client_id, suite } = read_first_frame(&mut r, &w, Duration::from_secs(5)).unwrap() else {
            panic!("expected noise")
        };
        assert_eq!(suite, Suite::AesGcm);
        let mut hs = server_handshake(r, w, &raw, &client_id, suite, &server_priv, ResolvedPsk::sentinel(), Duration::from_secs(5)).unwrap();
        assert!(!hs.credential_mismatch);
        hs.writer.send_json(&envelope("server/hello", json!({"name": "test"}))).unwrap();
        match hs.reader.recv().unwrap() {
            Incoming::Binary { data, .. } => assert_eq!(data, vec![12, 1, 2, 3]),
            other => panic!("{:?}", other),
        }
        let lt = ResolvedPsk::new(lt_psk, PskCategory::LongTerm);
        server_rehandshake(&mut hs.reader, &hs.writer, &hs.handshake_hash, &client_id, &server_priv, suite, &lt).unwrap();
        hs.writer.send_json(&envelope("server/activate", json!({"activities": []}))).unwrap();
        match hs.reader.recv().unwrap() {
            Incoming::Json { value, .. } => assert_eq!(msg_type(&value), "client/state"),
            other => panic!("{:?}", other),
        }
        client_thread.join().unwrap();
        let _ = server_id;
    }

    /// A server that references a long-term PSK the client lost: the
    /// client falls back to the Sentinel and the server detects the
    /// credential mismatch instead of failing.
    #[test]
    fn sentinel_fallback_on_lost_credential() {
        let client_priv = generate_private_key();
        let server_priv = generate_private_key();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let Accepted::WebSocket(r, w) = ws::accept(s, "/sendspin", Duration::from_secs(5)).unwrap() else {
                panic!()
            };
            let resolver = |_: &str, _: Option<PskCategory>| None;
            let hs = client_handshake(r, w, &client_priv, Suite::ChaChaPoly, &resolver, Duration::from_secs(5)).unwrap();
            assert!(hs.fell_back_to_sentinel);
            assert_eq!(hs.psk.category, PskCategory::Sentinel);
        });
        let (mut r, w) = ws::connect(addr, "127.0.0.1", "/sendspin", Duration::from_secs(5)).unwrap();
        let FirstFrame::Noise { raw, client_id, suite } = read_first_frame(&mut r, &w, Duration::from_secs(5)).unwrap() else {
            panic!()
        };
        let hs = server_handshake(
            r,
            w,
            &raw,
            &client_id,
            suite,
            &server_priv,
            ResolvedPsk::new([7; 32], PskCategory::LongTerm),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(hs.credential_mismatch);
        assert_eq!(hs.psk.category, PskCategory::Sentinel);
        t.join().unwrap();
    }
}
