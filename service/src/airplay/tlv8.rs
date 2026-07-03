//! HomeKit TLV8 encoding/decoding for the AirPlay 2 pairing handshake.
//!
//! TLV8 is HAP's wire format for pair-setup / pair-verify message bodies:
//! a flat sequence of `(type: u8, length: u8, value: [length bytes])`
//! items. Two rules matter for us:
//!
//!   * **Values longer than 255 bytes are split** into consecutive items
//!     of the *same* type, each carrying ≤255 bytes. The reader
//!     concatenates runs of identical types back into one value. (The SRP
//!     public keys are 384 bytes, so this is mandatory, not optional.)
//!   * A zero-length item is legal (used for some acknowledgements).
//!
//! Type codes are from `pair_ap`/HAP (`pair-tlv.h`).

use anyhow::{bail, Result};
use std::collections::HashMap;

// HAP TLV types we use.
pub const TYPE_METHOD: u8 = 0x00;
pub const TYPE_IDENTIFIER: u8 = 0x01;
pub const TYPE_SALT: u8 = 0x02;
pub const TYPE_PUBLIC_KEY: u8 = 0x03;
pub const TYPE_PROOF: u8 = 0x04;
pub const TYPE_ENCRYPTED_DATA: u8 = 0x05;
pub const TYPE_STATE: u8 = 0x06;
pub const TYPE_ERROR: u8 = 0x07;
pub const TYPE_SIGNATURE: u8 = 0x0a;
pub const TYPE_FLAGS: u8 = 0x13;
pub const TYPE_SEPARATOR: u8 = 0xff;

// Method values.
pub const METHOD_PAIR_SETUP: u8 = 0x00;

// Transient pairing flag (PairingFlagsTransient).
pub const FLAG_TRANSIENT: u8 = 0x10;

/// Decoded TLV items, keyed by type (runs of a type already concatenated).
#[derive(Debug, Default, Clone)]
pub struct Tlv {
    map: HashMap<u8, Vec<u8>>,
}

impl Tlv {
    pub fn get(&self, ty: u8) -> Option<&[u8]> {
        self.map.get(&ty).map(|v| v.as_slice())
    }

    pub fn get_u8(&self, ty: u8) -> Option<u8> {
        self.map.get(&ty).and_then(|v| v.first().copied())
    }

    pub fn state(&self) -> Option<u8> {
        self.get_u8(TYPE_STATE)
    }

    /// HAP error code if the peer reported one (non-zero).
    pub fn error(&self) -> Option<u8> {
        self.get_u8(TYPE_ERROR).filter(|&e| e != 0)
    }

    /// Parse a TLV8 byte stream. Consecutive items of the same type are
    /// concatenated (the 255-byte fragmentation rule). A `Separator`
    /// (0xff) item ends a run so the *next* same-type item starts fresh —
    /// we don't produce multi-valued types, but we honour the boundary by
    /// ignoring separators (any real duplicate-type-after-separator would
    /// overwrite, which none of our messages contain).
    pub fn decode(bytes: &[u8]) -> Result<Tlv> {
        let mut map: HashMap<u8, Vec<u8>> = HashMap::new();
        let mut i = 0;
        let mut last_type: Option<u8> = None;
        while i < bytes.len() {
            let ty = bytes[i];
            if i + 1 >= bytes.len() {
                bail!("TLV8 truncated: type 0x{:02x} with no length byte", ty);
            }
            let len = bytes[i + 1] as usize;
            let start = i + 2;
            let end = start + len;
            if end > bytes.len() {
                bail!(
                    "TLV8 truncated: type 0x{:02x} wants {} bytes, {} left",
                    ty,
                    len,
                    bytes.len() - start
                );
            }
            if ty == TYPE_SEPARATOR {
                last_type = None;
                i = end;
                continue;
            }
            // Concatenate only when this is a continuation of the *same*
            // type as the immediately preceding item (the fragmentation
            // rule); otherwise start a new value.
            let entry = map.entry(ty).or_default();
            if last_type == Some(ty) {
                entry.extend_from_slice(&bytes[start..end]);
            } else {
                entry.clear();
                entry.extend_from_slice(&bytes[start..end]);
            }
            last_type = Some(ty);
            i = end;
        }
        Ok(Tlv { map })
    }
}

