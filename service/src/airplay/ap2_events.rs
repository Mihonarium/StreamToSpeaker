//! AirPlay 2 event channel: the TCP connection to the receiver's
//! `eventPort` (from the first SETUP response).
//!
//! The receiver withholds its RECORD response until this connection
//! exists, then uses it to send *requests* to the sender (e.g. remote
//! commands, state updates). Traffic is HAP-framed and encrypted with the
//! event keys from pairing ([`crate::airplay::ap2_crypto::SessionKeys::event_ciphers`]).
//! Every inbound request is answered `200 OK` with its CSeq,
//! `Content-Length: 0` and `Audio-Latency: 0`, in the request's own
//! protocol. Long idle periods are normal; records and requests may be
//! split across reads and are reassembled.
//!
//! The peer closing the channel means the receiver dropped the session.

use log::{debug, info, warn};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::airplay::ap2_crypto::{ChannelCipher, HapReader};
use crate::airplay::ap2_health::{Ap2Fault, FaultChannel, FaultSlot};

/// Request header block limit.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Request body limit.
const MAX_BODY_BYTES: usize = 1 << 20;
/// Read slice, so the thread notices a stop promptly.
const READ_SLICE: Duration = Duration::from_millis(250);

/// One parsed inbound request (the body is consumed and ignored).
#[derive(Debug, PartialEq, Eq)]
pub struct EventRequest {
    pub method: String,
    pub uri: String,
    pub protocol: String,
    pub cseq: Option<String>,
    pub body_len: usize,
}

/// Take one complete request off the front of `buf`; `Ok(None)` when it
/// isn't complete yet. Errors on oversized or chunked requests.
pub fn parse_request(buf: &mut Vec<u8>) -> Result<Option<EventRequest>, String> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        if buf.len() > MAX_HEADER_BYTES {
            return Err("event request headers too large".into());
        }
        return Ok(None);
    };
    if end > MAX_HEADER_BYTES {
        return Err("event request headers too large".into());
    }
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let uri = parts.next().unwrap_or("").to_string();
    let protocol = parts.next().unwrap_or("RTSP/1.0").to_string();
    let mut cseq = None;
    let mut body_len = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "cseq" => cseq = Some(v.to_string()),
            "content-length" => {
                body_len = v.parse().map_err(|_| format!("bad Content-Length {v:?}"))?;
            }
            "transfer-encoding" if v.eq_ignore_ascii_case("chunked") => {
                return Err("chunked event requests are not supported".into());
            }
            _ => {}
        }
    }
    if body_len > MAX_BODY_BYTES {
        return Err(format!("event request body too large ({body_len} bytes)"));
    }
    let total = end + 4 + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    buf.drain(..total);
    Ok(Some(EventRequest { method, uri, protocol, cseq, body_len }))
}

/// The reply to every event request.
pub fn build_reply(req: &EventRequest) -> Vec<u8> {
    let protocol = if req.protocol.starts_with("HTTP/") || req.protocol.starts_with("RTSP/") {
        req.protocol.as_str()
    } else {
        "RTSP/1.0"
    };
    let mut out = format!("{protocol} 200 OK\r\n");
    if let Some(c) = &req.cseq {
        out.push_str(&format!("CSeq: {c}\r\n"));
    }
    out.push_str("Content-Length: 0\r\nAudio-Latency: 0\r\n\r\n");
    out.into_bytes()
}

/// Connect the event channel and serve it on a thread. Best-effort: a
/// missing or unreachable port logs and returns `None` rather than failing
/// the session. Without `ciphers` (or after a record fails to decrypt) the
/// channel is only kept open and drained, with no replies.
///
/// `close_is_fault`: whether the receiver closing the channel ends the
/// session (`peer_closed`, retryable); otherwise it's only logged.
pub fn spawn_event_channel(
    receiver_ip: IpAddr,
    event_port: u16,
    ciphers: Option<(ChannelCipher, ChannelCipher)>,
    stop_flag: Arc<AtomicBool>,
    faults: FaultSlot,
    close_is_fault: bool,
    receiver_name: String,
) -> Option<JoinHandle<()>> {
    if event_port == 0 {
        debug!("AirPlay 2: no eventPort advertised; skipping event channel");
        return None;
    }
    let addr = SocketAddr::new(receiver_ip, event_port);
    let stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(s) => s,
        Err(e) => {
            warn!("AirPlay 2: event channel connect to {} failed: {}", addr, e);
            return None;
        }
    };
    let _ = stream.set_read_timeout(Some(READ_SLICE));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    stream.set_nodelay(true).ok();
    debug!("AirPlay 2: event channel open to {}", addr);

    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-event:{}", receiver_name))
        .spawn(move || run_event_channel(stream, ciphers, stop_flag, faults, close_is_fault, receiver_name))
        .ok()
}

