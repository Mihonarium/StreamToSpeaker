//! AirPlay 2 session crypto: key derivation, the encrypted RTSP control
//! channel cipher, and per-packet audio encryption.
//!
//! After HomeKit pairing we hold a shared secret (the 64-byte SRP session
//! key K for transient pairing). From it we derive — exactly as `pair_ap`
//! / OwnTone do, so a HomePod agrees:
//!
//!   * **audio key** = `K[0..32]` (used verbatim, no HKDF).
//!   * **control write key** = HKDF-SHA512(salt=`Control-Salt`, ikm=K,
//!     info=`Control-Write-Encryption-Key`).
//!   * **control read key**  = HKDF-SHA512(salt=`Control-Salt`, ikm=K,
//!     info=`Control-Read-Encryption-Key`).
//!   * **event channel keys** = HKDF-SHA512(salt=`Events-Salt`, ikm=K, …):
//!     the receiver *writes* events with `Events-Write-Encryption-Key`
//!     (our read key) and reads our replies with
//!     `Events-Read-Encryption-Key` (our write key).
//!
//! ## Control channel framing (HAP transport)
//!
//! Once encryption is on, every RTSP request/response is split into
//! blocks of ≤ `0x400` plaintext bytes, each serialised as:
//!
//! ```text
//!   [u16 LE block_len][ChaCha20-Poly1305 ciphertext (block_len)][u8;16 tag]
//! ```
//!
//! The 2-byte length is the AEAD AAD. The 12-byte nonce is four zero bytes
//! followed by a per-direction 64-bit little-endian message counter that
//! increments once per block.
//!
//! ## Audio packets
//!
//! Each RTP audio payload is sealed with ChaCha20-Poly1305 under the audio
//! key: nonce = 4 zero bytes + a **64-bit little-endian extended sequence
//! number** — it starts at the stream's first RTP sequence number and
//! rises by one per sealed packet; AAD = RTP header bytes
//! 4..12 (timestamp + SSRC). On the wire the 16-byte tag follows the
//! ciphertext, then the **8-byte nonce (nonce[4..12]) is appended after
//! the tag** — the receiver reads it back from the packet to decrypt (it
//! never derives it), and omitting it makes every packet fail auth (silent
//! playback). Layout: header | ciphertext | tag | nonce.
//!
//! Until the 16-bit RTP sequence number first wraps, the suffix is
//! byte-identical to the sequence number itself (as it always was); after
//! the wrap it keeps counting (65536, 65537, …) where the 16-bit value
//! would restart and reuse a ChaCha20-Poly1305 nonce under the same key
//! (~8.7 min at 352 frames / 44.1 kHz), leaking the keystream. A retransmission re-sends the original on-wire bytes, so it
//! carries the original nonce and never consumes a new one.

use anyhow::{bail, Result};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use hkdf::Hkdf;
use sha2::Sha512;
use zeroize::Zeroize;

const CONTROL_SALT: &[u8] = b"Control-Salt";
const CONTROL_WRITE_INFO: &[u8] = b"Control-Write-Encryption-Key";
const CONTROL_READ_INFO: &[u8] = b"Control-Read-Encryption-Key";
const EVENTS_SALT: &[u8] = b"Events-Salt";
const EVENTS_WRITE_INFO: &[u8] = b"Events-Write-Encryption-Key";
const EVENTS_READ_INFO: &[u8] = b"Events-Read-Encryption-Key";

/// Max plaintext bytes per encrypted control block (HAP `ENCRYPTED_LEN_MAX`).
const BLOCK_MAX: usize = 0x400;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;

/// HKDF-SHA512 → fixed 32-byte output.
fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm).expect("32 is a valid HKDF-SHA512 length");
    okm
}

/// Keys derived from the pairing shared secret.
pub struct SessionKeys {
    audio: [u8; 32],
    control_write: [u8; 32],
    control_read: [u8; 32],
    events_tx: [u8; 32],
    events_rx: [u8; 32],
}

