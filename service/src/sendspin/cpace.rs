//! CPace balanced PAKE, cipher suite CPACE-X25519-SHA512
//! (draft-irtf-cfrg-cpace), initiator–responder mode with explicit mutual
//! confirmation (MCF) — the PAKE behind Sendspin's PIN pairing.
//!
//! Mirrors the Python `cpace` package that aiosendspin uses: generator via
//! Elligator2 on Curve25519 from `SHA-512(lv_cat(DSI, PRS, zpad, CI, sid))`,
//! `ISK = SHA-512(lv_cat("CPace255_ISK", sid, K) || lv(Ya, ADa) || lv(Yb, ADb))`,
//! MCF tags `HMAC-SHA-512(SHA-512("CPaceMac" || sid || ISK), lv(Y, AD))`.
//! The server is role A (initiator), the client role B (responder).

use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use num_traits::{One, Zero};
use rand::RngCore;
use sha2::{Digest, Sha512};

type HmacSha512 = Hmac<Sha512>;

const DSI: &[u8] = b"CPace255";
const DSI_ISK: &[u8] = b"CPace255_ISK";
const MAC_LABEL: &[u8] = b"CPaceMac";
const SHA512_BLOCK_BYTES: usize = 128;
const CURVE_A: u32 = 486_662;
/// Elligator2 non-square for Curve25519.
const Z: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Server side (`A`).
    Initiator,
    /// Client side (`B`).
    Responder,
}

fn prepend_len(data: &[u8]) -> Vec<u8> {
    let mut len = data.len();
    let mut out = Vec::with_capacity(data.len() + 2);
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(data);
    out
}

fn lv_cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.iter().flat_map(|p| prepend_len(p)).collect()
}

fn generator_string(prs: &[u8], ci: &[u8], sid: &[u8]) -> Vec<u8> {
    let len_zpad = SHA512_BLOCK_BYTES
        .saturating_sub(1)
        .saturating_sub(prepend_len(prs).len())
        .saturating_sub(prepend_len(DSI).len());
    let zpad = vec![0u8; len_zpad];
    lv_cat(&[DSI, prs, &zpad, ci, sid])
}

fn field_prime() -> BigUint {
    (BigUint::one() << 255u32) - BigUint::from(19u32)
}

fn le_to_int(b: &[u8]) -> BigUint {
    BigUint::from_bytes_le(b)
}

fn int_to_le32(x: &BigUint) -> [u8; 32] {
    let mut out = [0u8; 32];
    let b = x.to_bytes_le();
    out[..b.len()].copy_from_slice(&b);
    out
}

/// Elligator2 map for Curve25519 (RFC 9380 straight-line form as in the
/// draft's reference): returns the u-coordinate, little-endian.
fn elligator2(r: &BigUint) -> [u8; 32] {
    let q = field_prime();
    let r = r % &q;
    let a = BigUint::from(CURVE_A);
    let z = BigUint::from(Z);
    let inv0 = |x: &BigUint| x.modpow(&(&q - BigUint::from(2u32)), &q);
    let denom = (BigUint::one() + &z * &r * &r) % &q;
    // v = -A / (1 + Z r^2)
    let v = (&q - (&a * inv0(&denom)) % &q) % &q;
    // eps = legendre(v^3 + A v^2 + v) ∈ {0, 1, q-1}
    let rhs = (&v * &v * &v + &a * &v * &v + &v) % &q;
    let eps = rhs.modpow(&((&q - BigUint::one()) >> 1u32), &q);
    let inv2 = inv0(&BigUint::from(2u32));
    // x = eps*v - (1 - eps) * A / 2   (mod q)
    let one_minus_eps = (BigUint::one() + &q - &eps) % &q;
    let term = (one_minus_eps * &a % &q) * inv2 % &q;
    let x = ((&eps * &v) % &q + &q - term) % &q;
    int_to_le32(&x)
}

fn calculate_generator(prs: &[u8], ci: &[u8], sid: &[u8]) -> [u8; 32] {
    let digest = Sha512::digest(generator_string(prs, ci, sid));
    let mut u = [0u8; 32];
    u.copy_from_slice(&digest[..32]);
    u[31] &= 0x7F; // 255-bit field: ignore the unused top bit
    elligator2(&le_to_int(&u))
}

