//! HomeKit **persistent** pairing for Apple TV PIN verification.
//!
//! This is the flow OwnTone drives when an Apple TV (or an AP2 receiver
//! with access control) refuses transient pairing: a full HomeKit
//! `pair-setup` M1–M6 using the on-screen **PIN**, which exchanges
//! long-term Ed25519 keys, followed on every later connection by a
//! `pair-verify` M1–M4 using the stored keys.
//!
//! ```text
//!   pair-setup (once, needs the PIN):
//!     M1 →  State=1, Method=0
//!     M2 ←  State=2, Salt, PublicKey(B)
//!     M3 →  State=3, PublicKey(A), Proof(M1)           [SRP with PIN]
//!     M4 ←  State=4, Proof(M2)
//!     M5 →  State=5, EncryptedData{ Id, LTPK, Sig }    [our long-term key]
//!     M6 ←  State=6, EncryptedData{ Id, LTPK, Sig }    [accessory's key]
//!     ⇒ store PairingCredentials
//!
//!   pair-verify (every connect, from stored creds):
//!     M1 →  State=1, PublicKey(ephemeral X25519)
//!     M2 ←  State=2, PublicKey(acc eph), EncryptedData{ Id, Sig }
//!     M3 →  State=3, EncryptedData{ Id, Sig }
//!     M4 ←  State=4
//!     ⇒ SessionKeys from the X25519 shared secret
//! ```
//!
//! All key-derivation constants are the published HomeKit Accessory
//! Protocol (HAP) values that `pair_ap`/OwnTone use. The whole exchange
//! is exercised end-to-end in the tests against a simulated accessory, so
//! the crypto is self-consistent; the AirPlay-specific transport framing
//! (endpoint paths, the `X-Apple-HKP` header) lives in the caller and is
//! the part that still needs real hardware to confirm.

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha512;

use crate::airplay::srp::SrpClient;
use crate::airplay::tlv8::{
    Tlv, TlvBuilder, METHOD_PAIR_SETUP, TYPE_ENCRYPTED_DATA, TYPE_IDENTIFIER, TYPE_METHOD,
    TYPE_PUBLIC_KEY, TYPE_PROOF, TYPE_SALT, TYPE_SIGNATURE, TYPE_STATE,
};

// HAP HKDF salts/infos (SHA-512). Verbatim from the HAP spec / pair_ap.
const PS_ENCRYPT_SALT: &[u8] = b"Pair-Setup-Encrypt-Salt";
const PS_ENCRYPT_INFO: &[u8] = b"Pair-Setup-Encrypt-Info";
const PS_CONTROLLER_SIGN_SALT: &[u8] = b"Pair-Setup-Controller-Sign-Salt";
const PS_CONTROLLER_SIGN_INFO: &[u8] = b"Pair-Setup-Controller-Sign-Info";
const PS_ACCESSORY_SIGN_SALT: &[u8] = b"Pair-Setup-Accessory-Sign-Salt";
const PS_ACCESSORY_SIGN_INFO: &[u8] = b"Pair-Setup-Accessory-Sign-Info";
const PV_ENCRYPT_SALT: &[u8] = b"Pair-Verify-Encrypt-Salt";
const PV_ENCRYPT_INFO: &[u8] = b"Pair-Verify-Encrypt-Info";
// (pair-verify signs the raw ephemeral‖id‖ephemeral concatenation with the
// long-term key directly — no HKDF-derived sign key, unlike pair-setup.)

/// HTTP `X-Apple-HKP` value for the persistent HomeKit flow (vs `4` for
/// transient). Sent on `/pair-pin-start`, `/pair-setup`, `/pair-verify`.
pub const X_APPLE_HKP_PERSISTENT: &str = "3";

fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm).expect("32 is a valid HKDF-SHA512 length");
    okm
}

/// HAP AEAD nonce: the 8-byte ASCII label right-aligned in 12 bytes.
fn hap_nonce(label: &[u8; 8]) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..12].copy_from_slice(label);
    n
}

fn seal(key: &[u8; 32], label: &[u8; 8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(key.into());
    cipher
        .encrypt((&hap_nonce(label)).into(), Payload { msg: plaintext, aad: &[] })
        .expect("chacha20poly1305 encrypt never fails")
}

fn open(key: &[u8; 32], label: &[u8; 8], ct_and_tag: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    cipher
        .decrypt((&hap_nonce(label)).into(), Payload { msg: ct_and_tag, aad: &[] })
        .map_err(|_| anyhow!("HAP sub-TLV auth failed (wrong PIN / key mismatch)"))
}

/// Long-term pairing credentials, persisted per device so later connects
/// skip pair-setup and go straight to pair-verify.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairingCredentials {
    /// Our controller pairing id (a random UUID string).
    pub controller_id: String,
    /// Our Ed25519 long-term secret **seed** (32 bytes, hex).
    pub controller_seed_hex: String,
    /// The accessory's pairing id, learned in M6.
    pub accessory_id: String,
    /// The accessory's Ed25519 long-term public key (32 bytes, hex).
    pub accessory_ltpk_hex: String,
}