impl SessionKeys {
    /// Derive from the pairing shared secret (the 64-byte SRP session key
    /// for transient pairing; the X25519 secret for pair-verify).
    pub fn from_shared(shared: &[u8]) -> Self {
        let mut audio = [0u8; 32];
        let n = shared.len().min(32);
        audio[..n].copy_from_slice(&shared[..n]);
        Self {
            audio,
            control_write: hkdf32(CONTROL_SALT, shared, CONTROL_WRITE_INFO),
            control_read: hkdf32(CONTROL_SALT, shared, CONTROL_READ_INFO),
            // The event channel is named from the receiver's side: it
            // writes with the "Write" key, so that one is our reader.
            events_tx: hkdf32(EVENTS_SALT, shared, EVENTS_READ_INFO),
            events_rx: hkdf32(EVENTS_SALT, shared, EVENTS_WRITE_INFO),
        }
    }

    /// The 32-byte audio key sent as `shk` in the RTSP SETUP and used to
    /// seal audio packets.
    pub fn audio_key(&self) -> [u8; 32] {
        self.audio
    }

    /// Cipher for outbound (client→device) control traffic.
    pub fn control_writer(&self) -> ChannelCipher {
        ChannelCipher::new(&self.control_write)
    }

    /// Cipher for inbound (device→client) control traffic.
    pub fn control_reader(&self) -> ChannelCipher {
        ChannelCipher::new(&self.control_read)
    }

    /// `(writer, reader)` ciphers for the event channel (the TCP
    /// connection to the receiver's `eventPort`).
    pub fn event_ciphers(&self) -> (ChannelCipher, ChannelCipher) {
        (ChannelCipher::new(&self.events_tx), ChannelCipher::new(&self.events_rx))
    }

    /// The receiver's `(writer, reader)` for the event channel — the mirror
    /// of [`SessionKeys::event_ciphers`], for loopback tests.
    #[cfg(test)]
    pub fn receiver_event_ciphers(&self) -> (ChannelCipher, ChannelCipher) {
        (ChannelCipher::new(&self.events_rx), ChannelCipher::new(&self.events_tx))
    }
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.audio.zeroize();
        self.control_write.zeroize();
        self.control_read.zeroize();
        self.events_tx.zeroize();
        self.events_rx.zeroize();
    }
}

/// One direction of the encrypted control channel. Holds its own message
/// counter (the AEAD nonce source).
pub struct ChannelCipher {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl ChannelCipher {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key.into()),
            counter: 0,
        }
    }

    fn next_nonce(&mut self) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&self.counter.to_le_bytes());
        self.counter = self.counter.wrapping_add(1);
        nonce
    }

    /// Encrypt a full message into one or more framed blocks.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plaintext.len() + TAG_LEN + 2);
        // An empty message still needs no block; HAP only frames data.
        for chunk in plaintext.chunks(BLOCK_MAX).filter(|c| !c.is_empty()) {
            let len = chunk.len() as u16;
            let len_le = len.to_le_bytes();
            let nonce = self.next_nonce();
            let ct = self
                .cipher
                .encrypt(
                    (&nonce).into(),
                    Payload { msg: chunk, aad: &len_le },
                )
                .expect("chacha20poly1305 encrypt never fails");
            out.extend_from_slice(&len_le);
            out.extend_from_slice(&ct); // ciphertext + 16-byte tag
        }
        out
    }

    /// Decrypt a single block whose 2-byte length prefix has already been
    /// read. `ct_and_tag` must be exactly `block_len + 16` bytes.
    pub fn decrypt_block(&mut self, block_len: u16, ct_and_tag: &[u8]) -> Result<Vec<u8>> {
        if ct_and_tag.len() != block_len as usize + TAG_LEN {
            bail!(
                "control block size mismatch: got {}, want {}",
                ct_and_tag.len(),
                block_len as usize + TAG_LEN
            );
        }
        let len_le = block_len.to_le_bytes();
        let nonce = self.next_nonce();
        self.cipher
            .decrypt(
                (&nonce).into(),
                Payload { msg: ct_and_tag, aad: &len_le },
            )
            .map_err(|_| anyhow::anyhow!("control block auth failed (counter {})", self.counter - 1))
    }
}