fn scalar_mult_vfy(scalar: &[u8; 32], point: &[u8; 32]) -> Option<[u8; 32]> {
    let out = x25519_dalek::x25519(*scalar, *point);
    if out == [0u8; 32] {
        None
    } else {
        Some(out)
    }
}

/// One side of a CPace run.
pub struct CPace {
    role: Role,
    sid: Vec<u8>,
    ad: Vec<u8>,
    scalar: Option<[u8; 32]>,
    pub public_share: [u8; 32],
    derived: Option<Derived>,
}

struct Derived {
    isk: [u8; 64],
    mac_key: [u8; 64],
    /// ((Ya, ADa), (Yb, ADb)) — initiator first.
    sides: [(Vec<u8>, Vec<u8>); 2],
}

impl CPace {
    pub fn start(role: Role, prs: &[u8], sid: &[u8], ci: &[u8], ad: &[u8]) -> Result<Self> {
        let mut scalar = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut scalar);
        Self::start_with_scalar(role, prs, sid, ci, ad, scalar)
    }

    fn start_with_scalar(role: Role, prs: &[u8], sid: &[u8], ci: &[u8], ad: &[u8], scalar: [u8; 32]) -> Result<Self> {
        let g = calculate_generator(prs, ci, sid);
        let Some(share) = scalar_mult_vfy(&scalar, &g) else {
            bail!("CPace generator encodes a low-order point");
        };
        Ok(Self {
            role,
            sid: sid.to_vec(),
            ad: ad.to_vec(),
            scalar: Some(scalar),
            public_share: share,
            derived: None,
        })
    }

    /// Ingest the peer's share; derives the ISK and confirmation key.
    pub fn derive(&mut self, peer_share: &[u8], peer_ad: &[u8]) -> Result<()> {
        let Some(scalar) = self.scalar.take() else {
            bail!("CPace derive() may only be called once");
        };
        if peer_share.len() != 32 {
            bail!("CPace peer share must be 32 bytes, got {}", peer_share.len());
        }
        let mut peer = [0u8; 32];
        peer.copy_from_slice(peer_share);
        let Some(k) = scalar_mult_vfy(&scalar, &peer) else {
            bail!("CPace peer share encodes a low-order point");
        };
        let own = (self.public_share.to_vec(), self.ad.clone());
        let theirs = (peer.to_vec(), peer_ad.to_vec());
        let sides = match self.role {
            Role::Initiator => [own, theirs],
            Role::Responder => [theirs, own],
        };
        let mut transcript = lv_cat(&[&sides[0].0, &sides[0].1]);
        transcript.extend(lv_cat(&[&sides[1].0, &sides[1].1]));
        let mut h = Sha512::new();
        h.update(lv_cat(&[DSI_ISK, &self.sid, &k]));
        h.update(&transcript);
        let isk: [u8; 64] = h.finalize().into();
        let mut m = Sha512::new();
        m.update(MAC_LABEL);
        m.update(&self.sid);
        m.update(isk);
        let mac_key: [u8; 64] = m.finalize().into();
        self.derived = Some(Derived { isk, mac_key, sides });
        Ok(())
    }

    pub fn isk(&self) -> Result<[u8; 64]> {
        match &self.derived {
            Some(d) => Ok(d.isk),
            None => bail!("CPace: derive() first"),
        }
    }

    fn mac(&self, own: bool) -> Result<[u8; 64]> {
        let Some(d) = &self.derived else {
            bail!("CPace: derive() first");
        };
        let idx = if own == (self.role == Role::Initiator) { 0 } else { 1 };
        let (share, ad) = &d.sides[idx];
        let mut m = <HmacSha512 as Mac>::new_from_slice(&d.mac_key).expect("any key length");
        m.update(&lv_cat(&[share, ad]));
        Ok(m.finalize().into_bytes().into())
    }

    /// This side's confirmation tag (`Ta` for A, `Tb` for B).
    pub fn tag(&self) -> Result<[u8; 64]> {
        self.mac(true)
    }

    /// Constant-time check of the peer's confirmation tag.
    pub fn verify(&self, peer_tag: &[u8]) -> bool {
        let Some(d) = &self.derived else {
            return false;
        };
        if d.sides[0] == d.sides[1] {
            return false; // reflection
        }
        let Ok(expected) = self.mac(false) else {
            return false;
        };
        if peer_tag.len() != expected.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(peer_tag) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl Drop for CPace {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(s) = self.scalar.as_mut() {
            s.zeroize();
        }
        if let Some(d) = self.derived.as_mut() {
            d.isk.zeroize();
            d.mac_key.zeroize();
        }
    }
}