impl PairingCredentials {
    fn signing_key(&self) -> Result<SigningKey> {
        let seed = hex32(&self.controller_seed_hex).context("controller seed")?;
        Ok(SigningKey::from_bytes(&seed))
    }
    fn accessory_ltpk(&self) -> Result<VerifyingKey> {
        let pk = hex32(&self.accessory_ltpk_hex).context("accessory LTPK")?;
        VerifyingKey::from_bytes(&pk).map_err(|e| anyhow!("bad accessory LTPK: {}", e))
    }
}

/// Stored credentials with their keys parsed — what pair-verify works
/// from. They are parsed before anything is sent, so a damaged config
/// entry is reported as damaged instead of looking like a refusal from the
/// receiver.
#[derive(Clone)]
pub struct StoredPairing {
    controller_id: String,
    signing: SigningKey,
    accessory_id: String,
    accessory_ltpk: VerifyingKey,
}

impl StoredPairing {
    pub fn parse(creds: &PairingCredentials) -> Result<Self> {
        Ok(Self {
            controller_id: creds.controller_id.clone(),
            signing: creds.signing_key()?,
            accessory_id: creds.accessory_id.clone(),
            accessory_ltpk: creds.accessory_ltpk()?,
        })
    }
}

/// Parse exactly 32 bytes of hex. Total-function by construction — this
/// is fed from the hand-editable config.json, and the release profile is
/// `panic = "abort"`, so a slicing panic on a truncated/multibyte string
/// here would kill the whole app (and crash-loop it at launch while
/// auto-reconnect keeps retrying the same device).
fn hex32(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        bail!("expected 64 hex chars, got {} bytes", s.len());
    }
    let mut out = [0u8; 32];
    for (i, pair) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16);
        let lo = (pair[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(hi), Some(lo)) => out[i] = ((hi << 4) | lo) as u8,
            _ => bail!("bad hex at offset {}", i * 2),
        }
    }
    Ok(out)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// A random UUID-ish controller identifier (HAP uses a UUID string).
fn random_controller_id() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Mint a fresh persistent controller identity: `(controller_id,
/// ed25519_seed_hex)`. Generated ONCE per install and reused for every
/// pair-setup — HAP accessories key stored pairings on the controller id,
/// so re-pairing with the same identity replaces the record instead of
/// consuming another of the accessory's finite pairing slots (OwnTone
/// likewise passes one fixed device id for its lifetime).
pub fn generate_controller_identity() -> (String, String) {
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    (random_controller_id(), to_hex(&seed))
}

// ---------------------------------------------------------------------------
// pair-setup (persistent, with PIN)
// ---------------------------------------------------------------------------

/// Drives a persistent `pair-setup` with the receiver's on-screen PIN.
pub struct PairSetupPin {
    srp: SrpClient,
    controller_id: String,
    signing: SigningKey,
}

