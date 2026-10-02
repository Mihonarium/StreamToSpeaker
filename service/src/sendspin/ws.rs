//! Minimal RFC 6455 WebSocket transport over a blocking `TcpStream`.
//!
//! Sendspin needs a plain `ws://` socket (confidentiality comes from the
//! Noise layer inside the payloads), in both directions: we accept
//! connections from a Music Assistant server (we are a Sendspin *client*
//! that the server dials) and we dial Sendspin players (we are the
//! *server* side of the protocol but the WebSocket client).
//!
//! The split reader/writer design lets one thread block in `recv` while
//! other threads (audio sender, time-sync replies) write: the writer
//! serialises whole frames under a mutex, so frames never interleave.
//! A hand-rolled implementation keeps that split trivial (the socket is
//! `try_clone`d) and adds no dependency — the protocol surface we need
//! (text/binary/ping/pong/close, continuation frames, client masking) is
//! small.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// GUID appended to the client key when computing `Sec-WebSocket-Accept`.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Largest message we accept (artwork/codec headers are far below this;
/// it only bounds memory against a misbehaving peer).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Largest HTTP upgrade header block we read.
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Which end of the WebSocket we are. Clients mask every frame they send
/// (RFC 6455 §5.3); servers never do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsRole {
    Client,
    Server,
}

/// One complete WebSocket data message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    /// The peer sent a Close frame (or the stream ended cleanly).
    Close,
}

/// Write half. Cheap to share (`Arc`); every send writes one whole frame
/// under the mutex.
pub struct WsWriter {
    stream: Mutex<TcpStream>,
    role: WsRole,
}

impl WsWriter {
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.send_frame(OP_TEXT, text.as_bytes())
    }

    pub fn send_binary(&self, data: &[u8]) -> Result<()> {
        self.send_frame(OP_BINARY, data)
    }

    pub fn send_ping(&self, data: &[u8]) -> Result<()> {
        self.send_frame(OP_PING, data)
    }

    fn send_pong(&self, data: &[u8]) -> Result<()> {
        self.send_frame(OP_PONG, data)
    }

    /// Send a Close frame (best effort) and shut the socket down so a
    /// reader blocked in `recv` on another thread returns.
    pub fn close(&self) {
        let _ = self.send_frame(OP_CLOSE, &1000u16.to_be_bytes());
        self.shutdown();
    }

    /// Shut the socket down without a Close frame.
    pub fn shutdown(&self) {
        if let Ok(s) = self.stream.lock() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn send_frame(&self, opcode: u8, payload: &[u8]) -> Result<()> {
        let frame = encode_frame(opcode, payload, self.role);
        let mut s = self.stream.lock().map_err(|_| anyhow!("websocket writer poisoned"))?;
        s.write_all(&frame).context("websocket write")?;
        Ok(())
    }
}

/// Read half. Owned by exactly one thread.
pub struct WsReader {
    stream: TcpStream,
    /// Bytes already read from the socket but not yet consumed (the
    /// handshake read can over-read into the first frame).
    pending: Vec<u8>,
    writer: Arc<WsWriter>,
    role: WsRole,
}

impl WsReader {
    /// Receive the next complete data message. Pings are answered and
    /// pongs skipped transparently. Returns `WsMessage::Close` on a Close
    /// frame or EOF.
    pub fn recv(&mut self) -> Result<WsMessage> {
        let mut assembling: Option<(u8, Vec<u8>)> = None;
        loop {
            let frame = match self.read_frame()? {
                Some(f) => f,
                None => return Ok(WsMessage::Close),
            };
            match frame.opcode {
                OP_PING => {
                    let _ = self.writer.send_pong(&frame.payload);
                }
                OP_PONG => {}
                OP_CLOSE => {
                    // Echo the close (best effort) then report it.
                    let _ = self.writer.send_frame(OP_CLOSE, &frame.payload);
                    return Ok(WsMessage::Close);
                }
                OP_TEXT | OP_BINARY => {
                    if assembling.is_some() {
                        bail!("websocket: new data frame inside a fragmented message");
                    }
                    if frame.fin {
                        return finish_message(frame.opcode, frame.payload);
                    }
                    assembling = Some((frame.opcode, frame.payload));
                }
                OP_CONT => {
                    let Some((op, mut buf)) = assembling.take() else {
                        bail!("websocket: continuation frame without a started message");
                    };
                    buf.extend_from_slice(&frame.payload);
                    if buf.len() > MAX_MESSAGE_BYTES {
                        bail!("websocket: message exceeds {} bytes", MAX_MESSAGE_BYTES);
                    }
                    if frame.fin {
                        return finish_message(op, buf);
                    }
                    assembling = Some((op, buf));
                }
                other => bail!("websocket: unknown opcode {:#x}", other),
            }
        }
    }