/// Why [`HapReader::push`] failed. An authentication failure (wrong key,
/// corrupted or reordered record) is a different fault from the peer
/// closing the connection, and callers report it as such.
#[derive(Debug, thiserror::Error)]
pub enum HapReadError {
    #[error("encrypted record failed authentication")]
    Auth,
}

/// Incremental decoder for an inbound HAP-framed stream. Bytes arrive in
/// arbitrary pieces (a TCP read can end inside the length prefix, the
/// ciphertext or the tag); `push` buffers them and returns the plaintext
/// of every record completed so far. A timed-out read therefore never
/// loses or desynchronises data.
pub struct HapReader {
    cipher: ChannelCipher,
    pending: Vec<u8>,
}

impl HapReader {
    pub fn new(cipher: ChannelCipher) -> Self {
        Self { cipher, pending: Vec::new() }
    }

    /// True while a partial record is buffered.
    pub fn has_partial(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn push(&mut self, bytes: &[u8]) -> std::result::Result<Vec<u8>, HapReadError> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pos = 0;
        while self.pending.len() - pos >= 2 {
            let len = u16::from_le_bytes([self.pending[pos], self.pending[pos + 1]]);
            let total = 2 + len as usize + TAG_LEN;
            if self.pending.len() - pos < total {
                break;
            }
            let plain = self
                .cipher
                .decrypt_block(len, &self.pending[pos + 2..pos + total])
                .map_err(|_| HapReadError::Auth)?;
            out.extend_from_slice(&plain);
            pos += total;
        }
        self.pending.drain(..pos);
        Ok(out)
    }
}