/// Field element sanity for tests.
#[allow(dead_code)]
fn is_zero(x: &BigUint) -> bool {
    x.is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    #[test]
    fn leb128_lengths() {
        assert_eq!(prepend_len(b"ab"), vec![2, b'a', b'b']);
        let long = vec![0u8; 200];
        let p = prepend_len(&long);
        assert_eq!(&p[..2], &[0xC8, 0x01]);
        assert_eq!(p.len(), 202);
    }

    #[test]
    fn honest_run_agrees_and_confirms() {
        let sid = b"sendspin-pair-pake-v1-test";
        let mut a = CPace::start(Role::Initiator, b"123456", sid, b"", b"server").unwrap();
        let mut b = CPace::start(Role::Responder, b"123456", sid, b"", b"client").unwrap();
        let ya = a.public_share;
        let yb = b.public_share;
        a.derive(&yb, b"client").unwrap();
        b.derive(&ya, b"server").unwrap();
        assert_eq!(a.isk().unwrap(), b.isk().unwrap());
        assert!(b.verify(&a.tag().unwrap()));
        assert!(a.verify(&b.tag().unwrap()));
    }

    #[test]
    fn wrong_pin_fails_confirmation() {
        let sid = b"sid";
        let mut a = CPace::start(Role::Initiator, b"123456", sid, b"", b"server").unwrap();
        let mut b = CPace::start(Role::Responder, b"654321", sid, b"", b"client").unwrap();
        let (ya, yb) = (a.public_share, b.public_share);
        a.derive(&yb, b"client").unwrap();
        b.derive(&ya, b"server").unwrap();
        assert!(!b.verify(&a.tag().unwrap()));
    }

    #[test]
    fn low_order_share_is_rejected() {
        let mut a = CPace::start(Role::Initiator, b"1", b"s", b"", b"server").unwrap();
        assert!(a.derive(&[0u8; 32], b"client").is_err());
    }

    /// Reference values from the Python `cpace` package (aiosendspin's
    /// PAKE) with fixed scalars.
    #[test]
    fn matches_python_cpace_reference() {
        let v = &super::test_vectors::CPACE;
        let sa: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_mul(3).wrapping_add(1));
        let sb: [u8; 32] = core::array::from_fn(|i| (i as u8).wrapping_mul(5).wrapping_add(2));
        let sid = b"sendspin-pair-pake-v1 fixed sid";
        assert_eq!(hex(&calculate_generator(b"12345678", b"", sid)), v.generator);
        let mut a = CPace::start_with_scalar(Role::Initiator, b"12345678", sid, b"", b"server", sa).unwrap();
        let mut b = CPace::start_with_scalar(Role::Responder, b"12345678", sid, b"", b"client", sb).unwrap();
        assert_eq!(hex(&a.public_share), v.ya);
        assert_eq!(hex(&b.public_share), v.yb);
        let (ya, yb) = (a.public_share, b.public_share);
        a.derive(&yb, b"client").unwrap();
        b.derive(&ya, b"server").unwrap();
        assert_eq!(hex(&a.isk().unwrap()), v.isk);
        assert_eq!(hex(&a.tag().unwrap()), v.ta);
        assert_eq!(hex(&b.tag().unwrap()), v.tb);
    }
}

#[cfg(test)]
#[path = "cpace_vectors.rs"]
mod test_vectors;