    /// Set (or clear) the socket read timeout. A timed-out `recv` returns
    /// an error; callers that use timeouts must treat the connection as
    /// broken afterwards (a partial frame may have been consumed).
    pub fn set_read_timeout(&self, t: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(t).context("set_read_timeout")
    }

    fn fill(&mut self, n: usize) -> Result<bool> {
        let mut chunk = [0u8; 8192];
        while self.pending.len() < n {
            let got = match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(false),
                Ok(g) => g,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("websocket read"),
            };
            self.pending.extend_from_slice(&chunk[..got]);
        }
        Ok(true)
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let rest = self.pending.split_off(n);
        std::mem::replace(&mut self.pending, rest)
    }

    fn read_frame(&mut self) -> Result<Option<Frame>> {
        if !self.fill(2)? {
            return Ok(None);
        }
        let b0 = self.pending[0];
        let b1 = self.pending[1];
        let fin = b0 & 0x80 != 0;
        if b0 & 0x70 != 0 {
            bail!("websocket: reserved bits set (no extensions negotiated)");
        }
        let opcode = b0 & 0x0F;
        let masked = b1 & 0x80 != 0;
        let len7 = (b1 & 0x7F) as usize;
        let (len, hdr) = match len7 {
            126 => {
                if !self.fill(4)? {
                    return Ok(None);
                }
                (u16::from_be_bytes([self.pending[2], self.pending[3]]) as usize, 4)
            }
            127 => {
                if !self.fill(10)? {
                    return Ok(None);
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.pending[2..10]);
                let l = u64::from_be_bytes(b);
                if l > MAX_MESSAGE_BYTES as u64 {
                    bail!("websocket: frame of {} bytes exceeds limit", l);
                }
                (l as usize, 10)
            }
            n => (n, 2),
        };
        if len > MAX_MESSAGE_BYTES {
            bail!("websocket: frame of {} bytes exceeds limit", len);
        }
        let mask_len = if masked { 4 } else { 0 };
        if self.role == WsRole::Client && masked {
            bail!("websocket: server sent a masked frame");
        }
        if !self.fill(hdr + mask_len + len)? {
            return Ok(None);
        }
        let header = self.take(hdr + mask_len);
        let mut payload = self.take(len);
        if masked {
            let key = &header[hdr..hdr + 4];
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= key[i & 3];
            }
        }
        if opcode >= 0x8 && (!fin || len > 125) {
            bail!("websocket: malformed control frame");
        }
        Ok(Some(Frame { fin, opcode, payload }))
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn finish_message(opcode: u8, payload: Vec<u8>) -> Result<WsMessage> {
    if opcode == OP_TEXT {
        let s = String::from_utf8(payload).map_err(|_| anyhow!("websocket: text frame is not UTF-8"))?;
        Ok(WsMessage::Text(s))
    } else {
        Ok(WsMessage::Binary(payload))
    }
}