fn run_event_channel(
    mut stream: TcpStream,
    ciphers: Option<(ChannelCipher, ChannelCipher)>,
    stop_flag: Arc<AtomicBool>,
    faults: FaultSlot,
    close_is_fault: bool,
    name: String,
) {
    let mut codec = ciphers.map(|(tx, rx)| (tx, HapReader::new(rx)));
    let mut plain: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut served: u64 = 0;
    while !stop_flag.load(Ordering::Acquire) {
        let n = match stream.read(&mut buf) {
            Ok(0) => {
                if stop_flag.load(Ordering::Acquire) {
                    break;
                }
                if close_is_fault {
                    faults.raise(Ap2Fault::new(
                        "peer_closed",
                        FaultChannel::Events,
                        true,
                        "receiver closed the event channel",
                    ));
                } else {
                    warn!("AirPlay 2 {}: receiver closed the event channel", name);
                }
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                continue
            }
            Err(e) => {
                if !stop_flag.load(Ordering::Acquire) {
                    warn!("AirPlay 2 {}: event channel read failed: {}", name, e);
                }
                break;
            }
        };
        let Some((tx, rx)) = codec.as_mut() else {
            debug!("AirPlay 2 {}: event channel: {} bytes (not decoded)", name, n);
            continue;
        };
        match rx.push(&buf[..n]) {
            Ok(p) => plain.extend_from_slice(&p),
            Err(e) => {
                // Don't end the session over a channel we can't read; keep
                // it open as before and stop answering.
                warn!("AirPlay 2 {}: event channel {} — draining without replies", name, e);
                codec = None;
                plain.clear();
                continue;
            }
        }
        loop {
            match parse_request(&mut plain) {
                Ok(Some(req)) => {
                    served += 1;
                    if served <= 3 {
                        info!(
                            "AirPlay 2 {}: event {} {} ({} B body)",
                            name, req.method, req.uri, req.body_len
                        );
                    } else {
                        debug!("AirPlay 2 {}: event {} {}", name, req.method, req.uri);
                    }
                    let reply = tx.encrypt(&build_reply(&req));
                    if let Err(e) = stream.write_all(&reply) {
                        warn!("AirPlay 2 {}: event reply failed: {}", name, e);
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("AirPlay 2 {}: event channel: {} — draining without replies", name, e);
                    codec = None;
                    plain.clear();
                    break;
                }
            }
        }
    }
    debug!("AirPlay 2 {}: event channel closed ({} requests answered)", name, served);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::ap2_crypto::SessionKeys;
    use std::net::TcpListener;

    #[test]
    fn parses_split_request_with_body() {
        let msg = b"POST /command RTSP/1.0\r\nCSeq: 12\r\nContent-Length: 4\r\n\r\nbodyGET /x HTTP/1.1\r\nCSeq: 13\r\n\r\n";
        let mut buf = msg[..20].to_vec();
        assert_eq!(parse_request(&mut buf).unwrap(), None);
        buf.extend_from_slice(&msg[20..55]);
        assert_eq!(parse_request(&mut buf).unwrap(), None); // body incomplete
        buf.extend_from_slice(&msg[55..]);
        let a = parse_request(&mut buf).unwrap().unwrap();
        assert_eq!((a.method.as_str(), a.cseq.as_deref(), a.body_len), ("POST", Some("12"), 4));
        let b = parse_request(&mut buf).unwrap().unwrap();
        assert_eq!((b.protocol.as_str(), b.cseq.as_deref()), ("HTTP/1.1", Some("13")));
        assert!(buf.is_empty());
    }

    #[test]
    fn reply_echoes_protocol_and_cseq() {
        let req = EventRequest {
            method: "POST".into(),
            uri: "/command".into(),
            protocol: "HTTP/1.1".into(),
            cseq: Some("7".into()),
            body_len: 0,
        };
        assert_eq!(
            build_reply(&req),
            b"HTTP/1.1 200 OK\r\nCSeq: 7\r\nContent-Length: 0\r\nAudio-Latency: 0\r\n\r\n".to_vec()
        );
    }

    #[test]
    fn rejects_chunked_and_oversized() {
        assert!(parse_request(&mut b"POST / RTSP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()).is_err());
        assert!(parse_request(&mut b"POST / RTSP/1.0\r\nContent-Length: 2000000\r\n\r\n".to_vec()).is_err());
        assert!(parse_request(&mut vec![b'a'; MAX_HEADER_BYTES + 1]).is_err());
    }

    /// End to end over loopback: an idle period, then one encrypted
    /// request split across writes, answered with an encrypted 200; then
    /// the receiver closing the channel raises `peer_closed`.
    #[test]
    fn answers_encrypted_requests_and_reports_close() {
        let keys = SessionKeys::from_shared(&[8u8; 64]);
        let (mut recv_tx, recv_rx) = keys.receiver_event_ciphers();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let dead = Arc::new(AtomicBool::new(false));
        let faults = FaultSlot::new(dead.clone(), "test".into());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_event_channel(
            "127.0.0.1".parse().unwrap(),
            port,
            Some(keys.event_ciphers()),
            stop.clone(),
            faults.clone(),
            true,
            "test".into(),
        )
        .unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(300)); // idle
        let wire = recv_tx.encrypt(b"POST /command RTSP/1.0\r\nCSeq: 5\r\nContent-Length: 0\r\n\r\n");
        peer.write_all(&wire[..1]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        peer.write_all(&wire[1..30]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        peer.write_all(&wire[30..]).unwrap();

        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut reader = HapReader::new(recv_rx);
        let mut got = Vec::new();
        while !got.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 256];
            let n = peer.read(&mut b).unwrap();
            assert!(n > 0);
            got.extend(reader.push(&b[..n]).unwrap());
        }
        assert_eq!(got, b"RTSP/1.0 200 OK\r\nCSeq: 5\r\nContent-Length: 0\r\nAudio-Latency: 0\r\n\r\n".to_vec());
        assert!(!dead.load(Ordering::Acquire));

        drop(peer);
        handle.join().unwrap();
        assert!(dead.load(Ordering::Acquire));
        assert_eq!(faults.get().unwrap().code, "peer_closed");
    }
}
