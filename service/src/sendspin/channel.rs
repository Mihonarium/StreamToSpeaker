//! The message channel over one Sendspin WebSocket: Noise transport mode
//! (every message a binary WebSocket message carrying one Noise ciphertext
//! whose first plaintext byte is the message type) or — for pre-encryption
//! "legacy" players only — the unencrypted wire where JSON travels as text
//! frames and binary messages are sent raw.
//!
//! Message type 0 is a JSON body. Large messages are fragmented; two
//! framings exist in the field and both are accepted on receive:
//! * types 2/3 (fragment-more / fragment-end; aiosendspin 9.x),
//! * type 1 with a flags byte (current spec text).
//!
//! We never need to *send* a message larger than one Noise frame (audio
//! chunks are at most a few KB), so sending refuses oversize messages
//! rather than picking a framing.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::noise::{RecvCipher, SendCipher, MAX_PLAINTEXT};
use super::ws::{WsMessage, WsReader, WsWriter};

pub const MSG_JSON: u8 = 0;
const MSG_FRAGMENT_SPEC: u8 = 1;
const MSG_FRAGMENT_MORE: u8 = 2;
const MSG_FRAGMENT_END: u8 = 3;

/// One received application message.
#[derive(Debug)]
pub enum Incoming {
    /// A JSON message: parsed value plus the raw text (the raw text is
    /// needed where exact bytes matter, e.g. a re-handshake prologue).
    Json { value: Value, raw: String, received_at: Instant },
    /// A binary message; byte 0 is the message type.
    Binary { data: Vec<u8>, received_at: Instant },
    Closed,
}

/// Sending half, shareable across threads.
pub struct ChannelWriter {
    ws: Arc<WsWriter>,
    cipher: Mutex<Option<SendCipher>>,
}

impl ChannelWriter {
    /// Send a JSON message (`{"type":…, "payload":…}`).
    pub fn send_json(&self, msg: &Value) -> Result<()> {
        self.send_json_str(&msg.to_string())
    }

    pub fn send_json_str(&self, text: &str) -> Result<()> {
        let mut guard = self.cipher.lock().map_err(|_| anyhow!("channel poisoned"))?;
        match guard.as_mut() {
            Some(c) => {
                let mut pt = Vec::with_capacity(text.len() + 1);
                pt.push(MSG_JSON);
                pt.extend_from_slice(text.as_bytes());
                if pt.len() > MAX_PLAINTEXT {
                    bail!("JSON message of {} bytes exceeds one Noise frame", pt.len());
                }
                let ct = c.encrypt(&pt)?;
                // Encrypt + write under the same lock: the Noise counter
                // order must equal the wire order.
                self.ws.send_binary(&ct)
            }
            None => self.ws.send_text(text),
        }
    }

    /// Send a binary message whose first byte is its message type.
    pub fn send_binary(&self, data: &[u8]) -> Result<()> {
        let mut guard = self.cipher.lock().map_err(|_| anyhow!("channel poisoned"))?;
        match guard.as_mut() {
            Some(c) => {
                if data.len() > MAX_PLAINTEXT {
                    bail!("binary message of {} bytes exceeds one Noise frame", data.len());
                }
                let ct = c.encrypt(data)?;
                self.ws.send_binary(&ct)
            }
            None => self.ws.send_binary(data),
        }
    }

