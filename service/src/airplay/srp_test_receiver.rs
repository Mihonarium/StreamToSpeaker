//! Test-only receiver (accessory) side of HomeKit pair-setup SRP-6a
//! (3072-bit group, SHA-512), in the two byte conventions AirPlay
//! receivers are known to use, for testing [`crate::airplay::srp`]:
//!
//! * [`Convention::Adk`] — Apple's HomeKit ADK: a 16-byte salt; B sent
//!   without its leading zero bytes; the received A zero-extended to 384
//!   bytes; k = H(PAD(N) | PAD(g)); u = H(PAD(A) | PAD(B)); K = H(S without
//!   leading zero bytes); M1 hashes the salt, then A and B without their
//!   leading zero bytes; M2 = H(PAD(A) | M1 | K).
//! * [`Convention::PairAp`] — the pair_ap code shairport-sync runs: the
//!   salt is a random 128-bit number sent at natural length (15 bytes when
//!   its top byte is zero), B at natural length, k and u padded, K over S
//!   at natural length, M1 over A and B at natural length, and
//!   M2 = H(A at natural length | M1 | K).
//!
//! Written from those conventions; the group constant is RFC 5054's, not
//! srp.rs's.

use num_bigint::BigUint;
use num_traits::Zero;
use sha2::{Digest, Sha512};

pub const N_LEN: usize = 384;

const RFC5054_N_3072: &str = "
    FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1 29024E08
    8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD EF9519B3 CD3A431B
    302B0A6D F25F1437 4FE1356D 6D51C245 E485B576 625E7EC6 F44C42E9
    A637ED6B 0BFF5CB6 F406B7ED EE386BFB 5A899FA5 AE9F2411 7C4B1FE6
    49286651 ECE45B3D C2007CB8 A163BF05 98DA4836 1C55D39A 69163FA8
    FD24CF5F 83655D23 DCA3AD96 1C62F356 208552BB 9ED52907 7096966D
    670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B E39E772C
    180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9 DE2BCBF6 95581718
    3995497C EA956AE5 15D22618 98FA0510 15728E5A 8AAAC42D AD33170D
    04507A33 A85521AB DF1CBA64 ECFB8504 58DBEF0A 8AEA7157 5D060C7D
    B3970F85 A6E1E4C7 ABF5AE8C DB0933D7 1E8C94E0 4A25619D CEE3D226
    1AD2EE6B F12FFA06 D98A0864 D8760273 3EC86A64 521F2B18 177B200C
    BBE11757 7A615D6C 770988C0 BAD946E2 08E24FA0 74E5AB31 43DB5BFC
    E0FD108E 4B82D120 A93AD2CA FFFFFFFF FFFFFFFF";

pub fn n() -> BigUint {
    let hex: String = RFC5054_N_3072.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    BigUint::parse_bytes(hex.as_bytes(), 16).unwrap()
}

pub fn g() -> BigUint {
    BigUint::from(5u8)
}

pub fn h(parts: &[&[u8]]) -> Vec<u8> {
    let mut d = Sha512::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().to_vec()
}

/// Big-endian, left-padded with zeros to 384 bytes.
pub fn pad384(x: &BigUint) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0u8; N_LEN - b.len()];
    out.extend_from_slice(&b);
    out
}

/// The bytes after any leading zero bytes.
pub fn strip(b: &[u8]) -> &[u8] {
    &b[b.iter().take_while(|&&x| x == 0).count()..]
}

fn zero_extend(wire: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; N_LEN - wire.len()];
    out.extend_from_slice(wire);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Convention {
    Adk,
    PairAp,
}

/// What a receiver answers to M3.
pub enum M3Reply {
    /// M4 with the receiver's proof; `key` is the receiver's K.
    Proof { m2: Vec<u8>, key: Vec<u8> },
    /// M4 with TLV error 2 (Authentication) — what a wrong PIN gets.
    AuthError,
}

/// One pair-setup as the receiver runs it.
pub struct Receiver {
    conv: Convention,
    salt: Vec<u8>,
    v: BigUint,
    b: BigUint,
    big_b: BigUint,
}