/// Per-stream audio packet sealer: the ChaCha20-Poly1305 cipher under the
/// audio key plus the 64-bit nonce counter (an extended RTP sequence
/// number). One instance per audio stream;
/// every [`AudioSealer::seal`] consumes exactly one nonce, so no two
/// packets of the stream ever share one (the counter cannot realistically
/// wrap — 2^64 packets).
pub struct AudioSealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl AudioSealer {
    /// `first_seq` is the RTP sequence number of the stream's first
    /// packet; the counter starts there.
    pub fn new(audio_key: &[u8; 32], first_seq: u16) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(audio_key.into()),
            counter: first_seq as u64,
        }
    }

    /// Nonce counter the next sealed packet will carry.
    pub fn next_counter(&self) -> u64 {
        self.counter
    }

    /// Seal one RTP audio payload. Returns ciphertext + 16-byte tag + the
    /// 8-byte nonce suffix, to follow the 12-byte RTP header on the wire.
    /// `rtp_header` is the header already built (bytes 4..12 are the AAD).
    pub fn seal(&mut self, rtp_header: &[u8; 12], plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&self.counter.to_le_bytes());
        self.counter = self.counter.wrapping_add(1);
        let aad = &rtp_header[4..12]; // timestamp + SSRC
        let mut out = self
            .cipher
            .encrypt((&nonce).into(), Payload { msg: plaintext, aad })
            .expect("chacha20poly1305 encrypt never fails");
        // AirPlay 2 appends the 8-byte nonce (nonce[4..12]) after the tag so
        // the receiver can decrypt. Without it the receiver fails the
        // Poly1305 auth tag on every packet and silently drops the audio —
        // the session still shows "playing" but no sound. Layout:
        // [RTP header][ciphertext][16-byte tag][8-byte nonce].
        out.extend_from_slice(&nonce[4..12]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_channel_roundtrip_single_block() {
        let keys = SessionKeys::from_shared(&[0x5a; 64]);
        let mut tx = keys.control_writer();
        // The reader on the *other* end uses the matching key; here we
        // simulate the wire with a second cipher created from the same key
        // so we can validate the frame format + counter.
        let mut rx = ChannelCipher::new(&hkdf32(CONTROL_SALT, &[0x5a; 64], CONTROL_WRITE_INFO));

        let msg = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let wire = tx.encrypt(msg);
        // Parse one frame.
        let len = u16::from_le_bytes([wire[0], wire[1]]);
        let got = rx.decrypt_block(len, &wire[2..]).unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn control_channel_roundtrip_multi_block() {
        let keys = SessionKeys::from_shared(&[1u8; 64]);
        let mut tx = keys.control_writer();
        let mut rx = ChannelCipher::new(&hkdf32(CONTROL_SALT, &[1u8; 64], CONTROL_WRITE_INFO));

        // 2500 bytes → three blocks (0x400, 0x400, rest).
        let msg: Vec<u8> = (0..2500u32).map(|i| (i % 256) as u8).collect();
        let wire = tx.encrypt(&msg);

        let mut out = Vec::new();
        let mut p = 0;
        while p < wire.len() {
            let len = u16::from_le_bytes([wire[p], wire[p + 1]]);
            p += 2;
            let block = &wire[p..p + len as usize + TAG_LEN];
            p += len as usize + TAG_LEN;
            out.extend_from_slice(&rx.decrypt_block(len, block).unwrap());
        }
        assert_eq!(out, msg);
    }

    #[test]
    fn wrong_counter_fails_auth() {
        let keys = SessionKeys::from_shared(&[2u8; 64]);
        let mut tx = keys.control_writer();
        let mut rx = ChannelCipher::new(&hkdf32(CONTROL_SALT, &[2u8; 64], CONTROL_WRITE_INFO));
        let _ = tx.encrypt(b"first"); // advances tx counter to 1
        let wire = tx.encrypt(b"second"); // encrypted with counter 1
        // rx is at counter 0 → nonce mismatch → auth fails.
        let len = u16::from_le_bytes([wire[0], wire[1]]);
        assert!(rx.decrypt_block(len, &wire[2..]).is_err());
    }

    #[test]
    fn hap_reader_reassembles_records_split_anywhere() {
        let keys = SessionKeys::from_shared(&[4u8; 64]);
        let mut tx = keys.control_writer();
        let msg_a = b"POST /command RTSP/1.0\r\nCSeq: 3\r\n\r\n".to_vec();
        let msg_b: Vec<u8> = (0..1500u32).map(|i| (i * 7) as u8).collect();
        let mut wire = tx.encrypt(&msg_a);
        wire.extend_from_slice(&tx.encrypt(&msg_b));
        let mut want = msg_a.clone();
        want.extend_from_slice(&msg_b);
        let first_len = 2 + msg_a.len() + TAG_LEN;
        // Split inside the length prefix, the ciphertext and the tag of
        // the first record, plus every-byte feeding.
        for cuts in [
            vec![1],
            vec![10],
            vec![first_len - 3],
            vec![first_len + 1, first_len + 600],
            (1..wire.len()).collect::<Vec<_>>(),
        ] {
            let mut rx = HapReader::new(keys.control_writer());
            let mut got = Vec::new();
            let mut last = 0;
            for c in cuts.into_iter().chain([wire.len()]) {
                got.extend(rx.push(&wire[last..c]).unwrap());
                last = c;
            }
            assert_eq!(got, want);
            assert!(!rx.has_partial());
        }
    }

    #[test]
    fn hap_reader_wrong_key_is_auth_error() {
        let mut tx = SessionKeys::from_shared(&[4u8; 64]).control_writer();
        let wire = tx.encrypt(b"hello");
        let mut rx = HapReader::new(SessionKeys::from_shared(&[5u8; 64]).control_writer());
        assert!(matches!(rx.push(&wire), Err(HapReadError::Auth)));
    }

    #[test]
    fn event_keys_are_mirrored_between_sides() {
        // What the receiver writes with "Events-Write" we must read, and
        // vice versa — the two directions use distinct keys.
        let keys = SessionKeys::from_shared(&[6u8; 64]);
        let (mut our_tx, _) = keys.event_ciphers();
        let mut receiver_rx = ChannelCipher::new(&hkdf32(EVENTS_SALT, &[6u8; 64], EVENTS_READ_INFO));
        let wire = our_tx.encrypt(b"RTSP/1.0 200 OK\r\n\r\n");
        let len = u16::from_le_bytes([wire[0], wire[1]]);
        assert!(receiver_rx.decrypt_block(len, &wire[2..]).is_ok());
        let mut receiver_tx = ChannelCipher::new(&hkdf32(EVENTS_SALT, &[6u8; 64], EVENTS_WRITE_INFO));
        let (_, our_rx) = keys.event_ciphers();
        let mut reader = HapReader::new(our_rx);
        assert_eq!(reader.push(&receiver_tx.encrypt(b"ev")).unwrap(), b"ev");
    }

    #[test]
    fn audio_seal_appends_tag_and_counter_suffix() {
        let mut sealer = AudioSealer::new(&[7u8; 32], 0);
        let header = [0x80, 0x60, 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78];
        let plain = vec![0xABu8; 1416];
        let s0 = sealer.seal(&header, &plain);
        let s1 = sealer.seal(&header, &plain);
        // ciphertext + 16-byte tag + 8-byte appended nonce
        assert_eq!(s0.len(), plain.len() + TAG_LEN + 8);
        // Same header and payload, next counter → different ciphertext.
        assert_ne!(s0, s1);
        assert_eq!(&s0[s0.len() - 8..], &0u64.to_le_bytes());
        assert_eq!(&s1[s1.len() - 8..], &1u64.to_le_bytes());
        assert_eq!(sealer.next_counter(), 2);
    }

    /// Open a sealed packet the way a receiver does: nonce from the 8-byte
    /// suffix, AAD from the header.
    fn open(key: &[u8; 32], header: &[u8; 12], sealed: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&sealed[sealed.len() - 8..]);
        let cipher = ChaCha20Poly1305::new(key.into());
        cipher
            .decrypt(
                (&nonce).into(),
                Payload { msg: &sealed[..sealed.len() - 8], aad: &header[4..12] },
            )
            .unwrap()
    }

    #[test]
    fn audio_seal_roundtrips_with_matching_nonce_aad() {
        let key = [9u8; 32];
        let header = [0x80, 0x60, 0x11, 0x22, 0x01, 0x02, 0x03, 0x04, 0x0A, 0x0B, 0x0C, 0x0D];
        let plain = b"hello airplay 2 audio".to_vec();
        let mut sealer = AudioSealer::new(&key, 0x1234);
        for _ in 0..3 {
            let sealed = sealer.seal(&header, &plain);
            assert_eq!(open(&key, &header, &sealed), plain);
        }
    }

    #[test]
    fn audio_nonces_never_repeat_across_seq_wrap() {
        // Drive a stream through two full 16-bit sequence wraps: the RTP
        // seq repeats, the nonce suffix must not.
        let key = [3u8; 32];
        let mut sealer = AudioSealer::new(&key, 65_000);
        let mut seen = std::collections::HashSet::new();
        let mut seq: u16 = 65_000;
        let total = 2 * 65_536 + 1_000;
        for i in 0..total {
            let mut header = [0u8; 12];
            header[0] = 0x80;
            header[1] = 0x60;
            header[2..4].copy_from_slice(&seq.to_be_bytes());
            header[4..8].copy_from_slice(&(i as u32).wrapping_mul(352).to_be_bytes());
            let sealed = sealer.seal(&header, &[0u8; 4]);
            let suffix: [u8; 8] = sealed[sealed.len() - 8..].try_into().unwrap();
            assert!(seen.insert(suffix), "nonce reused at packet {i} (seq {seq})");
            if i < 536 {
                // Before the first wrap: exactly the old seq-derived suffix.
                assert_eq!(suffix, old_seq_suffix(seq));
            }
            seq = seq.wrapping_add(1);
        }
        assert_eq!(sealer.next_counter(), 65_000 + total as u64);
    }

    /// The suffix the seq-derived nonce used to carry: seq (LE) in the
    /// low two bytes, zeros elsewhere.
    fn old_seq_suffix(seq: u16) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&(seq as u32).to_le_bytes());
        out
    }

    #[test]
    fn audio_nonce_matches_seq_until_first_wrap() {
        let mut sealer = AudioSealer::new(&[2u8; 32], 0);
        let header = [0x80, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        for seq in 0..=u16::MAX {
            let sealed = sealer.seal(&header, &[]);
            assert_eq!(sealed[sealed.len() - 8..], old_seq_suffix(seq));
        }
        // The next one would have reused seq 0's nonce; it doesn't.
        let sealed = sealer.seal(&header, &[]);
        assert_eq!(&sealed[sealed.len() - 8..], &65_536u64.to_le_bytes());
    }
}
