//! Noise `KKpsk2` handshake + transport, as used by Sendspin.
//!
//! Pattern (Noise spec §7.4 + §9 psk modifier):
//!
//! ```text
//! KKpsk2:
//!   -> s
//!   <- s
//!   ...
//!   -> e, es, ss
//!   <- e, ee, se, psk
//! ```
//!
//! The Sendspin **server is always the Noise initiator** and the client
//! the responder, regardless of who opened the WebSocket. Both static keys
//! are known up front (they are the `server_id` / `client_id`), and the
//! PSK is only mixed in at the end of message 2 — which is what lets the
//! client read message 1's payload (the `psk_id`) *before* it has to pick
//! the PSK. Two suites: `25519_ChaChaPoly_SHA256` and
//! `25519_AESGCM_SHA256`.
//!
//! Implemented directly from the Noise Protocol Framework rev 34 rather
//! than via a crate: the state is plain data (`Clone`), which a server
//! needs to verify message 2 against a second PSK (the Sentinel fallback),
//! and the PSK can be supplied after message 1 has been read.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

type HmacSha256 = Hmac<Sha256>;

/// Noise limit on one transport message, ciphertext included.
pub const MAX_NOISE_MESSAGE: usize = 65_535;
/// AEAD tag length for both suites.
pub const TAG_LEN: usize = 16;
/// Largest plaintext that fits one Noise transport message.
pub const MAX_PLAINTEXT: usize = MAX_NOISE_MESSAGE - TAG_LEN;

/// The two Sendspin cipher suites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suite {
    ChaChaPoly,
    AesGcm,
}

impl Suite {
    /// The `suite` string carried in `client/init`.
    pub fn wire_name(self) -> &'static str {
        match self {
            Suite::ChaChaPoly => "25519_ChaChaPoly_SHA256",
            Suite::AesGcm => "25519_AESGCM_SHA256",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "25519_ChaChaPoly_SHA256" => Some(Suite::ChaChaPoly),
            "25519_AESGCM_SHA256" => Some(Suite::AesGcm),
            _ => None,
        }
    }

    fn protocol_name(self) -> String {
        format!("Noise_KKpsk2_{}", self.wire_name())
    }

    fn nonce(self, n: u64) -> [u8; 12] {
        let mut out = [0u8; 12];
        match self {
            // ChaChaPoly: 32 bits of zeros + little-endian counter.
            Suite::ChaChaPoly => out[4..].copy_from_slice(&n.to_le_bytes()),
            // AESGCM: 32 bits of zeros + big-endian counter.
            Suite::AesGcm => out[4..].copy_from_slice(&n.to_be_bytes()),
        }
        out
    }

    /// AEAD encrypt with an explicit 12-byte nonce (also used for the
    /// pairing PSK wrap, which uses an all-zero nonce under the suite AEAD).
    pub fn seal(self, key: &[u8; 32], nonce: &[u8; 12], ad: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
        let payload = Payload { msg: pt, aad: ad };
        match self {
            Suite::ChaChaPoly => chacha20poly1305::ChaCha20Poly1305::new(key.into())
                .encrypt(nonce.into(), payload)
                .map_err(|_| anyhow!("chacha20poly1305 encrypt failed")),
            Suite::AesGcm => aes_gcm::Aes256Gcm::new(key.into())
                .encrypt(nonce.into(), payload)
                .map_err(|_| anyhow!("aes-gcm encrypt failed")),
        }
    }

    pub fn open(self, key: &[u8; 32], nonce: &[u8; 12], ad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
        let payload = Payload { msg: ct, aad: ad };
        match self {
            Suite::ChaChaPoly => chacha20poly1305::ChaCha20Poly1305::new(key.into())
                .decrypt(nonce.into(), payload)
                .map_err(|_| anyhow!("AEAD authentication failed")),
            Suite::AesGcm => aes_gcm::Aes256Gcm::new(key.into())
                .decrypt(nonce.into(), payload)
                .map_err(|_| anyhow!("AEAD authentication failed")),
        }
    }
}

/// X25519 keypair helpers (raw 32-byte keys; the private key is clamped
/// by the scalar multiplication itself, RFC 7748).
pub fn x25519_public(private: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(*private, x25519_dalek::X25519_BASEPOINT_BYTES)
}

pub fn generate_private_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    k
}