impl Receiver {
    /// `salt16` and `b32` are the random bytes the receiver would draw.
    pub fn new(conv: Convention, pin: &str, salt16: [u8; 16], b32: [u8; 32]) -> Receiver {
        let (n, g) = (n(), g());
        let salt = match conv {
            Convention::Adk => salt16.to_vec(),
            Convention::PairAp => strip(&salt16).to_vec(),
        };
        let x = BigUint::from_bytes_be(&h(&[&salt, &h(&[b"Pair-Setup", b":", pin.as_bytes()])]));
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&h(&[&pad384(&n), &pad384(&g)]));
        let b = BigUint::from_bytes_be(&b32);
        let big_b = (&k * &v + g.modpow(&b, &n)) % &n;
        Receiver { conv, salt, v, b, big_b }
    }

    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    /// B as both conventions put it on the wire: without leading zeros.
    pub fn b_wire(&self) -> Vec<u8> {
        strip(&pad384(&self.big_b)).to_vec()
    }

    pub fn b_top_byte_zero(&self) -> bool {
        pad384(&self.big_b)[0] == 0
    }

    /// The receiver's S for a client A as received.
    pub fn premaster(&self, a_wire: &[u8]) -> BigUint {
        let n = n();
        let a384 = zero_extend(a_wire);
        let u = BigUint::from_bytes_be(&h(&[&a384, &pad384(&self.big_b)]));
        ((BigUint::from_bytes_be(&a384) * self.v.modpow(&u, &n)) % &n).modpow(&self.b, &n)
    }

    /// Handle M3: the client's A and proof M1.
    pub fn m3(&self, a_wire: &[u8], client_m1: &[u8]) -> M3Reply {
        let n = n();
        if a_wire.len() > N_LEN {
            return M3Reply::AuthError;
        }
        let a384 = zero_extend(a_wire);
        if (BigUint::from_bytes_be(&a384) % &n).is_zero() {
            return M3Reply::AuthError;
        }
        let key = h(&[strip(&pad384(&self.premaster(a_wire)))]);
        let hn = h(&[strip(&pad384(&n))]);
        let hg = h(&[strip(&pad384(&g()))]);
        let hng: Vec<u8> = hn.iter().zip(&hg).map(|(p, q)| p ^ q).collect();
        let b384 = pad384(&self.big_b);
        let m1 = h(&[&hng, &h(&[b"Pair-Setup"]), &self.salt, strip(&a384), strip(&b384), &key]);
        if m1 != client_m1 {
            return M3Reply::AuthError;
        }
        let m2 = match self.conv {
            Convention::Adk => h(&[&a384, &m1, &key]),
            Convention::PairAp => h(&[strip(&a384), &m1, &key]),
        };
        M3Reply::Proof { m2, key }
    }
}

/// Fixed random draws with the properties their names say (each test that
/// relies on a property asserts it). `A_*`/`B_*` are 32-byte exponents;
/// the B ones are paired with [`SALT`] and PIN `3939`.
pub mod vectors {
    pub const SALT: &str = "2e3fce77cf8c4c7478a96d207c1c3971";
    /// [`SALT`] with a zero top byte: pair_ap sends it as 15 bytes.
    pub const SALT_SHORT: &str = "003fce77cf8c4c7478a96d207c1c3971";
    /// g^a has a non-zero top byte.
    pub const A_FULL: &str = "aae7390e8952190594cf581d10289c057f2f4c0b438d1eef5e48fce074fd244a";
    /// g^a has a zero top byte.
    pub const A_SHORT: &str = "25ad6b9c825b33b7ff2024590c3e312827ac3562b616a894e5ed03e095fb6186";
    /// B has a non-zero top byte.
    pub const B_FULL: &str = "b920c366ae4069b2c7f825ff67de601eb74753ccee569cda8e86d7d577cbca6b";
    /// B has a zero top byte (383 bytes on the wire).
    pub const B_SHORT: &str = "211fbbdedb7193dfd11c399d25ff5ebb64a49e870d83677be56380489b0c6e47";
    /// With [`A_FULL`]: B is full length but S has a zero top byte.
    pub const B_FOR_SHORT_S: &str = "c0e3d5bf749b091d76ddba596d6c15265e18ca6fd4506d80936c134908d046de";

    pub fn bytes<const L: usize>(hex: &str) -> [u8; L] {
        let v: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        v.try_into().unwrap()
    }
}
