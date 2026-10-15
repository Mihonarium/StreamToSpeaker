//! Test-only scripted AirPlay 2 receivers on loopback: one per way a
//! pair-verify can end, plus the RTSP request/reply plumbing they share,
//! for the tests in ap2_rtsp.rs, ap2_session.rs and app.rs.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use ed25519_dalek::{Signer, SigningKey};
use hkdf::Hkdf;
use sha2::Sha512;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::airplay::hap_pairing::{PairingCredentials, StoredPairing};
use crate::airplay::tlv8::{
    Tlv, TlvBuilder, TYPE_ENCRYPTED_DATA, TYPE_ERROR, TYPE_IDENTIFIER, TYPE_PUBLIC_KEY, TYPE_SIGNATURE,
    TYPE_STATE,
};

/// The paired accessory's pairing identifier.
pub const ACC_ID: &str = "AA:BB:CC:DD:EE:FF";

/// The paired accessory's long-term key.
pub fn accessory_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// The pairing the client has stored for that accessory, as the config
/// holds it.
pub fn stored_credentials() -> PairingCredentials {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{:02x}", x)).collect::<String>();
    PairingCredentials {
        controller_id: "6b1f2c3d-0000-4000-8000-00000000c0de".into(),
        controller_seed_hex: hex(&[9u8; 32]),
        accessory_id: ACC_ID.into(),
        accessory_ltpk_hex: hex(&accessory_key().verifying_key().to_bytes()),
    }
}

/// [`stored_credentials`], parsed.
pub fn stored_pairing() -> StoredPairing {
    StoredPairing::parse(&stored_credentials()).unwrap()
}

/// Read one RTSP request: `(request line, CSeq, body)`. None once the
/// client has closed (or sent something that isn't a plaintext request).
pub fn read_request(r: &mut BufReader<TcpStream>) -> Option<(String, u32, Vec<u8>)> {
    let mut first = String::new();
    if r.read_line(&mut first).ok()? == 0 {
        return None;
    }
    let (mut len, mut cseq) = (0usize, 0u32);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().unwrap_or(0),
                "cseq" => cseq = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((first.trim_end().to_string(), cseq, body))
}

pub fn reply(w: &mut TcpStream, cseq: u32, status: &str, body: &[u8]) {
    let head = format!("RTSP/1.0 {}\r\nCSeq: {}\r\nContent-Length: {}\r\n\r\n", status, cseq, body.len());
    let _ = w.write_all(head.as_bytes());
    let _ = w.write_all(body);
    let _ = w.flush();
}

/// An accessory's M2 for the client's M1: a fresh X25519 key, and
/// `acc_id` with a signature by `signer` over (accessory key | acc_id |
/// client key), sealed under the pair-verify key.
pub fn build_m2(m1: &[u8], acc_id: &str, signer: &SigningKey) -> Vec<u8> {
    let tlv = Tlv::decode(m1).expect("M1 decodes");
    let client_eph: [u8; 32] = tlv.get(TYPE_PUBLIC_KEY).expect("M1 key").try_into().expect("32 bytes");
    let secret = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
    let acc_eph = x25519_dalek::PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(client_eph)).to_bytes();
    let mut key = [0u8; 32];
    Hkdf::<Sha512>::new(Some(b"Pair-Verify-Encrypt-Salt"), &shared)
        .expand(b"Pair-Verify-Encrypt-Info", &mut key)
        .unwrap();
    let mut info = Vec::new();
    info.extend_from_slice(&acc_eph);
    info.extend_from_slice(acc_id.as_bytes());
    info.extend_from_slice(&client_eph);
    let sub = TlvBuilder::new()
        .add(TYPE_IDENTIFIER, acc_id.as_bytes())
        .add(TYPE_SIGNATURE, &signer.sign(&info).to_bytes())
        .build();
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(b"PV-Msg02");
    let sealed = ChaCha20Poly1305::new((&key).into())
        .encrypt((&nonce).into(), Payload { msg: &sub, aad: &[] })
        .unwrap();
    TlvBuilder::new()
        .add_u8(TYPE_STATE, 2)
        .add(TYPE_PUBLIC_KEY, &acc_eph)
        .add(TYPE_ENCRYPTED_DATA, &sealed)
        .build()
}