impl PairSetupPin {
    /// Throwaway identity — tests only. Real ceremonies use
    /// [`with_identity`] so the controller id is stable across re-pairs.
    ///
    /// [`with_identity`]: PairSetupPin::with_identity
    pub fn new(pin: &str) -> Self {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        Self {
            srp: SrpClient::new_homekit_with_pin(pin),
            controller_id: random_controller_id(),
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// Pair-setup with the install's persistent controller identity (see
    /// [`generate_controller_identity`]).
    pub fn with_identity(pin: &str, controller_id: String, controller_seed_hex: &str) -> Result<Self> {
        let seed = hex32(controller_seed_hex).context("controller identity seed")?;
        Ok(Self {
            srp: SrpClient::new_homekit_with_pin(pin),
            controller_id,
            signing: SigningKey::from_bytes(&seed),
        })
    }

    /// M1 request body. Persistent pair-setup: `State=1, Method=0` — no
    /// transient flag (that flag is what makes it PIN-less).
    pub fn start(&self) -> Vec<u8> {
        TlvBuilder::new()
            .add_u8(TYPE_STATE, 1)
            .add_u8(TYPE_METHOD, METHOD_PAIR_SETUP)
            .build()
    }

    /// M2 → M3 (SRP: client public key + proof). Same maths as transient.
    pub fn handle_m2(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
        let tlv = Tlv::decode(m2).context("decoding pair-setup M2")?;
        check_state(&tlv, 2)?;
        check_error(&tlv, Step::SetupM2)?;
        let salt = tlv.get(TYPE_SALT).ok_or_else(|| anyhow!("M2 missing Salt"))?;
        let server_pub = tlv
            .get(TYPE_PUBLIC_KEY)
            .ok_or_else(|| anyhow!("M2 missing PublicKey"))?;
        self.srp.process(salt, server_pub).context("SRP M2")?;
        Ok(TlvBuilder::new()
            .add_u8(TYPE_STATE, 3)
            .add(TYPE_PUBLIC_KEY, self.srp.public_key())
            .add(TYPE_PROOF, self.srp.proof())
            .build())
    }

    /// M4 (verify server SRP proof) → M5 (our encrypted Id + LTPK + Sig).
    pub fn handle_m4(&mut self, m4: &[u8]) -> Result<Vec<u8>> {
        let tlv = Tlv::decode(m4).context("decoding pair-setup M4")?;
        check_state(&tlv, 4)?;
        check_error(&tlv, Step::SetupM4)?;
        let server_proof = tlv.get(TYPE_PROOF).ok_or_else(|| anyhow!("M4 missing Proof"))?;
        if !self.srp.verify_server(server_proof) {
            bail!("pair-setup M4 proof failed — wrong PIN");
        }
        Ok(build_setup_m5(
            self.srp.session_key(),
            &self.controller_id,
            &self.signing,
        ))
    }

    /// M6 → the long-term [`PairingCredentials`] to persist.
    pub fn handle_m6(self, m6: &[u8]) -> Result<PairingCredentials> {
        let (acc_id, acc_ltpk) = parse_setup_m6(self.srp.session_key(), m6)?;
        Ok(PairingCredentials {
            controller_id: self.controller_id,
            controller_seed_hex: to_hex(&self.signing.to_bytes()),
            accessory_id: acc_id,
            accessory_ltpk_hex: to_hex(&acc_ltpk),
        })
    }
}

/// Build the M5 body: encrypt {controllerId, our LTPK, our signature} with
/// the pair-setup encrypt key. Split out (taking the SRP key `k`
/// explicitly) so the crypto is unit-testable without a live SRP run.
fn build_setup_m5(k: &[u8], controller_id: &str, signing: &SigningKey) -> Vec<u8> {
    let encrypt_key = hkdf32(PS_ENCRYPT_SALT, k, PS_ENCRYPT_INFO);
    let device_x = hkdf32(PS_CONTROLLER_SIGN_SALT, k, PS_CONTROLLER_SIGN_INFO);
    let ltpk = signing.verifying_key().to_bytes();

    // Signature over deviceX ‖ controllerId ‖ LTPK.
    let mut sig_material = Vec::new();
    sig_material.extend_from_slice(&device_x);
    sig_material.extend_from_slice(controller_id.as_bytes());
    sig_material.extend_from_slice(&ltpk);
    let signature = signing.sign(&sig_material).to_bytes();

    let sub = TlvBuilder::new()
        .add(TYPE_IDENTIFIER, controller_id.as_bytes())
        .add(TYPE_PUBLIC_KEY, &ltpk)
        .add(TYPE_SIGNATURE, &signature)
        .build();
    TlvBuilder::new()
        .add_u8(TYPE_STATE, 5)
        .add(TYPE_ENCRYPTED_DATA, &seal(&encrypt_key, b"PS-Msg05", &sub))
        .build()
}

/// Decrypt + verify M6, returning `(accessory_id, accessory_ltpk)`.
fn parse_setup_m6(k: &[u8], m6: &[u8]) -> Result<(String, [u8; 32])> {
    let tlv = Tlv::decode(m6).context("decoding pair-setup M6")?;
    check_state(&tlv, 6)?;
    check_error(&tlv, Step::SetupM6)?;
    let enc = tlv
        .get(TYPE_ENCRYPTED_DATA)
        .ok_or_else(|| anyhow!("M6 missing EncryptedData"))?;

    let encrypt_key = hkdf32(PS_ENCRYPT_SALT, k, PS_ENCRYPT_INFO);
    let sub_bytes = open(&encrypt_key, b"PS-Msg06", enc).context("decrypting M6")?;
    let sub = Tlv::decode(&sub_bytes).context("decoding M6 sub-TLV")?;

    let acc_id = sub
        .get(TYPE_IDENTIFIER)
        .ok_or_else(|| anyhow!("M6 missing accessory Identifier"))?;
    let acc_ltpk = sub
        .get(TYPE_PUBLIC_KEY)
        .ok_or_else(|| anyhow!("M6 missing accessory LTPK"))?;
    let acc_sig = sub
        .get(TYPE_SIGNATURE)
        .ok_or_else(|| anyhow!("M6 missing accessory Signature"))?;

    let acc_x = hkdf32(PS_ACCESSORY_SIGN_SALT, k, PS_ACCESSORY_SIGN_INFO);
    let mut material = Vec::new();
    material.extend_from_slice(&acc_x);
    material.extend_from_slice(acc_id);
    material.extend_from_slice(acc_ltpk);
    verify_sig(acc_ltpk, &material, acc_sig).context("verifying accessory M6 signature")?;

    let mut ltpk = [0u8; 32];
    if acc_ltpk.len() != 32 {
        bail!("accessory LTPK is {} bytes, want 32", acc_ltpk.len());
    }
    ltpk.copy_from_slice(acc_ltpk);
    Ok((String::from_utf8_lossy(acc_id).to_string(), ltpk))
}

// ---------------------------------------------------------------------------
// pair-verify (from stored credentials)
// ---------------------------------------------------------------------------

/// Why pair-verify's M4 didn't complete the exchange.
#[derive(Debug)]
pub enum M4Failure {
    /// M4 carries an error: the accessory's refusal of this controller
    /// (HAP's answer for one it no longer knows). M4 carries no signature,
    /// so a relay in the middle can send this as well as the accessory.
    Refused(anyhow::Error),
    /// M4 is no answer the exchange can end on: no TLV, or neither State 4
    /// nor an error. It says nothing about the pairing.
    Unreadable(anyhow::Error),
}

/// Drives a `pair-verify` from a [`StoredPairing`].
pub struct PairVerify {
    pairing: StoredPairing,
    eph_secret: Option<x25519_dalek::EphemeralSecret>,
    eph_public: [u8; 32],
    shared: Option<[u8; 32]>,
}

impl PairVerify {
    pub fn new(pairing: StoredPairing) -> Self {
        let secret = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
        let public = x25519_dalek::PublicKey::from(&secret);
        Self {
            pairing,
            eph_secret: Some(secret),
            eph_public: public.to_bytes(),
            shared: None,
        }
    }

    /// M1: our ephemeral X25519 public key.
    pub fn start(&self) -> Vec<u8> {
        TlvBuilder::new()
            .add_u8(TYPE_STATE, 1)
            .add(TYPE_PUBLIC_KEY, &self.eph_public)
            .build()
    }

    /// M2 → M3. Derives the shared secret, verifies the accessory's
    /// signature against the stored LTPK, and returns the encrypted M3.
    /// Every error here means the peer didn't prove it holds the paired
    /// accessory's key: until the signature check passes, anyone could
    /// have produced this M2 (decrypting it proves nothing — the key comes
    /// from the peer's own ephemeral key).
    pub fn handle_m2(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
        let tlv = Tlv::decode(m2).context("decoding pair-verify M2")?;
        check_state(&tlv, 2)?;
        check_error(&tlv, Step::VerifyM2)?;
        let acc_eph = tlv
            .get(TYPE_PUBLIC_KEY)
            .ok_or_else(|| anyhow!("M2 missing accessory PublicKey"))?;
        let enc = tlv
            .get(TYPE_ENCRYPTED_DATA)
            .ok_or_else(|| anyhow!("M2 missing EncryptedData"))?;
        if acc_eph.len() != 32 {
            bail!("M2 accessory PublicKey is {} bytes, want 32", acc_eph.len());
        }
        let mut acc_pub = [0u8; 32];
        acc_pub.copy_from_slice(acc_eph);

        let secret = self
            .eph_secret
            .take()
            .ok_or_else(|| anyhow!("pair-verify M2 called twice"))?;
        let shared = secret
            .diffie_hellman(&x25519_dalek::PublicKey::from(acc_pub))
            .to_bytes();
        let session_key = hkdf32(PV_ENCRYPT_SALT, &shared, PV_ENCRYPT_INFO);

        // Decrypt M2 sub-TLV and verify the accessory's signature over
        // accEph ‖ accId ‖ ourEph with the *stored* accessory LTPK.
        let sub_bytes = open(&session_key, b"PV-Msg02", enc).context("decrypting M2")?;
        let sub = Tlv::decode(&sub_bytes).context("decoding M2 sub-TLV")?;
        let acc_id = sub
            .get(TYPE_IDENTIFIER)
            .ok_or_else(|| anyhow!("M2 missing accessory Identifier"))?;
        let acc_sig = sub
            .get(TYPE_SIGNATURE)
            .ok_or_else(|| anyhow!("M2 missing accessory Signature"))?;
        if String::from_utf8_lossy(acc_id) != self.pairing.accessory_id {
            bail!(
                "it identifies as {:?}, not as the paired accessory",
                String::from_utf8_lossy(acc_id)
            );
        }
        let mut material = Vec::new();
        material.extend_from_slice(acc_eph);
        material.extend_from_slice(acc_id);
        material.extend_from_slice(&self.eph_public);
        self.pairing
            .accessory_ltpk
            .verify(&material, &ed25519_sig(acc_sig)?)
            .map_err(|_| anyhow!("its signature doesn't match the paired accessory's key"))?;

        // Build M3: our signature over ourEph ‖ ourId ‖ accEph.
        let mut our_material = Vec::new();
        our_material.extend_from_slice(&self.eph_public);
        our_material.extend_from_slice(self.pairing.controller_id.as_bytes());
        our_material.extend_from_slice(acc_eph);
        let our_sig = self.pairing.signing.sign(&our_material).to_bytes();
        let sub = TlvBuilder::new()
            .add(TYPE_IDENTIFIER, self.pairing.controller_id.as_bytes())
            .add(TYPE_SIGNATURE, &our_sig)
            .build();
        let encrypted = seal(&session_key, b"PV-Msg03", &sub);

        self.shared = Some(shared);
        Ok(TlvBuilder::new()
            .add_u8(TYPE_STATE, 3)
            .add(TYPE_ENCRYPTED_DATA, &encrypted)
            .build())
    }

    /// M4 (success) → the X25519 shared secret to build the session keys.
    pub fn finish(self, m4: &[u8]) -> std::result::Result<[u8; 32], M4Failure> {
        let tlv = Tlv::decode(m4)
            .context("decoding pair-verify M4")
            .map_err(M4Failure::Unreadable)?;
        // An error is the accessory's answer whatever State it came with
        // (HAP sends it with State 4).
        check_error(&tlv, Step::VerifyM4).map_err(M4Failure::Refused)?;
        check_state(&tlv, 4).map_err(M4Failure::Unreadable)?;
        self.shared
            .ok_or_else(|| M4Failure::Unreadable(anyhow!("pair-verify finished before M2")))
    }
}

impl Drop for PairVerify {
    /// The X25519 shared secret is the input keying material for every
    /// session key — wipe our copy on drop, as SessionKeys wipes its keys
    /// and srp.rs its session key K. `finish` copies the array out; this
    /// clears the copy that stays behind in the struct.
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(s) = self.shared.as_mut() {
            s.zeroize();
        }
    }
}

fn ed25519_sig(bytes: &[u8]) -> Result<ed25519_dalek::Signature> {
    let arr: [u8; 64] = bytes
        .try_into()
        .map_err(|_| anyhow!("signature is {} bytes, want 64", bytes.len()))?;
    Ok(ed25519_dalek::Signature::from_bytes(&arr))
}

fn verify_sig(ltpk: &[u8], message: &[u8], sig: &[u8]) -> Result<()> {
    let pk: [u8; 32] = ltpk
        .try_into()
        .map_err(|_| anyhow!("LTPK is {} bytes, want 32", ltpk.len()))?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|e| anyhow!("bad LTPK: {}", e))?;
    vk.verify(message, &ed25519_sig(sig)?)
        .map_err(|_| anyhow!("signature verification failed"))
}

