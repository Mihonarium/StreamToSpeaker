//! Crypto helpers for the RAOP audio path.
//!
//! Two layers:
//!   1. **Session-key wrap** — at ANNOUNCE time we generate a random
//!      128-bit AES key + 128-bit IV, RSA-OAEP-SHA1-encrypt the key
//!      under Apple's published AirPort Express public RSA key (2048-
//!      bit), and ship the 256-byte ciphertext in the SDP `rsaaeskey`
//!      attribute. The receiver has the matching private key baked
//!      into firmware and decrypts.
//!   2. **Per-packet AES-128-CBC** — every audio packet's payload is
//!      encrypted in CBC mode with the session key + IV. CBC means
//!      only the leading `len & ~15` bytes are encrypted; any trailing
//!      `len & 15` bytes are sent as plaintext (verified against
//!      shairport-sync receiver source). The IV is reset to the
//!      original `aes_iv` at the start of every packet — there is no
//!      CBC chaining across packets.
//!
//! The RSA modulus + exponent are the values from `et=1, vn=3`
//! advertised by every shipping RAOP receiver. The same values appear
//! in PipeWire's `module-raop-sink`, openairplay/node_airtunes,
//! shairport-sync, and dozens of other open-source implementations.
//! They're not secret — Apple's IP claim was on the private half,
//! which was extracted from leaked AirPort Express firmware ~2004.

use aes::cipher::{BlockEncryptMut, KeyIvInit};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rand::RngCore;
use rsa::{BigUint, Oaep, RsaPublicKey};
use sha1::Sha1;

/// Apple AirTunes RSA-2048 public modulus, base64.
///
/// Verified across PipeWire `module-raop-sink.c`,
/// `openairplay/node_airtunes`, and the Airtunes2 spec — three
/// independent sources that all carry the same 256-byte value below.
/// Note: an older copy in `LinusU/rust-raop-player` has a 1-char typo
/// (`EpVviyhnimNV…` vs the canonical `EpVviYnhimNV…`) — we use the
/// canonical version.
const APPLE_RSA_MODULUS_B64: &str = "59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Qg90u3sG/1CUtwC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6DZHJ2YCCLlDRKSKv6kDqnw4UwPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLvqr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf/+sG+NCK3eQJVxqcaJ/vEHKIVd2M+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/UAaHqn9JdsBWLUEpVviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEew==";

/// Apple AirTunes public exponent — the conventional 65537 in base64.
const APPLE_RSA_EXPONENT_B64: &str = "AQAB";

/// 16-byte AES-128 session key and matching IV. Lives for one RAOP
/// session and is reused for every audio packet.
#[derive(Debug, Clone)]
pub struct SessionKey {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

impl SessionKey {
    /// Roll a fresh random AES key + IV.
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut iv);
        Self { key, iv }
    }

    /// RSA-OAEP-SHA1 encrypt this session's AES key under Apple's
    /// public RSA key. Output is the raw 256-byte ciphertext (2048-bit
    /// modulus), ready to be base64-without-padding-encoded for the
    /// SDP attribute.
    pub fn rsa_wrapped_key(&self) -> Result<Vec<u8>> {
        let pubkey = apple_rsa_public_key()?;
        let padding = Oaep::new::<Sha1>();
        let mut rng = rand::thread_rng();
        pubkey
            .encrypt(&mut rng, padding, &self.key)
            .map_err(|e| anyhow!("RSA-OAEP encrypting session AES key: {}", e))
    }
}

/// Reconstruct the Apple RSA public key from the embedded modulus +
/// exponent. We rebuild each call rather than cache (single-shot per
/// session start, well under 1 ms even on Windows).
fn apple_rsa_public_key() -> Result<RsaPublicKey> {
    let mod_bytes = base64::engine::general_purpose::STANDARD
        .decode(APPLE_RSA_MODULUS_B64)
        .context("decoding Apple RSA modulus base64")?;
    let exp_bytes = base64::engine::general_purpose::STANDARD
        .decode(APPLE_RSA_EXPONENT_B64)
        .context("decoding Apple RSA exponent base64")?;
    let n = BigUint::from_bytes_be(&mod_bytes);
    let e = BigUint::from_bytes_be(&exp_bytes);
    RsaPublicKey::new(n, e).map_err(|e| anyhow!("constructing Apple RSA public key: {}", e))
}

