//! AirPlay 2 HomeKit **transient** pair-setup state machine.
//!
//! Transient pairing is the PIN-less path (fixed PIN `3939`, flag `0x10`)
//! that HomePods and other AP2 receivers accept for casual streaming. It
//! is a 4-message SRP exchange — no Ed25519 long-term keys, no `pair-verify`
//! reconnect, no FairPlay — after which both sides share the SRP session
//! key and the RTSP channel becomes ChaCha20-Poly1305 encrypted.
//!
//! ```text
//!   POST /pair-setup  M1: State=1, Method=0, Flags=0x10
//!   ← response        M2: State=2, Salt(16), PublicKey(B, 384)
//!   POST /pair-setup  M3: State=3, PublicKey(A, 384), Proof(M1, 64)
//!   ← response        M4: State=4, Proof(M2, 64)            [verify]
//!   ⇒ SessionKeys derived from SRP K (64 bytes)
//! ```
//!
//! This type is transport-agnostic: it produces request bodies and
//! consumes response bodies, so `ap2_rtsp` can drive it over the wire and
//! we can unit-test the message construction in isolation.

use anyhow::{anyhow, bail, Context, Result};

use crate::airplay::ap2_crypto::SessionKeys;
use crate::airplay::srp::SrpClient;
use crate::airplay::tlv8::{
    Tlv, TlvBuilder, FLAG_TRANSIENT, METHOD_PAIR_SETUP, TYPE_FLAGS, TYPE_METHOD, TYPE_PROOF,
    TYPE_PUBLIC_KEY, TYPE_SALT, TYPE_STATE,
};

/// HTTP header AirPlay 2 senders attach to pair-setup / pair-verify
/// requests. `4` selects the CoreUtils (transient-capable) pairing
/// variant used by HomePods.
pub const X_APPLE_HKP_VALUE: &str = "4";

/// Driver for one transient pair-setup exchange.
pub struct TransientPairing {
    srp: SrpClient,
}

impl Default for TransientPairing {
    fn default() -> Self {
        Self::new()
    }
}

impl TransientPairing {
    pub fn new() -> Self {
        Self {
            srp: SrpClient::new_homekit_transient(),
        }
    }

    /// M1 request body (TLV8) for the first `POST /pair-setup`.
    pub fn start(&self) -> Vec<u8> {
        TlvBuilder::new()
            .add_u8(TYPE_STATE, 1)
            .add_u8(TYPE_METHOD, METHOD_PAIR_SETUP)
            .add_u8(TYPE_FLAGS, FLAG_TRANSIENT)
            .build()
    }

    /// Consume the M2 response (salt + server public key), run the SRP
    /// computation, and produce the M3 request body (client public key +
    /// proof).
    pub fn handle_m2(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
        let tlv = Tlv::decode(m2).context("decoding pair-setup M2")?;
        check_state(&tlv, 2)?;
        check_error(&tlv)?;

        let salt = tlv
            .get(TYPE_SALT)
            .ok_or_else(|| anyhow!("pair-setup M2 missing Salt"))?;
        let server_pub = tlv
            .get(TYPE_PUBLIC_KEY)
            .ok_or_else(|| anyhow!("pair-setup M2 missing PublicKey"))?;
        if salt.len() != 16 {
            bail!("pair-setup M2 Salt is {} bytes, expected 16", salt.len());
        }

        self.srp
            .process(salt, server_pub)
            .context("SRP processing server M2")?;

        Ok(TlvBuilder::new()
            .add_u8(TYPE_STATE, 3)
            .add(TYPE_PUBLIC_KEY, self.srp.public_key())
            .add(TYPE_PROOF, self.srp.proof())
            .build())
    }

    /// Consume the M4 response (server proof), verify it, and derive the
    /// session keys. Consumes `self` — the exchange is complete.
    pub fn handle_m4(self, m4: &[u8]) -> Result<SessionKeys> {
        let tlv = Tlv::decode(m4).context("decoding pair-setup M4")?;
        check_state(&tlv, 4)?;
        check_error(&tlv)?;

        let server_proof = tlv
            .get(TYPE_PROOF)
            .ok_or_else(|| anyhow!("pair-setup M4 missing Proof"))?;
        if !self.srp.verify_server(server_proof) {
            bail!(
                "pair-setup M4 server proof failed — the receiver rejected transient \
                 pairing (it likely requires a password or on-screen device \
                 verification, which isn't supported yet)"
            );
        }
        Ok(SessionKeys::from_shared(self.srp.session_key()))
    }
}