    /// Install new transport keys (after a re-handshake).
    pub fn swap_cipher(&self, cipher: SendCipher) {
        if let Ok(mut g) = self.cipher.lock() {
            *g = Some(cipher);
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Raw WebSocket text (handshake phase only).
    pub fn ws(&self) -> &Arc<WsWriter> {
        &self.ws
    }

    pub fn close(&self) {
        self.ws.close();
    }
}

/// Receiving half, owned by one thread.
pub struct ChannelReader {
    ws: WsReader,
    cipher: Option<RecvCipher>,
    reassembly: Option<(u8, Vec<u8>)>,
}

impl ChannelReader {
    pub fn recv(&mut self) -> Result<Incoming> {
        loop {
            let msg = self.ws.recv()?;
            let received_at = Instant::now();
            let plaintext = match (msg, self.cipher.as_mut()) {
                (WsMessage::Close, _) => return Ok(Incoming::Closed),
                (WsMessage::Text(_), Some(_)) => {
                    // Cleartext after switching to transport mode is a
                    // silent failure: close.
                    bail!("cleartext message received in transport mode");
                }
                (WsMessage::Text(t), None) => {
                    let value: Value = serde_json::from_str(&t).context("parsing JSON message")?;
                    return Ok(Incoming::Json { value, raw: t, received_at });
                }
                (WsMessage::Binary(b), None) => return Ok(Incoming::Binary { data: b, received_at }),
                (WsMessage::Binary(ct), Some(c)) => c.decrypt(&ct).context("Noise transport decrypt")?,
            };
            if let Some(done) = self.handle_plaintext(plaintext)? {
                return Ok(match done {
                    (MSG_JSON, body) => {
                        let raw = String::from_utf8(body).map_err(|_| anyhow!("JSON body is not UTF-8"))?;
                        let value: Value = serde_json::from_str(&raw).context("parsing JSON message")?;
                        Incoming::Json { value, raw, received_at }
                    }
                    (ty, body) => {
                        let mut data = Vec::with_capacity(body.len() + 1);
                        data.push(ty);
                        data.extend_from_slice(&body);
                        Incoming::Binary { data, received_at }
                    }
                });
            }
        }
    }

    /// Returns `Some((type, body))` when a complete message is available.
    fn handle_plaintext(&mut self, pt: Vec<u8>) -> Result<Option<(u8, Vec<u8>)>> {
        let Some(&ty) = pt.first() else {
            bail!("empty Noise plaintext");
        };
        match ty {
            MSG_FRAGMENT_SPEC => {
                let flags = *pt.get(1).ok_or_else(|| anyhow!("fragment without flags"))?;
                if flags & 0xFC != 0 {
                    bail!("fragment flags reserved bits set");
                }
                let first = flags & 0x02 != 0;
                let last = flags & 0x01 != 0;
                if first {
                    if self.reassembly.is_some() {
                        bail!("first fragment while another is in flight");
                    }
                    let orig = *pt.get(2).ok_or_else(|| anyhow!("first fragment without type"))?;
                    if orig == MSG_FRAGMENT_SPEC {
                        bail!("fragment orig_type 1");
                    }
                    self.reassembly = Some((orig, pt[3..].to_vec()));
                } else {
                    let Some((_, buf)) = self.reassembly.as_mut() else {
                        bail!("continuation fragment with none in flight");
                    };
                    buf.extend_from_slice(&pt[2..]);
                }
                self.check_reassembly_size()?;
                if last {
                    return Ok(self.reassembly.take());
                }
                Ok(None)
            }
            MSG_FRAGMENT_MORE => {
                match self.reassembly.as_mut() {
                    None => {
                        let orig = *pt.get(1).ok_or_else(|| anyhow!("fragment without type"))?;
                        if orig == MSG_FRAGMENT_MORE || orig == MSG_FRAGMENT_END {
                            bail!("fragment orig_type is a fragment type");
                        }
                        self.reassembly = Some((orig, pt[2..].to_vec()));
                    }
                    Some((_, buf)) => buf.extend_from_slice(&pt[1..]),
                }
                self.check_reassembly_size()?;
                Ok(None)
            }
            MSG_FRAGMENT_END => {
                let Some((orig, mut buf)) = self.reassembly.take() else {
                    bail!("fragment-end with none in flight");
                };
                buf.extend_from_slice(&pt[1..]);
                Ok(Some((orig, buf)))
            }
            _ => {
                if self.reassembly.is_some() {
                    bail!("non-fragment message while a fragmented one is in flight");
                }
                Ok(Some((ty, pt[1..].to_vec())))
            }
        }
    }

    fn check_reassembly_size(&self) -> Result<()> {
        if let Some((_, b)) = &self.reassembly {
            if b.len() > super::ws::MAX_MESSAGE_BYTES {
                bail!("fragmented message too large");
            }
        }
        Ok(())
    }

    pub fn swap_cipher(&mut self, cipher: RecvCipher) {
        self.cipher = Some(cipher);
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Raw WebSocket access (handshake phase only).
    pub fn ws_mut(&mut self) -> &mut WsReader {
        &mut self.ws
    }
}

/// Build a channel pair over a WebSocket. `cipher` is `None` for a
/// legacy unencrypted player connection.
pub fn channel(ws_reader: WsReader, ws_writer: Arc<WsWriter>, cipher: Option<(SendCipher, RecvCipher)>) -> (ChannelReader, Arc<ChannelWriter>) {
    let (send, recv) = match cipher {
        Some((s, r)) => (Some(s), Some(r)),
        None => (None, None),
    };
    (
        ChannelReader {
            ws: ws_reader,
            cipher: recv,
            reassembly: None,
        },
        Arc::new(ChannelWriter {
            ws: ws_writer,
            cipher: Mutex::new(send),
        }),
    )
}

/// `{"type": t, "payload": p}`.
pub fn envelope(msg_type: &str, payload: Value) -> Value {
    serde_json::json!({ "type": msg_type, "payload": payload })
}

/// The `type` field of a message, or "".
pub fn msg_type(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The `payload` object of a message (Null when absent).
pub fn payload(v: &Value) -> &Value {
    v.get("payload").unwrap_or(&Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader_stub() -> ChannelReader {
        // A reader whose socket is never used: only exercise reassembly.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        let c = std::net::TcpStream::connect(a).unwrap();
        let (ws_reader, _w) = super::super::ws::test_pair_from_stream(c);
        ChannelReader {
            ws: ws_reader,
            cipher: None,
            reassembly: None,
        }
    }

    #[test]
    fn reassembles_v9_fragments() {
        let mut r = reader_stub();
        assert!(r.handle_plaintext(vec![2, 4, 1, 2]).unwrap().is_none());
        assert!(r.handle_plaintext(vec![2, 3]).unwrap().is_none());
        let (ty, body) = r.handle_plaintext(vec![3, 4, 5]).unwrap().unwrap();
        assert_eq!(ty, 4);
        assert_eq!(body, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn reassembles_spec_fragments() {
        let mut r = reader_stub();
        assert!(r.handle_plaintext(vec![1, 0b10, 0, b'{']).unwrap().is_none());
        assert!(r.handle_plaintext(vec![1, 0b00, b'"', b'a']).unwrap().is_none());
        let (ty, body) = r.handle_plaintext(vec![1, 0b01, b'"', b':', b'1', b'}']).unwrap().unwrap();
        assert_eq!(ty, 0);
        assert_eq!(body, b"{\"a\":1}".to_vec());
    }

    #[test]
    fn malformed_fragment_sequences_error() {
        let mut r = reader_stub();
        assert!(r.handle_plaintext(vec![3, 1]).is_err());
        let mut r = reader_stub();
        r.handle_plaintext(vec![2, 4, 1]).unwrap();
        assert!(r.handle_plaintext(vec![4, 0]).is_err());
        let mut r = reader_stub();
        assert!(r.handle_plaintext(vec![1, 0b100, 4]).is_err());
        let mut r = reader_stub();
        assert!(r.handle_plaintext(vec![1, 0b00, 4]).is_err());
    }

    #[test]
    fn plain_messages_pass_through() {
        let mut r = reader_stub();
        let (ty, body) = r.handle_plaintext(vec![0, b'{', b'}']).unwrap().unwrap();
        assert_eq!((ty, body), (0, b"{}".to_vec()));
        let (ty, body) = r.handle_plaintext(vec![12, 9]).unwrap().unwrap();
        assert_eq!((ty, body), (12, vec![9]));
    }
}