/// Encrypt one audio packet's payload in-place with AES-128-CBC.
///
/// **Only the leading `len & !15` bytes — the part that's a whole
/// number of 16-byte blocks — are encrypted.** Any trailing `len &
/// 15` bytes are left as plaintext. This matches the RAOP convention
/// as implemented by every receiver (see shairport-sync's
/// `openssl_aes_decrypt_cbc` callsite).
///
/// The IV is the per-session IV (i.e. CBC does not chain across
/// packets — each packet is encrypted with the same starting IV).
pub fn encrypt_audio_packet_in_place(buf: &mut [u8], key: &SessionKey) {
    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
    let blocks = buf.len() / 16;
    if blocks == 0 {
        return;
    }
    let mut enc = Aes128CbcEnc::new(&key.key.into(), &key.iv.into());
    // Encrypt block-by-block in-place. We can't use encrypt_padded_mut
    // because that always adds a padding scheme — RAOP wants raw
    // block-aligned CBC with no padding and no trailing rewrite.
    for chunk in buf[..blocks * 16].chunks_exact_mut(16) {
        let block = aes::cipher::generic_array::GenericArray::from_mut_slice(chunk);
        enc.encrypt_block_mut(block);
    }
}

/// Generate a random 16-byte challenge for the RTSP OPTIONS
/// Apple-Challenge header. We don't currently verify the receiver's
/// Apple-Response (it'd need a per-receiver key for verification);
/// just sending the challenge is what makes receivers consider us a
/// well-behaved client.
pub fn random_apple_challenge() -> [u8; 16] {
    let mut rng = rand::thread_rng();
    let mut nonce = [0u8; 16];
    rng.fill_bytes(&mut nonce);
    nonce
}

/// Base64-encode without trailing padding — the RAOP convention for
/// SDP attributes (`rsaaeskey`, `aesiv`) and the Apple-Challenge
/// header value.
pub fn base64_nopad(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_rsa_key_parses_and_is_2048_bit() {
        use rsa::traits::PublicKeyParts;
        let pk = apple_rsa_public_key().expect("Apple RSA public key parses");
        // 2048 bits / 8 = 256 bytes
        assert_eq!(pk.size(), 256, "expected 2048-bit modulus");
    }

    #[test]
    fn rsa_wrap_produces_256_byte_ciphertext() {
        let sk = SessionKey::random();
        let wrapped = sk.rsa_wrapped_key().expect("wrap");
        assert_eq!(wrapped.len(), 256, "2048-bit RSA → 256-byte ciphertext");
    }

    #[test]
    fn cbc_encrypts_only_aligned_prefix_leaves_tail_plaintext() {
        let sk = SessionKey {
            key: [0xAA; 16],
            iv: [0x55; 16],
        };
        // 35 bytes = two whole blocks (32) + 3 trailing plaintext bytes.
        let mut buf = vec![0u8; 35];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        let trail = buf[32..].to_vec();
        encrypt_audio_packet_in_place(&mut buf, &sk);
        // The trailing 3 bytes must be unchanged.
        assert_eq!(&buf[32..], &trail[..]);
        // The first 32 bytes must NOT be 0..32 anymore.
        let pre: Vec<u8> = (0..32u8).collect();
        assert_ne!(&buf[..32], &pre[..]);
    }

    #[test]
    fn cbc_resets_iv_per_call() {
        // Same input + same key + same starting IV ⇒ same output.
        // Confirms we're not carrying CBC state between calls.
        let sk = SessionKey {
            key: [1; 16],
            iv: [2; 16],
        };
        let payload = vec![0xABu8; 32];
        let mut a = payload.clone();
        let mut b = payload.clone();
        encrypt_audio_packet_in_place(&mut a, &sk);
        encrypt_audio_packet_in_place(&mut b, &sk);
        assert_eq!(a, b);
    }

    #[test]
    fn base64_nopad_drops_padding() {
        assert_eq!(base64_nopad(&[1, 2]), "AQI");
        assert_eq!(base64_nopad(&[1, 2, 3]), "AQID");
    }
}