/// Encode one unfragmented frame.
pub(crate) fn encode_frame(opcode: u8, payload: &[u8], role: WsRole) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0F));
    let mask_bit = if role == WsRole::Client { 0x80 } else { 0 };
    let len = payload.len();
    if len < 126 {
        out.push(mask_bit | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(mask_bit | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if role == WsRole::Client {
        let mut key = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut key);
        out.extend_from_slice(&key);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    } else {
        out.extend_from_slice(payload);
    }
    out
}

/// `Sec-WebSocket-Accept` for a client key.
pub fn accept_key(client_key: &str) -> String {
    let mut h = Sha1::new();
    h.update(client_key.trim().as_bytes());
    h.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// Read an HTTP header block (up to and including the blank line).
/// Returns `(header_text, leftover_bytes)`.
fn read_http_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let leftover = buf[pos + 4..].to_vec();
            return Ok((head, leftover));
        }
        if buf.len() > MAX_HANDSHAKE_BYTES {
            bail!("websocket: HTTP header block too large");
        }
        let n = stream.read(&mut chunk).context("reading HTTP upgrade header")?;
        if n == 0 {
            bail!("websocket: connection closed during HTTP upgrade");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Case-insensitive header lookup in a raw header block.
pub(crate) fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Outcome of reading an inbound HTTP request on a listener socket.
pub enum Accepted {
    /// A WebSocket upgrade for the expected path — the connection is live.
    WebSocket(WsReader, Arc<WsWriter>),
    /// Something else (wrong path, plain GET); a response was already sent.
    Rejected(String),
}

/// Server side: complete the HTTP upgrade on an accepted TCP stream.
/// `path` is the expected request path (e.g. `/sendspin`).
pub fn accept(mut stream: TcpStream, path: &str, handshake_timeout: Duration) -> Result<Accepted> {
    stream.set_read_timeout(Some(handshake_timeout)).ok();
    stream.set_nodelay(true).ok();
    let (head, leftover) = read_http_head(&mut stream)?;
    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let req_path = target.split('?').next().unwrap_or("");
    let upgrade = header_value(&head, "Upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let key = header_value(&head, "Sec-WebSocket-Key");
    if method != "GET" || req_path != path || !upgrade || key.is_none() {
        let body = "Not a Sendspin WebSocket endpoint\n";
        let resp = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        return Ok(Accepted::Rejected(format!("{} {}", method, target)));
    }
    let accept = accept_key(key.unwrap_or_default());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept
    );
    stream.write_all(resp.as_bytes()).context("writing 101 response")?;
    stream.set_read_timeout(None).ok();
    let (r, w) = split(stream, leftover, WsRole::Server)?;
    Ok(Accepted::WebSocket(r, w))
}

/// Client side: connect to `ws://addr<path>` and complete the upgrade.
pub fn connect(addr: SocketAddr, host_header: &str, path: &str, timeout: Duration) -> Result<(WsReader, Arc<WsWriter>)> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("connecting to {}", addr))?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let mut key_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key_bytes);
    let key = base64::engine::general_purpose::STANDARD.encode(key_bytes);
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: {}\r\n\r\n",
        path,
        host_header,
        key,
        crate::PRODUCT_UA
    );
    stream.write_all(req.as_bytes()).context("sending WebSocket upgrade")?;
    let (head, leftover) = read_http_head(&mut stream)?;
    let status = head.lines().next().unwrap_or("");
    if !status.split_whitespace().nth(1).map(|c| c == "101").unwrap_or(false) {
        bail!("WebSocket upgrade refused: {}", status);
    }
    let expected = accept_key(&key);
    match header_value(&head, "Sec-WebSocket-Accept") {
        Some(v) if v == expected => {}
        other => bail!("bad Sec-WebSocket-Accept {:?}", other),
    }
    stream.set_read_timeout(None).ok();
    split(stream, leftover, WsRole::Client)
}

fn split(stream: TcpStream, leftover: Vec<u8>, role: WsRole) -> Result<(WsReader, Arc<WsWriter>)> {
    // A stalled peer must not wedge our writers forever (a blocked write
    // would hold the mutex and freeze every sender behind it).
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let write_half = stream.try_clone().context("cloning TCP stream")?;
    let writer = Arc::new(WsWriter {
        stream: Mutex::new(write_half),
        role,
    });
    Ok((
        WsReader {
            stream,
            pending: leftover,
            writer: writer.clone(),
            role,
        },
        writer,
    ))
}