fn check_state(tlv: &Tlv, expected: u8) -> Result<()> {
    match tlv.state() {
        Some(s) if s == expected => Ok(()),
        Some(s) => bail!("pair-setup: expected State={}, got {}", expected, s),
        None => bail!("pair-setup: response missing State"),
    }
}

fn check_error(tlv: &Tlv) -> Result<()> {
    if let Some(code) = tlv.error() {
        // HAP error codes: 2=Authentication, 3=Backoff, 4=MaxPeers,
        // 5=MaxTries, 6=Unavailable, 7=Busy.
        let meaning = match code {
            2 => "authentication — receiver requires a password / device \
                  verification (transient pairing rejected; not supported yet)",
            3 => "backoff (too many attempts, wait)",
            5 => "max tries (too many wrong attempts — the receiver locked out)",
            6 => "unavailable",
            7 => "busy (another sender is pairing)",
            _ => "unknown",
        };
        bail!("pair-setup: device returned error {} ({})", code, meaning);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::tlv8::{TYPE_ERROR, TYPE_STATE};

    #[test]
    fn m1_has_transient_flag_and_state_1() {
        let p = TransientPairing::new();
        let m1 = p.start();
        let tlv = Tlv::decode(&m1).unwrap();
        assert_eq!(tlv.state(), Some(1));
        assert_eq!(tlv.get_u8(TYPE_METHOD), Some(0));
        assert_eq!(tlv.get_u8(TYPE_FLAGS), Some(0x10));
    }

    #[test]
    fn handle_m2_builds_m3_with_pubkey_and_proof() {
        let mut p = TransientPairing::new();
        let _ = p.start();
        // Synthetic M2: State=2, 16-byte salt, 384-byte "B" (non-zero).
        let m2 = TlvBuilder::new()
            .add_u8(TYPE_STATE, 2)
            .add(TYPE_SALT, &[0x11u8; 16])
            .add(TYPE_PUBLIC_KEY, &[0x42u8; 384])
            .build();
        let m3 = p.handle_m2(&m2).unwrap();
        let tlv = Tlv::decode(&m3).unwrap();
        assert_eq!(tlv.state(), Some(3));
        assert_eq!(tlv.get(TYPE_PUBLIC_KEY).unwrap().len(), 384); // padded A
        assert_eq!(tlv.get(TYPE_PROOF).unwrap().len(), 64); // SHA-512 M1
    }

    #[test]
    fn m2_with_error_is_rejected() {
        let mut p = TransientPairing::new();
        let m2 = TlvBuilder::new()
            .add_u8(TYPE_STATE, 2)
            .add_u8(TYPE_ERROR, 2)
            .build();
        let err = p.handle_m2(&m2).unwrap_err().to_string();
        assert!(err.contains("error 2"), "got: {}", err);
    }

    #[test]
    fn m2_wrong_state_is_rejected() {
        let mut p = TransientPairing::new();
        let m2 = TlvBuilder::new().add_u8(TYPE_STATE, 5).build();
        assert!(p.handle_m2(&m2).is_err());
    }

    #[test]
    fn m4_bad_proof_is_rejected() {
        let mut p = TransientPairing::new();
        let m2 = TlvBuilder::new()
            .add_u8(TYPE_STATE, 2)
            .add(TYPE_SALT, &[0x11u8; 16])
            .add(TYPE_PUBLIC_KEY, &[0x42u8; 384])
            .build();
        let _ = p.handle_m2(&m2).unwrap();
        let m4 = TlvBuilder::new()
            .add_u8(TYPE_STATE, 4)
            .add(TYPE_PROOF, &[0u8; 64])
            .build();
        assert!(p.handle_m4(&m4).is_err());
    }
}
