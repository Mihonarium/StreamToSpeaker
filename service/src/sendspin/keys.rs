//! Identity keys, PSK identifiers and pairing tokens.
//!
//! * `client_id` / `server_id` are the base64url (no padding) X25519
//!   public keys — 43 characters.
//! * `psk_id = base64url(SHA-256("sendspin-psk-id-v1" || PSK))` lets the
//!   client pick the PSK named in Noise message 1.
//! * The Sentinel PSK (`SHA-256("sendspin-sentinel-psk-v1")`) is the
//!   published "no credential" PSK used for unpaired connections.
//! * A version-0 pairing token (`SP:0…`) carries `client_key || pairing_psk`
//!   in base32 with `2`→`9` transliteration, so an operator can paste or
//!   scan it into a server.

use anyhow::{bail, Result};
use base64::Engine;
use sha2::{Digest, Sha256};

pub const PSK_ID_LABEL: &[u8] = b"sendspin-psk-id-v1";
pub const SENTINEL_LABEL: &[u8] = b"sendspin-sentinel-psk-v1";

/// base64url without padding.
pub fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Decode base64url, tolerating (but not requiring) `=` padding.
pub fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    let trimmed = s.trim_end_matches('=');
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed)?)
}

/// Decode a base64url field that must be exactly 32 bytes.
pub fn b64url_32(s: &str) -> Result<[u8; 32]> {
    let v = b64url_decode(s)?;
    if v.len() != 32 {
        bail!("expected 32 bytes, got {}", v.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Parse a 43-character peer id into its raw public key.
pub fn peer_id_to_key(id: &str) -> Result<[u8; 32]> {
    if id.len() != 43 {
        bail!("peer id must be 43 characters, got {}", id.len());
    }
    b64url_32(id)
}

pub fn sentinel_psk() -> [u8; 32] {
    Sha256::digest(SENTINEL_LABEL).into()
}

pub fn psk_id(psk: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(PSK_ID_LABEL);
    h.update(psk);
    b64url(&h.finalize())
}

/// Which kind of PSK keyed a connection. Spec codes: `lt` / `pr` / `sn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PskCategory {
    LongTerm,
    Pairing,
    Sentinel,
}

impl PskCategory {
    pub fn wire(self) -> &'static str {
        match self {
            PskCategory::LongTerm => "lt",
            PskCategory::Pairing => "pr",
            PskCategory::Sentinel => "sn",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "lt" => Some(PskCategory::LongTerm),
            "pr" => Some(PskCategory::Pairing),
            "sn" => Some(PskCategory::Sentinel),
            _ => None,
        }
    }
}

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => bail!("invalid base32 character {:?}", c as char),
        };
        buffer = (buffer << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

/// Encode a version-0 pairing token for `client_public || pairing_psk`.
pub fn encode_pairing_token(client_public: &[u8; 32], pairing_psk: &[u8; 32]) -> String {
    let mut payload = Vec::with_capacity(64);
    payload.extend_from_slice(client_public);
    payload.extend_from_slice(pairing_psk);
    format!("SP:0{}", base32_encode(&payload).replace('2', "9"))
}

/// Decode a version-0 pairing token (lenient with whitespace, case and the
/// `SP:` prefix). Returns `(client_public, pairing_psk)`.
pub fn decode_pairing_token(token: &str) -> Result<([u8; 32], [u8; 32])> {
    let t = token.trim().to_ascii_uppercase();
    let t: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    let t = t.strip_prefix("SP:").unwrap_or(&t);
    let mut chars = t.chars();
    match chars.next() {
        Some('0') => {}
        Some(v) => bail!("unsupported pairing token version {:?}", v),
        None => bail!("empty pairing token"),
    }
    let body: String = chars.as_str().replace('9', "2");
    let payload = base32_decode(&body)?;
    if payload.len() < 64 {
        bail!("pairing token too short");
    }
    let mut key = [0u8; 32];
    let mut psk = [0u8; 32];
    key.copy_from_slice(&payload[..32]);
    psk.copy_from_slice(&payload[32..64]);
    Ok((key, psk))
}

/// Present a pairing token for reading/typing: groups of 4 after the prefix.
pub fn format_token_for_display(token: &str) -> String {
    let (prefix, body) = token.split_at(token.len().min(4));
    let groups: Vec<String> = body
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    format!("{} {}", prefix, groups.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_matches_published_constants() {
        let psk = sentinel_psk();
        let hex: String = psk.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(hex, "1b5e24dbc1aed95fc2a5a338a90c05df44bd10f5ec1f4cd66cbf86272767b9d3");
        assert_eq!(psk_id(&psk), "GFsV9tLaSQm9HcFWpKsgYQOr7wFTvNUtkmFwuVz3zoo");
    }

    #[test]
    fn pairing_token_reference_vector() {
        // Spec "Pairing PSK Flow": client_key = 00..1f, pairing_psk = e0..ff.
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let psk: [u8; 32] = core::array::from_fn(|i| 0xe0 + i as u8);
        let token = encode_pairing_token(&key, &psk);
        assert_eq!(
            token,
            "SP:0AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQTCQKRMFYYDENBWHA5DYP6BYPC4PSOLZXH5DU6V97M5XXO74HR6LZ7J5PW674PT6X37T6757Y"
        );
        let (k2, p2) = decode_pairing_token(&format!("  {} ", token.to_lowercase())).unwrap();
        assert_eq!(k2, key);
        assert_eq!(p2, psk);
        // Display grouping survives a paste back.
        let (k3, _) = decode_pairing_token(&format_token_for_display(&token)).unwrap();
        assert_eq!(k3, key);
    }

    #[test]
    fn bad_tokens_are_rejected() {
        assert!(decode_pairing_token("SP:1AAAA").is_err());
        assert!(decode_pairing_token("SP:0AAAA").is_err());
        assert!(decode_pairing_token("").is_err());
    }

    #[test]
    fn b64url_roundtrip_and_peer_ids() {
        let k = [0xfbu8; 32];
        let id = b64url(&k);
        assert_eq!(id.len(), 43);
        assert!(!id.contains('='));
        assert_eq!(peer_id_to_key(&id).unwrap(), k);
        assert!(peer_id_to_key("short").is_err());
    }
}