/// Test helper: wrap an already-connected stream without a handshake.
#[cfg(test)]
pub(crate) fn test_pair_from_stream(stream: TcpStream) -> (WsReader, Arc<WsWriter>) {
    split(stream, Vec::new(), WsRole::Server).expect("split")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn accept_key_matches_rfc6455_example() {
        // RFC 6455 §1.3 worked example.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn frame_lengths_encode_all_three_forms() {
        let small = encode_frame(OP_BINARY, &[1, 2, 3], WsRole::Server);
        assert_eq!(&small[..2], &[0x82, 3]);
        let mid = encode_frame(OP_BINARY, &vec![0u8; 300], WsRole::Server);
        assert_eq!(&mid[..4], &[0x82, 126, 0x01, 0x2C]);
        let big = encode_frame(OP_BINARY, &vec![0u8; 70_000], WsRole::Server);
        assert_eq!(big[1], 127);
        assert_eq!(u64::from_be_bytes(big[2..10].try_into().unwrap()), 70_000);
        // Client frames are masked: mask bit set and 4 key bytes.
        let masked = encode_frame(OP_TEXT, b"hi", WsRole::Client);
        assert_eq!(masked[1], 0x80 | 2);
        assert_eq!(masked.len(), 2 + 4 + 2);
        let key = &masked[2..6];
        assert_eq!(masked[6] ^ key[0], b'h');
        assert_eq!(masked[7] ^ key[1], b'i');
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let head = "GET /x HTTP/1.1\r\nsec-websocket-key: abc\r\nUpgrade: WebSocket";
        assert_eq!(header_value(head, "Sec-WebSocket-Key"), Some("abc"));
        assert_eq!(header_value(head, "upgrade"), Some("WebSocket"));
        assert_eq!(header_value(head, "missing"), None);
    }

    #[test]
    fn loopback_roundtrip_with_fragmentation_and_ping() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let Accepted::WebSocket(mut r, w) = accept(s, "/sendspin", Duration::from_secs(5)).unwrap() else {
                panic!("rejected");
            };
            // Echo two messages back.
            for _ in 0..2 {
                match r.recv().unwrap() {
                    WsMessage::Text(t) => w.send_text(&format!("echo:{}", t)).unwrap(),
                    WsMessage::Binary(b) => w.send_binary(&b).unwrap(),
                    WsMessage::Close => panic!("early close"),
                }
            }
            assert_eq!(r.recv().unwrap(), WsMessage::Close);
        });
        let (mut r, w) = connect(addr, "127.0.0.1", "/sendspin", Duration::from_secs(5)).unwrap();
        w.send_ping(b"p").unwrap();
        w.send_text("hello").unwrap();
        assert_eq!(r.recv().unwrap(), WsMessage::Text("echo:hello".into()));
        // A message split across a binary frame + continuation frame.
        {
            let mut first = encode_frame(OP_BINARY, &[1, 2], WsRole::Client);
            first[0] &= 0x7F; // clear FIN
            let cont = encode_frame(OP_CONT, &[3, 4], WsRole::Client);
            let mut s = w.stream.lock().unwrap();
            s.write_all(&first).unwrap();
            s.write_all(&cont).unwrap();
        }
        assert_eq!(r.recv().unwrap(), WsMessage::Binary(vec![1, 2, 3, 4]));
        w.close();
        server.join().unwrap();
    }

    #[test]
    fn wrong_path_is_rejected_with_404() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            matches!(accept(s, "/sendspin", Duration::from_secs(5)).unwrap(), Accepted::Rejected(_))
        });
        let err = connect(addr, "127.0.0.1", "/other", Duration::from_secs(5)).err().unwrap();
        assert!(format!("{:#}", err).contains("404"));
        assert!(server.join().unwrap());
    }
}