fn dh(private: &[u8; 32], public: &[u8; 32]) -> Result<[u8; 32]> {
    let out = x25519_dalek::x25519(*private, *public);
    if out == [0u8; 32] {
        // A low-order peer point; reject rather than mixing a known value.
        bail!("X25519 produced the all-zero output (low-order point)");
    }
    Ok(out)
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// Noise HKDF (§4.3) with 2 or 3 outputs.
fn hkdf(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let temp = hmac(ck, &[ikm]);
    let o1 = hmac(&temp, &[&[1u8]]);
    let o2 = hmac(&temp, &[&o1, &[2u8]]);
    let o3 = hmac(&temp, &[&o2, &[3u8]]);
    (o1, o2, o3)
}

/// One direction's key + counter.
#[derive(Clone)]
pub struct CipherState {
    suite: Suite,
    k: Option<[u8; 32]>,
    n: u64,
}

impl Drop for CipherState {
    fn drop(&mut self) {
        if let Some(k) = self.k.as_mut() {
            k.zeroize();
        }
    }
}

impl CipherState {
    fn new(suite: Suite) -> Self {
        Self { suite, k: None, n: 0 }
    }

    fn with_key(suite: Suite, k: [u8; 32]) -> Self {
        Self { suite, k: Some(k), n: 0 }
    }

    fn encrypt_with_ad(&mut self, ad: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
        let Some(k) = self.k else {
            return Ok(pt.to_vec());
        };
        if self.n == u64::MAX {
            bail!("Noise nonce exhausted");
        }
        let ct = self.suite.seal(&k, &self.suite.nonce(self.n), ad, pt)?;
        self.n += 1;
        Ok(ct)
    }

    fn decrypt_with_ad(&mut self, ad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
        let Some(k) = self.k else {
            return Ok(ct.to_vec());
        };
        if self.n == u64::MAX {
            bail!("Noise nonce exhausted");
        }
        // The counter only advances on success (Noise §5.1).
        let pt = self.suite.open(&k, &self.suite.nonce(self.n), ad, ct)?;
        self.n += 1;
        Ok(pt)
    }
}

#[derive(Clone)]
struct SymmetricState {
    ck: [u8; 32],
    h: [u8; 32],
    cs: CipherState,
}

impl SymmetricState {
    fn new(suite: Suite) -> Self {
        let name = suite.protocol_name();
        let h: [u8; 32] = if name.len() <= 32 {
            let mut h = [0u8; 32];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h
        } else {
            Sha256::digest(name.as_bytes()).into()
        };
        Self {
            ck: h,
            h,
            cs: CipherState::new(suite),
        }
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut d = Sha256::new();
        d.update(self.h);
        d.update(data);
        self.h = d.finalize().into();
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let (ck, k, _) = hkdf(&self.ck, ikm);
        self.ck = ck;
        self.cs = CipherState::with_key(self.cs.suite, k);
    }

    fn mix_key_and_hash(&mut self, ikm: &[u8]) {
        let (ck, th, k) = hkdf(&self.ck, ikm);
        self.ck = ck;
        self.mix_hash(&th);
        self.cs = CipherState::with_key(self.cs.suite, k);
    }

    fn encrypt_and_hash(&mut self, pt: &[u8]) -> Result<Vec<u8>> {
        let h = self.h;
        let ct = self.cs.encrypt_with_ad(&h, pt)?;
        self.mix_hash(&ct);
        Ok(ct)
    }

    fn decrypt_and_hash(&mut self, ct: &[u8]) -> Result<Vec<u8>> {
        let h = self.h;
        let pt = self.cs.decrypt_with_ad(&h, ct)?;
        self.mix_hash(ct);
        Ok(pt)
    }

    fn split(&self) -> (CipherState, CipherState) {
        let (k1, k2, _) = hkdf(&self.ck, &[]);
        (
            CipherState::with_key(self.cs.suite, k1),
            CipherState::with_key(self.cs.suite, k2),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Initiator: next is write msg1. Responder: next is read msg1.
    Msg1,
    /// Initiator: next is read msg2. Responder: next is write msg2.
    Msg2,
    Done,
}

/// KKpsk2 handshake state for one side.
#[derive(Clone)]
pub struct Handshake {
    suite: Suite,
    initiator: bool,
    ss: SymmetricState,
    s: [u8; 32],
    rs: [u8; 32],
    e: Option<[u8; 32]>,
    re: Option<[u8; 32]>,
    psk: Option<[u8; 32]>,
    step: Step,
}

impl Drop for Handshake {
    fn drop(&mut self) {
        self.s.zeroize();
        if let Some(e) = self.e.as_mut() {
            e.zeroize();
        }
        if let Some(p) = self.psk.as_mut() {
            p.zeroize();
        }
    }
}

impl Handshake {
    fn new(suite: Suite, initiator: bool, local_private: &[u8; 32], remote_public: &[u8; 32], prologue: &[u8]) -> Self {
        let mut ss = SymmetricState::new(suite);
        ss.mix_hash(prologue);
        let local_public = x25519_public(local_private);
        // Pre-messages: "-> s" (initiator's static) then "<- s" (responder's).
        if initiator {
            ss.mix_hash(&local_public);
            ss.mix_hash(remote_public);
        } else {
            ss.mix_hash(remote_public);
            ss.mix_hash(&local_public);
        }
        Self {
            suite,
            initiator,
            ss,
            s: *local_private,
            rs: *remote_public,
            e: None,
            re: None,
            psk: None,
            step: Step::Msg1,
        }
    }

    /// Server side. The PSK is known before the handshake starts.
    pub fn initiator(suite: Suite, local_private: &[u8; 32], remote_public: &[u8; 32], prologue: &[u8], psk: &[u8; 32]) -> Self {
        let mut hs = Self::new(suite, true, local_private, remote_public, prologue);
        hs.psk = Some(*psk);
        hs
    }

    /// Client side. The PSK is chosen after reading message 1.
    pub fn responder(suite: Suite, local_private: &[u8; 32], remote_public: &[u8; 32], prologue: &[u8]) -> Self {
        Self::new(suite, false, local_private, remote_public, prologue)
    }

    /// Responder: supply the PSK selected from message 1's `psk_id`.
    /// Initiator: swap the PSK before reading message 2 (Sentinel fallback).
    pub fn set_psk(&mut self, psk: &[u8; 32]) {
        self.psk = Some(*psk);
    }

    /// Initiator only: `-> e, es, ss` + payload.
    pub fn write_message_1(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if !self.initiator || self.step != Step::Msg1 {
            bail!("Noise: write_message_1 out of order");
        }
        self.write_message_1_with_ephemeral(generate_private_key(), payload)
    }

    fn write_message_1_with_ephemeral(&mut self, e: [u8; 32], payload: &[u8]) -> Result<Vec<u8>> {
        let e_pub = x25519_public(&e);
        self.e = Some(e);
        let mut out = e_pub.to_vec();
        self.ss.mix_hash(&e_pub);
        self.ss.mix_key(&e_pub); // psk modifier: "e" also mixes the key
        self.ss.mix_key(&dh(&e, &self.rs)?); // es
        self.ss.mix_key(&dh(&self.s, &self.rs)?); // ss
        out.extend_from_slice(&self.ss.encrypt_and_hash(payload)?);
        self.step = Step::Msg2;
        Ok(out)
    }

    /// Responder only: process message 1 and return its payload.
    pub fn read_message_1(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        if self.initiator || self.step != Step::Msg1 {
            bail!("Noise: read_message_1 out of order");
        }
        if msg.len() < 32 + TAG_LEN {
            bail!("Noise message 1 too short");
        }
        let mut re = [0u8; 32];
        re.copy_from_slice(&msg[..32]);
        self.re = Some(re);
        self.ss.mix_hash(&re);
        self.ss.mix_key(&re);
        self.ss.mix_key(&dh(&self.s, &re)?); // es (responder side)
        self.ss.mix_key(&dh(&self.s, &self.rs)?); // ss
        let payload = self.ss.decrypt_and_hash(&msg[32..])?;
        self.step = Step::Msg2;
        Ok(payload)
    }

    /// Responder only: `<- e, ee, se, psk` + payload. Requires `set_psk`.
    pub fn write_message_2(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.initiator || self.step != Step::Msg2 {
            bail!("Noise: write_message_2 out of order");
        }
        self.write_message_2_with_ephemeral(generate_private_key(), payload)
    }

    fn write_message_2_with_ephemeral(&mut self, e: [u8; 32], payload: &[u8]) -> Result<Vec<u8>> {
        let psk = self.psk.ok_or_else(|| anyhow!("Noise: PSK not set before message 2"))?;
        let re = self.re.ok_or_else(|| anyhow!("Noise: no remote ephemeral"))?;
        let e_pub = x25519_public(&e);
        self.e = Some(e);
        let mut out = e_pub.to_vec();
        self.ss.mix_hash(&e_pub);
        self.ss.mix_key(&e_pub);
        self.ss.mix_key(&dh(&e, &re)?); // ee
        self.ss.mix_key(&dh(&e, &self.rs)?); // se (responder: own e, initiator's s)
        self.ss.mix_key_and_hash(&psk);
        out.extend_from_slice(&self.ss.encrypt_and_hash(payload)?);
        self.step = Step::Done;
        Ok(out)
    }

    /// Initiator only: process message 2 and return its payload.
    pub fn read_message_2(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
        if !self.initiator || self.step != Step::Msg2 {
            bail!("Noise: read_message_2 out of order");
        }
        if msg.len() < 32 + TAG_LEN {
            bail!("Noise message 2 too short");
        }
        let psk = self.psk.ok_or_else(|| anyhow!("Noise: PSK not set"))?;
        let e = self.e.ok_or_else(|| anyhow!("Noise: no local ephemeral"))?;
        let mut re = [0u8; 32];
        re.copy_from_slice(&msg[..32]);
        self.re = Some(re);
        self.ss.mix_hash(&re);
        self.ss.mix_key(&re);
        self.ss.mix_key(&dh(&e, &re)?); // ee
        self.ss.mix_key(&dh(&self.s, &re)?); // se (initiator: own s, responder's e)
        self.ss.mix_key_and_hash(&psk);
        let payload = self.ss.decrypt_and_hash(&msg[32..])?;
        self.step = Step::Done;
        Ok(payload)
    }

    /// The handshake hash `h` (meaningful once complete).
    pub fn handshake_hash(&self) -> [u8; 32] {
        self.ss.h
    }

    pub fn is_complete(&self) -> bool {
        self.step == Step::Done
    }

    /// Split into transport keys. Errors unless the handshake completed.
    pub fn into_transport(self) -> Result<Transport> {
        if self.step != Step::Done {
            bail!("Noise handshake not complete");
        }
        let (c1, c2) = self.ss.split();
        let (send, recv) = if self.initiator { (c1, c2) } else { (c2, c1) };
        Ok(Transport {
            suite: self.suite,
            send,
            recv,
            handshake_hash: self.ss.h,
        })
    }
}

/// Transport-mode state: one cipher per direction.
pub struct Transport {
    pub suite: Suite,
    send: CipherState,
    recv: CipherState,
    pub handshake_hash: [u8; 32],
}

impl Transport {
    pub fn encrypt(&mut self, pt: &[u8]) -> Result<Vec<u8>> {
        if pt.len() > MAX_PLAINTEXT {
            bail!("Noise plaintext of {} bytes exceeds one transport message", pt.len());
        }
        self.send.encrypt_with_ad(&[], pt)
    }

    pub fn decrypt(&mut self, ct: &[u8]) -> Result<Vec<u8>> {
        if ct.len() > MAX_NOISE_MESSAGE {
            bail!("Noise message too large");
        }
        self.recv.decrypt_with_ad(&[], ct)
    }

    /// Split into independent send/receive halves, so one thread can
    /// read while others write.
    pub fn split(self) -> (SendCipher, RecvCipher) {
        (
            SendCipher { cs: self.send.clone() },
            RecvCipher { cs: self.recv.clone() },
        )
    }
}

pub struct SendCipher {
    cs: CipherState,
}

impl SendCipher {
    pub fn encrypt(&mut self, pt: &[u8]) -> Result<Vec<u8>> {
        if pt.len() > MAX_PLAINTEXT {
            bail!("Noise plaintext of {} bytes exceeds one transport message", pt.len());
        }
        self.cs.encrypt_with_ad(&[], pt)
    }
}

pub struct RecvCipher {
    cs: CipherState,
}

impl RecvCipher {
    pub fn decrypt(&mut self, ct: &[u8]) -> Result<Vec<u8>> {
        if ct.len() > MAX_NOISE_MESSAGE {
            bail!("Noise message too large");
        }
        self.cs.decrypt_with_ad(&[], ct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair(seed: u8) -> ([u8; 32], [u8; 32]) {
        let mut k = [seed; 32];
        k[0] = seed.wrapping_add(1);
        (k, x25519_public(&k))
    }

    fn run(suite: Suite, psk_i: [u8; 32], psk_r: [u8; 32]) -> Result<(Transport, Transport, Vec<u8>, Vec<u8>)> {
        let (si, pi) = keypair(1);
        let (sr, pr) = keypair(2);
        let mut ini = Handshake::initiator(suite, &si, &pr, b"prologue", &psk_i);
        let mut res = Handshake::responder(suite, &sr, &pi, b"prologue");
        let m1 = ini.write_message_1(b"{\"psk_id\":\"x\"}")?;
        let p1 = res.read_message_1(&m1)?;
        res.set_psk(&psk_r);
        let m2 = res.write_message_2(b"{}")?;
        let p2 = ini.read_message_2(&m2)?;
        assert_eq!(ini.handshake_hash(), res.handshake_hash());
        Ok((ini.into_transport()?, res.into_transport()?, p1, p2))
    }

    #[test]
    fn handshake_and_transport_roundtrip_both_suites() {
        for suite in [Suite::ChaChaPoly, Suite::AesGcm] {
            let (mut a, mut b, p1, p2) = run(suite, [7; 32], [7; 32]).unwrap();
            assert_eq!(p1, b"{\"psk_id\":\"x\"}");
            assert_eq!(p2, b"{}");
            for i in 0..3u8 {
                let c = a.encrypt(&[i, 1, 2]).unwrap();
                assert_eq!(b.decrypt(&c).unwrap(), vec![i, 1, 2]);
                let c = b.encrypt(&[i]).unwrap();
                assert_eq!(a.decrypt(&c).unwrap(), vec![i]);
            }
        }
    }

    #[test]
    fn psk_mismatch_fails_message_2_and_fallback_state_can_retry() {
        let (si, pi) = keypair(1);
        let (sr, pr) = keypair(2);
        let mut ini = Handshake::initiator(Suite::ChaChaPoly, &si, &pr, b"p", &[1; 32]);
        let mut res = Handshake::responder(Suite::ChaChaPoly, &sr, &pi, b"p");
        let m1 = ini.write_message_1(b"{}").unwrap();
        res.read_message_1(&m1).unwrap();
        res.set_psk(&[9; 32]); // client fell back to another PSK
        let m2 = res.write_message_2(b"{}").unwrap();
        let mut retry = ini.clone();
        assert!(ini.read_message_2(&m2).is_err());
        retry.set_psk(&[9; 32]);
        assert_eq!(retry.read_message_2(&m2).unwrap(), b"{}");
    }

    #[test]
    fn tampered_transport_message_is_rejected() {
        let (mut a, mut b, _, _) = run(Suite::ChaChaPoly, [3; 32], [3; 32]).unwrap();
        let mut c = a.encrypt(b"hello").unwrap();
        c[0] ^= 1;
        assert!(b.decrypt(&c).is_err());
    }

    #[test]
    fn wrong_static_key_fails_message_1() {
        let (si, _pi) = keypair(1);
        let (sr, pr) = keypair(2);
        let (_, other_pub) = keypair(5);
        let mut ini = Handshake::initiator(Suite::ChaChaPoly, &si, &pr, b"p", &[1; 32]);
        // Responder believes the initiator has a different static key.
        let mut res = Handshake::responder(Suite::ChaChaPoly, &sr, &other_pub, b"p");
        let m1 = ini.write_message_1(b"{}").unwrap();
        assert!(res.read_message_1(&m1).is_err());
    }

    #[test]
    fn nonce_layouts_follow_the_noise_spec() {
        assert_eq!(Suite::ChaChaPoly.nonce(1), [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Suite::AesGcm.nonce(1), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    /// Cross-implementation vector generated with the Python `noiseprotocol`
    /// library (the one aiosendspin uses) from fixed static and ephemeral
    /// keys. Message bytes and handshake hash must match exactly.
    #[test]
    fn matches_noiseprotocol_reference_vector() {
        let si: [u8; 32] = core::array::from_fn(|i| i as u8);
        let sr: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_add(32));
        let ei: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_add(64));
        let er: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_add(96));
        let psk: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_add(128));
        for (suite, want_m1, want_m2, want_h) in super::test_vectors::VECTORS {
            let mut ini = Handshake::initiator(*suite, &si, &x25519_public(&sr), b"prologue-bytes", &psk);
            let mut res = Handshake::responder(*suite, &sr, &x25519_public(&si), b"prologue-bytes");
            let m1 = ini.write_message_1_with_ephemeral(ei, b"{\"psk_id\":\"abc\"}").unwrap();
            assert_eq!(hex(&m1), *want_m1, "{:?} message 1", suite);
            res.read_message_1(&m1).unwrap();
            res.set_psk(&psk);
            let m2 = res.write_message_2_with_ephemeral(er, b"{}").unwrap();
            assert_eq!(hex(&m2), *want_m2, "{:?} message 2", suite);
            ini.read_message_2(&m2).unwrap();
            assert_eq!(hex(&ini.handshake_hash()), *want_h, "{:?} h", suite);
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }
}

#[cfg(test)]
#[path = "noise_vectors.rs"]
mod test_vectors;