/// Builder for an ordered TLV8 message. Order matters on the wire (some
/// receivers are sensitive to it), so we keep an explicit vector rather
/// than a map.
#[derive(Default)]
pub struct TlvBuilder {
    items: Vec<(u8, Vec<u8>)>,
}

impl TlvBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_u8(mut self, ty: u8, val: u8) -> Self {
        self.items.push((ty, vec![val]));
        self
    }

    pub fn add(mut self, ty: u8, val: &[u8]) -> Self {
        self.items.push((ty, val.to_vec()));
        self
    }

    /// Serialise, splitting any value >255 bytes into consecutive
    /// same-type fragments of ≤255 bytes each.
    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (ty, val) in &self.items {
            if val.is_empty() {
                out.push(*ty);
                out.push(0);
                continue;
            }
            for chunk in val.chunks(255) {
                out.push(*ty);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let bytes = TlvBuilder::new()
            .add_u8(TYPE_STATE, 1)
            .add_u8(TYPE_METHOD, METHOD_PAIR_SETUP)
            .add_u8(TYPE_FLAGS, FLAG_TRANSIENT)
            .build();
        let tlv = Tlv::decode(&bytes).unwrap();
        assert_eq!(tlv.state(), Some(1));
        assert_eq!(tlv.get_u8(TYPE_METHOD), Some(0));
        assert_eq!(tlv.get_u8(TYPE_FLAGS), Some(0x10));
    }

    #[test]
    fn fragments_long_value_and_reassembles() {
        // 384-byte SRP public key must split into 255 + 129.
        let key: Vec<u8> = (0..384u16).map(|i| (i % 251) as u8).collect();
        let bytes = TlvBuilder::new().add(TYPE_PUBLIC_KEY, &key).build();
        // Expect two fragments: [3,255,...][3,129,...]
        assert_eq!(bytes[0], TYPE_PUBLIC_KEY);
        assert_eq!(bytes[1], 255);
        assert_eq!(bytes[2 + 255], TYPE_PUBLIC_KEY);
        assert_eq!(bytes[2 + 255 + 1], 129);
        let tlv = Tlv::decode(&bytes).unwrap();
        assert_eq!(tlv.get(TYPE_PUBLIC_KEY).unwrap(), &key[..]);
    }

    #[test]
    fn exact_255_value_is_single_fragment() {
        let v = vec![7u8; 255];
        let bytes = TlvBuilder::new().add(TYPE_SALT, &v).build();
        assert_eq!(bytes.len(), 2 + 255);
        let tlv = Tlv::decode(&bytes).unwrap();
        assert_eq!(tlv.get(TYPE_SALT).unwrap().len(), 255);
    }

    #[test]
    fn error_helper_ignores_zero() {
        let ok = Tlv::decode(&TlvBuilder::new().add_u8(TYPE_ERROR, 0).build()).unwrap();
        assert_eq!(ok.error(), None);
        let err = Tlv::decode(&TlvBuilder::new().add_u8(TYPE_ERROR, 2).build()).unwrap();
        assert_eq!(err.error(), Some(2));
    }

    #[test]
    fn decode_truncated_errors() {
        assert!(Tlv::decode(&[TYPE_STATE]).is_err()); // no length
        assert!(Tlv::decode(&[TYPE_STATE, 4, 1, 2]).is_err()); // short value
    }

    #[test]
    fn separator_resets_run() {
        // Two distinct same-type values separated by 0xff should NOT be
        // concatenated; we keep the last one.
        let mut bytes = TlvBuilder::new().add(TYPE_IDENTIFIER, b"aa").build();
        bytes.push(TYPE_SEPARATOR);
        bytes.push(0);
        bytes.extend_from_slice(&TlvBuilder::new().add(TYPE_IDENTIFIER, b"bb").build());
        let tlv = Tlv::decode(&bytes).unwrap();
        assert_eq!(tlv.get(TYPE_IDENTIFIER).unwrap(), b"bb");
    }
}
