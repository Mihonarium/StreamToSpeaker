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
//! key: nonce = 4 zero bytes + the RTP sequence number (little-endian) in
//! bytes 4..8 + 4 zero bytes; AAD = RTP header bytes 4..12 (timestamp +
//! SSRC); the 16-byte tag is appended after the ciphertext. (Verified
//! against OwnTone `airplay.c`.)

use anyhow::{bail, Result};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use hkdf::Hkdf;
use sha2::Sha512;
use zeroize::Zeroize;

const CONTROL_SALT: &[u8] = b"Control-Salt";
const CONTROL_WRITE_INFO: &[u8] = b"Control-Write-Encryption-Key";
const CONTROL_READ_INFO: &[u8] = b"Control-Read-Encryption-Key";

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
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.audio.zeroize();
        self.control_write.zeroize();
        self.control_read.zeroize();
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

/// Seal one RTP audio payload with the audio key. Returns ciphertext with
/// the 16-byte Poly1305 tag appended (to follow the 12-byte RTP header on
/// the wire). `rtp_header` is the 12-byte header already built; `seq` is
/// the RTP sequence number used to build the nonce.
pub fn seal_audio(audio_key: &[u8; 32], rtp_header: &[u8; 12], seq: u16, plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(audio_key.into());
    let mut nonce = [0u8; 12];
    // seqnum at bytes 4..8 (little-endian); a u16 fits in the low two.
    nonce[4..8].copy_from_slice(&(seq as u32).to_le_bytes());
    let aad = &rtp_header[4..12]; // timestamp + SSRC
    cipher
        .encrypt((&nonce).into(), Payload { msg: plaintext, aad })
        .expect("chacha20poly1305 encrypt never fails")
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
    fn audio_seal_appends_tag_and_is_unique_per_seq() {
        let key = [7u8; 32];
        let header = [0x80, 0x60, 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78];
        let plain = vec![0xABu8; 1416];
        let s0 = seal_audio(&key, &header, 1, &plain);
        let s1 = seal_audio(&key, &header, 2, &plain);
        // ciphertext + 16-byte tag
        assert_eq!(s0.len(), plain.len() + TAG_LEN);
        // Different sequence → different nonce → different ciphertext.
        assert_ne!(s0, s1);
    }

    #[test]
    fn audio_seal_roundtrips_with_matching_nonce_aad() {
        let key = [9u8; 32];
        let header = [0x80, 0x60, 0x11, 0x22, 0x01, 0x02, 0x03, 0x04, 0x0A, 0x0B, 0x0C, 0x0D];
        let plain = b"hello airplay 2 audio".to_vec();
        let sealed = seal_audio(&key, &header, 0x1234, &plain);

        // Reconstruct the nonce + AAD a receiver would and decrypt.
        let cipher = ChaCha20Poly1305::new((&key).into());
        let mut nonce = [0u8; 12];
        nonce[4..8].copy_from_slice(&(0x1234u32).to_le_bytes());
        let aad = &header[4..12];
        let opened = cipher
            .decrypt((&nonce).into(), Payload { msg: &sealed, aad })
            .unwrap();
        assert_eq!(opened, plain);
    }
}