fn check_state(tlv: &Tlv, expected: u8) -> Result<()> {
    match tlv.state() {
        Some(s) if s == expected => Ok(()),
        Some(s) => bail!("pairing: expected State={}, got {}", expected, s),
        None => bail!("pairing: response missing State"),
    }
}

/// The message a TLV error came in. The same HAP code means different
/// things at different steps: Authentication is a wrong PIN in pair-setup
/// M4, but a controller the accessory no longer knows in pair-verify M4.
#[derive(Clone, Copy)]
enum Step {
    SetupM2,
    SetupM4,
    SetupM6,
    VerifyM2,
    VerifyM4,
}

fn check_error(tlv: &Tlv, step: Step) -> Result<()> {
    if let Some(code) = tlv.error() {
        let meaning = match (code, step) {
            (2, Step::SetupM4) => "authentication: wrong PIN",
            (2, Step::SetupM6) => "authentication: the device didn't accept this PC's pairing key",
            (2, Step::VerifyM4) => "authentication: the device no longer accepts this PC's pairing",
            (2, Step::SetupM2 | Step::VerifyM2) => "authentication",
            (3, _) => "backoff (too many attempts — wait)",
            (4, _) => "max peers — the device's pairing list is full; remove old \
                       pairings on the device and try again",
            (5, _) => "max tries (receiver locked out)",
            (6, _) => "unavailable",
            (7, _) => "busy (another sender is pairing)",
            _ => "unknown",
        };
        bail!("device returned error {} ({})", code, meaning);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal in-test "accessory" that speaks the other side of the HAP
    // protocol, so we can run a full pair-setup + pair-verify handshake and
    // prove the client's message construction + crypto are self-consistent.
    // It reuses the client SRP as the server is symmetric enough for the K
    // agreement check via a shared PIN + our own SRP server stand-in is out
    // of scope; instead we validate the *sign/encrypt* halves that are the
    // error-prone part, by having the accessory reuse the derived K.

    fn accessory_keypair() -> (SigningKey, VerifyingKey) {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let sk = SigningKey::from_bytes(&seed);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    #[test]
    fn pair_setup_m1_is_state1_method0_no_flags() {
        let ps = PairSetupPin::new("123-45-678");
        let tlv = Tlv::decode(&ps.start()).unwrap();
        assert_eq!(tlv.state(), Some(1));
        assert_eq!(tlv.get_u8(TYPE_METHOD), Some(0));
        assert!(tlv.get(TYPE_FLAGS_LOCAL).is_none(), "persistent must omit the transient flag");
    }

    // TYPE_FLAGS is 0x13; re-declare locally to avoid widening the tlv8 API.
    const TYPE_FLAGS_LOCAL: u8 = 0x13;

    // Build an accessory-side M6 for a given SRP key K and accessory keypair.
    fn accessory_m6(k: &[u8], acc_sk: &SigningKey, acc_id: &str) -> Vec<u8> {
        let encrypt_key = hkdf32(PS_ENCRYPT_SALT, k, PS_ENCRYPT_INFO);
        let acc_x = hkdf32(PS_ACCESSORY_SIGN_SALT, k, PS_ACCESSORY_SIGN_INFO);
        let acc_ltpk = acc_sk.verifying_key().to_bytes();
        let mut material = Vec::new();
        material.extend_from_slice(&acc_x);
        material.extend_from_slice(acc_id.as_bytes());
        material.extend_from_slice(&acc_ltpk);
        let acc_sig = acc_sk.sign(&material).to_bytes();
        let sub = TlvBuilder::new()
            .add(TYPE_IDENTIFIER, acc_id.as_bytes())
            .add(TYPE_PUBLIC_KEY, &acc_ltpk)
            .add(TYPE_SIGNATURE, &acc_sig)
            .build();
        TlvBuilder::new()
            .add_u8(TYPE_STATE, 6)
            .add(TYPE_ENCRYPTED_DATA, &seal(&encrypt_key, b"PS-Msg06", &sub))
            .build()
    }

    #[test]
    fn setup_m5_m6_roundtrip_with_shared_key() {
        // Fixed SRP session key both sides share (the SRP K-agreement itself
        // is validated in srp.rs). Verify the client's M5 is well-formed and
        // its M6 parse extracts + verifies the accessory credentials.
        let k = [0x5au8; 64];
        let (client_sk, client_vk) = accessory_keypair(); // reuse the keygen
        let controller_id = "client-uuid";

        // M5: decrypt on the "accessory" side and check the client signature.
        let m5 = Tlv::decode(&build_setup_m5(&k, controller_id, &client_sk)).unwrap();
        assert_eq!(m5.state(), Some(5));
        let m5_sub = Tlv::decode(
            &open(&hkdf32(PS_ENCRYPT_SALT, &k, PS_ENCRYPT_INFO), b"PS-Msg05", m5.get(TYPE_ENCRYPTED_DATA).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(m5_sub.get(TYPE_IDENTIFIER).unwrap(), controller_id.as_bytes());
        assert_eq!(m5_sub.get(TYPE_PUBLIC_KEY).unwrap(), &client_vk.to_bytes());
        let device_x = hkdf32(PS_CONTROLLER_SIGN_SALT, &k, PS_CONTROLLER_SIGN_INFO);
        let mut expect = Vec::new();
        expect.extend_from_slice(&device_x);
        expect.extend_from_slice(controller_id.as_bytes());
        expect.extend_from_slice(&client_vk.to_bytes());
        client_vk
            .verify(&expect, &ed25519_sig(m5_sub.get(TYPE_SIGNATURE).unwrap()).unwrap())
            .expect("accessory verifies client M5 signature");

        // M6: build one on the accessory side and parse it on the client side.
        let (acc_sk, acc_vk) = accessory_keypair();
        let m6 = accessory_m6(&k, &acc_sk, "AA:BB:CC:DD:EE:FF");
        let (id, ltpk) = parse_setup_m6(&k, &m6).expect("M6 accepted");
        assert_eq!(id, "AA:BB:CC:DD:EE:FF");
        assert_eq!(ltpk, acc_vk.to_bytes());
    }

    /// PIN pair-setup M2 to M6 against receivers of both SRP byte
    /// conventions, one with a B that starts with a zero byte and one with
    /// pair_ap's 15-byte salt; ends with stored credentials naming the
    /// accessory. (The vectors are for the PIN `3939`.)
    #[test]
    fn pin_pair_setup_completes_against_receiver_models() {
        use crate::airplay::srp_test_receiver::{vectors as v, vectors::bytes, Convention, M3Reply, Receiver};
        let pin = "3939";
        for (conv, salt, b) in [
            (Convention::Adk, v::SALT, v::B_SHORT),
            (Convention::PairAp, v::SALT_SHORT, v::B_FULL),
        ] {
            let rx = Receiver::new(conv, pin, bytes(salt), bytes(b));
            assert!(rx.b_top_byte_zero() || rx.salt().len() == 15, "vector: {conv:?} has its corner");
            let mut ps = PairSetupPin::new(pin);
            let m2 = TlvBuilder::new()
                .add_u8(TYPE_STATE, 2)
                .add(TYPE_SALT, rx.salt())
                .add(TYPE_PUBLIC_KEY, &rx.b_wire())
                .build();
            let m3 = Tlv::decode(&ps.handle_m2(&m2).unwrap()).unwrap();
            let M3Reply::Proof { m2: proof, key } =
                rx.m3(m3.get(TYPE_PUBLIC_KEY).unwrap(), m3.get(TYPE_PROOF).unwrap())
            else {
                panic!("{conv:?}: the receiver refused M3 for the right PIN");
            };
            let m4 = TlvBuilder::new().add_u8(TYPE_STATE, 4).add(TYPE_PROOF, &proof).build();
            ps.handle_m4(&m4).expect("client accepts the receiver's proof");
            let (acc_sk, acc_vk) = accessory_keypair();
            let creds = ps.handle_m6(&accessory_m6(&key, &acc_sk, "acc-id")).expect("M6 accepted");
            assert_eq!(creds.accessory_id, "acc-id");
            assert_eq!(creds.accessory_ltpk_hex, to_hex(&acc_vk.to_bytes()));
        }
    }

    #[test]
    fn hex32_rejects_malformed_input_without_panicking() {
        // Fed from the hand-editable config.json — every malformed shape
        // must be an Err, never a panic (release builds abort on panic).
        assert!(hex32("").is_err());
        assert!(hex32("ab").is_err()); // too short
        assert!(hex32(&"a".repeat(63)).is_err()); // odd length
        assert!(hex32(&"a".repeat(65)).is_err()); // too long
        assert!(hex32(&"g".repeat(64)).is_err()); // not hex
        assert!(hex32(&"é".repeat(32)).is_err()); // multibyte, 64 bytes
        let good = "a0".repeat(32);
        assert_eq!(hex32(&good).unwrap()[0], 0xa0);
    }

    #[test]
    fn identity_roundtrips_through_hex() {
        let (id, seed_hex) = generate_controller_identity();
        assert_eq!(seed_hex.len(), 64);
        let ps = PairSetupPin::with_identity("123-45-678", id.clone(), &seed_hex).unwrap();
        assert_eq!(ps.controller_id, id);
        // Same seed → same signing key → deterministic public key.
        let ps2 = PairSetupPin::with_identity("123-45-678", id, &seed_hex).unwrap();
        assert_eq!(
            ps.signing.verifying_key().to_bytes(),
            ps2.signing.verifying_key().to_bytes()
        );
    }

    /// A damaged stored entry is caught when it is parsed, naming the
    /// field — before anything is sent to the receiver.
    #[test]
    fn stored_pairing_parse_names_the_damaged_field() {
        let (_, acc_vk) = accessory_keypair();
        let good = PairingCredentials {
            controller_id: "client-uuid".into(),
            controller_seed_hex: "a0".repeat(32),
            accessory_id: "acc-uuid".into(),
            accessory_ltpk_hex: to_hex(&acc_vk.to_bytes()),
        };
        assert!(StoredPairing::parse(&good).is_ok());
        let seed = PairingCredentials { controller_seed_hex: "a0".repeat(31), ..good.clone() };
        let e = format!("{:#}", StoredPairing::parse(&seed).err().unwrap());
        assert!(e.contains("controller seed"), "{e}");
        let ltpk = PairingCredentials { accessory_ltpk_hex: "zz".repeat(32), ..good.clone() };
        let e = format!("{:#}", StoredPairing::parse(&ltpk).err().unwrap());
        assert!(e.contains("accessory LTPK"), "{e}");
        // 32 bytes of hex that are no Ed25519 public key.
        let not_a_point = (2u8..=255)
            .map(|k| {
                let mut b = [0u8; 32];
                b[0] = k;
                b
            })
            .find(|b| VerifyingKey::from_bytes(b).is_err())
            .unwrap();
        let point = PairingCredentials { accessory_ltpk_hex: to_hex(&not_a_point), ..good };
        let e = format!("{:#}", StoredPairing::parse(&point).err().unwrap());
        assert!(e.contains("accessory LTPK"), "{e}");
    }

    /// Error 2 reads as a wrong PIN only where it is one.
    #[test]
    fn authentication_error_text_depends_on_the_step() {
        let m = |state: u8| TlvBuilder::new().add_u8(TYPE_STATE, state).add_u8(0x07, 2).build();
        let text = |state: u8, step: Step| check_error(&Tlv::decode(&m(state)).unwrap(), step).unwrap_err().to_string();
        assert!(text(4, Step::SetupM4).contains("wrong PIN"));
        let verify = text(4, Step::VerifyM4);
        assert!(verify.contains("no longer accepts this PC's pairing") && !verify.contains("PIN"), "{verify}");
        assert!(!text(2, Step::VerifyM2).contains("PIN"));
        assert!(!text(6, Step::SetupM6).contains("PIN"));
    }

    /// An M4 with an error is the accessory's refusal, whatever State it
    /// carries; an M4 that can't be read is not one.
    #[test]
    fn an_unreadable_m4_is_not_a_refusal() {
        use crate::airplay::tlv8::TYPE_ERROR;
        let (_, acc_vk) = accessory_keypair();
        let creds = PairingCredentials {
            controller_id: "client-uuid".into(),
            controller_seed_hex: "a0".repeat(32),
            accessory_id: "acc-uuid".into(),
            accessory_ltpk_hex: to_hex(&acc_vk.to_bytes()),
        };
        let finish = |m4: &[u8]| PairVerify::new(StoredPairing::parse(&creds).unwrap()).finish(m4);
        let error = |state: u8| TlvBuilder::new().add_u8(TYPE_STATE, state).add_u8(TYPE_ERROR, 2).build();
        for state in [4, 2] {
            match finish(&error(state)) {
                Err(M4Failure::Refused(e)) => {
                    assert!(format!("{e:#}").contains("no longer accepts this PC's pairing"), "{e:#}")
                }
                other => panic!("State {state} with an error: {other:?}"),
            }
        }
        let no_state = TlvBuilder::new().add_u8(TYPE_ERROR, 2).build();
        assert!(matches!(finish(&no_state), Err(M4Failure::Refused(_))));
        for (what, m4) in [
            ("not a TLV", b"\x99\x99".to_vec()),
            ("empty", Vec::new()),
            ("State 2, no error", TlvBuilder::new().add_u8(TYPE_STATE, 2).build()),
        ] {
            assert!(matches!(finish(&m4), Err(M4Failure::Unreadable(_))), "{what}");
        }
    }

    #[test]
    fn max_peers_error_is_actionable() {
        let m2 = TlvBuilder::new()
            .add_u8(TYPE_STATE, 2)
            .add_u8(0x07, 4) // TYPE_ERROR = 0x07, kTLVError_MaxPeers
            .build();
        let mut ps = PairSetupPin::new("123-45-678");
        let err = ps.handle_m2(&m2).unwrap_err().to_string();
        assert!(err.contains("pairing list is full"), "got: {}", err);
    }

    #[test]
    fn m6_tampered_signature_is_rejected() {
        let k = [0x11u8; 64];
        let (acc_sk, _) = accessory_keypair();
        let mut m6 = accessory_m6(&k, &acc_sk, "id");
        // Flip a bit inside the encrypted blob → auth fails.
        let n = m6.len();
        m6[n - 5] ^= 0x01;
        assert!(parse_setup_m6(&k, &m6).is_err());
    }

    #[test]
    fn pair_verify_full_handshake_agrees_on_shared_secret() {
        // Given stored creds (client seed + accessory LTPK), run the whole
        // pair-verify against a simulated accessory and check both sides
        // derive the same X25519 shared secret.
        let (acc_sk, acc_vk) = accessory_keypair();
        let client_sk = {
            let mut s = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut s);
            SigningKey::from_bytes(&s)
        };
        let creds = PairingCredentials {
            controller_id: "client-uuid".into(),
            controller_seed_hex: to_hex(&client_sk.to_bytes()),
            accessory_id: "acc-uuid".into(),
            accessory_ltpk_hex: to_hex(&acc_vk.to_bytes()),
        };

        let mut pv = PairVerify::new(StoredPairing::parse(&creds).unwrap());
        let m1 = Tlv::decode(&pv.start()).unwrap();
        let client_eph = m1.get(TYPE_PUBLIC_KEY).unwrap().to_vec();

        // Accessory M2.
        let acc_secret = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
        let acc_eph = x25519_dalek::PublicKey::from(&acc_secret).to_bytes();
        let mut client_eph_arr = [0u8; 32];
        client_eph_arr.copy_from_slice(&client_eph);
        let acc_shared = acc_secret
            .diffie_hellman(&x25519_dalek::PublicKey::from(client_eph_arr))
            .to_bytes();
        let acc_session = hkdf32(PV_ENCRYPT_SALT, &acc_shared, PV_ENCRYPT_INFO);
        let mut acc_material = Vec::new();
        acc_material.extend_from_slice(&acc_eph);
        acc_material.extend_from_slice(creds.accessory_id.as_bytes());
        acc_material.extend_from_slice(&client_eph);
        let acc_sig = acc_sk.sign(&acc_material).to_bytes();
        let acc_sub = TlvBuilder::new()
            .add(TYPE_IDENTIFIER, creds.accessory_id.as_bytes())
            .add(TYPE_SIGNATURE, &acc_sig)
            .build();
        let m2 = TlvBuilder::new()
            .add_u8(TYPE_STATE, 2)
            .add(TYPE_PUBLIC_KEY, &acc_eph)
            .add(TYPE_ENCRYPTED_DATA, &seal(&acc_session, b"PV-Msg02", &acc_sub))
            .build();

        // Client M2 → M3.
        let m3_bytes = pv.handle_m2(&m2).expect("client accepts accessory M2");
        let m3 = Tlv::decode(&m3_bytes).unwrap();
        assert_eq!(m3.state(), Some(3));
        // Accessory verifies M3's signature (client id + sig over the infos).
        let m3_enc = m3.get(TYPE_ENCRYPTED_DATA).unwrap();
        let m3_sub = Tlv::decode(&open(&acc_session, b"PV-Msg03", m3_enc).unwrap()).unwrap();
        let client_id = m3_sub.get(TYPE_IDENTIFIER).unwrap();
        let client_sig = m3_sub.get(TYPE_SIGNATURE).unwrap();
        assert_eq!(client_id, creds.controller_id.as_bytes());
        let mut expect = Vec::new();
        expect.extend_from_slice(&client_eph);
        expect.extend_from_slice(creds.controller_id.as_bytes());
        expect.extend_from_slice(&acc_eph);
        client_sk
            .verifying_key()
            .verify(&expect, &ed25519_sig(client_sig).unwrap())
            .expect("accessory verifies client M3 signature");

        // M4 → client returns the shared secret; it must match the accessory's.
        let m4 = TlvBuilder::new().add_u8(TYPE_STATE, 4).build();
        let client_shared = pv.finish(&m4).unwrap();
        assert_eq!(client_shared, acc_shared, "both sides agree on the pair-verify secret");
    }
}