/// What the paired accessory answers to M3.
#[derive(Clone, Copy, Debug)]
pub enum M4 {
    Accept,
    /// M4 with this TLV error (2 = it doesn't know this controller).
    Error(u8),
    /// This RTSP status instead of an M4.
    Status(&'static str),
    /// 200 with a body that is no TLV.
    Junk,
    /// 200 with a TLV that is neither State 4 nor an error.
    WrongState,
}

/// How a scripted receiver answers pair-verify.
#[derive(Clone, Debug)]
pub enum Peer {
    /// Answers M1 with this status line and no body.
    StatusAtM1(&'static str),
    /// Answers M1 with these raw bytes.
    RawAtM1(&'static [u8]),
    /// Answers M1 with 200 and a body that is no M2.
    GarbageAtM1,
    /// Answers M1 with an M2 that carries this TLV error.
    ErrorTlvAtM1(u8),
    /// Claims the paired accessory's identifier, signs with its own key.
    Impostor,
    /// Another device: its own identifier and key.
    OtherDevice,
    /// The paired accessory: a genuine M2, then this answer to M3.
    Genuine(M4),
    /// Reads M1 and closes the connection.
    CloseAtM1,
    /// Reads M1 and never answers.
    SilentAtM1,
    /// Passes M1 to the real accessory listening on `upstream` and its
    /// genuine M2 back, then answers M3 itself with a forged refusal.
    RelayForgingM4 { upstream: u16 },
}

/// Serve one connection as `peer` on a fresh loopback port. Returns the
/// port and a handle that yields the request lines the peer received, in
/// order, once the client has closed.
pub fn spawn(peer: Peer) -> (u16, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut r = BufReader::new(sock.try_clone().unwrap());
        let mut w = sock;
        let mut lines = Vec::new();
        let Some((mut line, mut cseq, mut m1)) = read_request(&mut r) else { return lines };
        // A session opens with GET /info (AirPlay2Session::start does):
        // answer it as a receiver with nothing to report, and take the
        // next request as M1.
        if line.starts_with("GET /info") {
            reply(&mut w, cseq, "200 OK", &[]);
            lines.push(line);
            let Some(next) = read_request(&mut r) else { return lines };
            (line, cseq, m1) = next;
        }
        lines.push(line);
        match peer {
            Peer::StatusAtM1(status) => reply(&mut w, cseq, status, &[]),
            Peer::RawAtM1(bytes) => {
                let _ = w.write_all(bytes);
            }
            Peer::GarbageAtM1 => reply(&mut w, cseq, "200 OK", b"\x99\x99"),
            Peer::ErrorTlvAtM1(code) => {
                let m2 = TlvBuilder::new().add_u8(TYPE_STATE, 2).add_u8(TYPE_ERROR, code).build();
                reply(&mut w, cseq, "200 OK", &m2)
            }
            Peer::Impostor => reply(&mut w, cseq, "200 OK", &build_m2(&m1, ACC_ID, &SigningKey::from_bytes(&[66; 32]))),
            Peer::OtherDevice => {
                reply(&mut w, cseq, "200 OK", &build_m2(&m1, "11:22:33:44:55:66", &SigningKey::from_bytes(&[13; 32])))
            }
            Peer::Genuine(m4) => {
                reply(&mut w, cseq, "200 OK", &build_m2(&m1, ACC_ID, &accessory_key()));
                let Some((line, cseq, _m3)) = read_request(&mut r) else { return lines };
                lines.push(line);
                match m4 {
                    M4::Accept => reply(&mut w, cseq, "200 OK", &TlvBuilder::new().add_u8(TYPE_STATE, 4).build()),
                    M4::Error(code) => {
                        let body = TlvBuilder::new().add_u8(TYPE_STATE, 4).add_u8(TYPE_ERROR, code).build();
                        reply(&mut w, cseq, "200 OK", &body)
                    }
                    M4::Status(status) => reply(&mut w, cseq, status, &[]),
                    M4::Junk => reply(&mut w, cseq, "200 OK", b"\x99\x99"),
                    M4::WrongState => reply(&mut w, cseq, "200 OK", &TlvBuilder::new().add_u8(TYPE_STATE, 2).build()),
                }
            }
            Peer::CloseAtM1 => return lines,
            Peer::SilentAtM1 => {}
            Peer::RelayForgingM4 { upstream } => {
                let up = TcpStream::connect(("127.0.0.1", upstream)).unwrap();
                up.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let head = format!(
                    "POST /pair-verify RTSP/1.0\r\nCSeq: 1\r\nX-Apple-HKP: 3\r\nContent-Length: {}\r\n\r\n",
                    m1.len()
                );
                let mut up_w = up.try_clone().unwrap();
                up_w.write_all(head.as_bytes()).unwrap();
                up_w.write_all(&m1).unwrap();
                let m2 = read_reply_body(&mut BufReader::new(up));
                reply(&mut w, cseq, "200 OK", &m2);
                let Some((line, cseq, _m3)) = read_request(&mut r) else { return lines };
                lines.push(line);
                let forged = TlvBuilder::new().add_u8(TYPE_STATE, 4).add_u8(TYPE_ERROR, 2).build();
                reply(&mut w, cseq, "200 OK", &forged);
            }
        }
        // Whatever the client sends next, until it closes.
        while let Some((line, _, _)) = read_request(&mut r) {
            lines.push(line);
        }
        lines
    });
    (port, handle)
}

/// The body of one RTSP reply.
fn read_reply_body(r: &mut BufReader<TcpStream>) -> Vec<u8> {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    body
}
