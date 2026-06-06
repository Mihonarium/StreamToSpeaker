//! SRP-6a client for HomeKit / AirPlay 2 transient pair-setup.
//!
//! HomeKit pairing runs SRP-6a with the **3072-bit group** (RFC 5054
//! Appendix A, which for ≥2048-bit takes its primes from RFC 3526; the
//! 3072-bit modulus here is RFC 3526 Group 15) and **SHA-512**. The
//! username is the literal `"Pair-Setup"` and, for *transient* pairing,
//! the password is the fixed `"3939"`.
//!
//! The maths follows RFC 5054 / Tom Wu's `libsrp` (which is what
//! `pair_ap` / OwnTone use, so the proofs match what a HomePod computes):
//!
//! ```text
//!   k  = H(N | PAD(g))
//!   x  = H(s | H(I | ":" | P))
//!   A  = g^a mod N                       (a = random, ≥256 bits)
//!   u  = H(PAD(A) | PAD(B))
//!   S  = (B - k * g^x) ^ (a + u*x) mod N
//!   K  = H(S)
//!   M1 = H( H(N) XOR H(g) | H(I) | s | A | B | K )   (client proof)
//!   M2 = H( A | M1 | K )                              (server proof)
//! ```
//!
//! The core (`k`, `x`, `u`, `S`) is unit-tested against the published
//! RFC 5054 Appendix B 1024-bit/SHA-1 vector, which validates the modpow,
//! padding and hashing plumbing independent of the production group.

use anyhow::{bail, Result};
use num_bigint::BigUint;
use num_traits::Zero;
use sha2::{Digest, Sha512};
use zeroize::Zeroize;

/// HomeKit SRP username.
pub const HOMEKIT_USERNAME: &str = "Pair-Setup";
/// Fixed PIN for HomeKit *transient* pairing.
pub const TRANSIENT_PIN: &str = "3939";

/// RFC 3526 Group 15 (3072-bit MODP) prime — the HomeKit SRP modulus.
/// Verified against RFC 3526 §4. Whitespace is stripped at parse time.
const N_3072_HEX: &str = "\
    FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1 \
    29024E08 8A67CC74 020BBEA6 3B139B22 514A0879 8E3404DD \
    EF9519B3 CD3A431B 302B0A6D F25F1437 4FE1356D 6D51C245 \
    E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED \
    EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE45B3D \
    C2007CB8 A163BF05 98DA4836 1C55D39A 69163FA8 FD24CF5F \
    83655D23 DCA3AD96 1C62F356 208552BB 9ED52907 7096966D \
    670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B \
    E39E772C 180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9 \
    DE2BCBF6 95581718 3995497C EA956AE5 15D22618 98FA0510 \
    15728E5A 8AAAC42D AD33170D 04507A33 A85521AB DF1CBA64 \
    ECFB8504 58DBEF0A 8AEA7157 5D060C7D B3970F85 A6E1E4C7 \
    ABF5AE8C DB0933D7 1E8C94E0 4A25619D CEE3D226 1AD2EE6B \
    F12FFA06 D98A0864 D8760273 3EC86A64 521F2B18 177B200C \
    BBE11757 7A615D6C 770988C0 BAD946E2 08E24FA0 74E5AB31 \
    43DB5BFC E0FD108E 4B82D120 A93AD2CA FFFFFFFF FFFFFFFF";

/// Hash algorithm — SHA-512 in production; SHA-1 only for the RFC test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Alg {
    Sha512,
    #[cfg(test)]
    Sha1,
}

impl Alg {
    fn digest(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            Alg::Sha512 => {
                let mut h = Sha512::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
            #[cfg(test)]
            Alg::Sha1 => {
                use sha1::Sha1;
                let mut h = Sha1::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
        }
    }
}

struct Group {
    n: BigUint,
    g: BigUint,
    n_len: usize,
    alg: Alg,
}

fn hex_to_biguint(hex: &str) -> BigUint {
    let clean: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    BigUint::parse_bytes(clean.as_bytes(), 16).expect("valid hex group constant")
}

/// Left-pad the big-endian bytes of `x` to exactly `len` bytes.
fn pad(x: &BigUint, len: usize) -> Vec<u8> {
    let b = x.to_bytes_be();
    if b.len() >= len {
        return b;
    }
    let mut out = vec![0u8; len - b.len()];
    out.extend_from_slice(&b);
    out
}

/// Result of the client-side SRP computation (the parts we test).
struct Computed {
    #[cfg_attr(not(test), allow(dead_code))]
    big_a: Vec<u8>,
    session_key: Vec<u8>,
    m1: Vec<u8>,
    #[cfg_attr(not(test), allow(dead_code))]
    x: BigUint,
    #[cfg_attr(not(test), allow(dead_code))]
    u: BigUint,
    #[cfg_attr(not(test), allow(dead_code))]
    s: BigUint,
}

fn compute(grp: &Group, identity: &str, password: &str, a: &BigUint, salt: &[u8], b: &BigUint) -> Result<Computed> {
    let Group { n, g, n_len, alg } = grp;
    let n_len = *n_len;
    let alg = *alg;

    if (b % n).is_zero() {
        bail!("SRP: server public B ≡ 0 mod N (invalid)");
    }

    let big_a = g.modpow(a, n);
    let a_bytes = pad(&big_a, n_len);
    let b_bytes = pad(b, n_len);

    // k = H(N | PAD(g))
    let k = BigUint::from_bytes_be(&alg.digest(&[&pad(n, n_len), &pad(g, n_len)]));

    // x = H(s | H(I | ":" | P))
    let inner = alg.digest(&[identity.as_bytes(), b":", password.as_bytes()]);
    let x = BigUint::from_bytes_be(&alg.digest(&[salt, &inner]));

    // u = H(PAD(A) | PAD(B))
    let u = BigUint::from_bytes_be(&alg.digest(&[&a_bytes, &b_bytes]));

    // S = (B - k * g^x) ^ (a + u*x) mod N, with modular subtraction.
    let gx = g.modpow(&x, n);
    let kgx = (&k * &gx) % n;
    let base = (b + n - kgx) % n; // b + n keeps it non-negative before mod
    let exp = a + &u * &x;
    let s = base.modpow(&exp, n);

    // K = H(S)
    let session_key = alg.digest(&[&s.to_bytes_be()]);

    // M1 = H( H(N) XOR H(g) | H(I) | s | A | B | K )
    let hn = alg.digest(&[&n.to_bytes_be()]);
    let hg = alg.digest(&[&g.to_bytes_be()]);
    let hn_xor_hg: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
    let hi = alg.digest(&[identity.as_bytes()]);
    let m1 = alg.digest(&[&hn_xor_hg, &hi, salt, &a_bytes, &b_bytes, &session_key]);

    Ok(Computed { big_a: a_bytes, session_key, m1, x, u, s })
}

/// A live SRP-6a client session for HomeKit transient pairing.
pub struct SrpClient {
    grp: Group,
    identity: String,
    password: String,
    a: BigUint,
    /// Padded client public key A (n_len bytes).
    public: Vec<u8>,
    /// Session key K = H(S) (64 bytes for SHA-512). Set after `process`.
    session_key: Vec<u8>,
    /// Client proof M1. Set after `process`.
    m1: Vec<u8>,
}

impl SrpClient {
    /// New HomeKit transient client (3072-bit group, SHA-512, username
    /// `Pair-Setup`, PIN `3939`). Generates the private exponent `a` and
    /// public `A` immediately.
    pub fn new_homekit_transient() -> Self {
        Self::new_homekit_with_pin(TRANSIENT_PIN)
    }

    pub fn new_homekit_with_pin(pin: &str) -> Self {
        let grp = Group {
            n: hex_to_biguint(N_3072_HEX),
            g: BigUint::from(5u8),
            n_len: 384,
            alg: Alg::Sha512,
        };
        // a: 256-bit random private exponent (RFC 5054 says ≥256 bits).
        let mut a_bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut a_bytes);
        let a = BigUint::from_bytes_be(&a_bytes);
        a_bytes.zeroize();
        let public = pad(&grp.g.modpow(&a, &grp.n), grp.n_len);
        Self {
            grp,
            identity: HOMEKIT_USERNAME.to_string(),
            password: pin.to_string(),
            a,
            public,
            session_key: Vec::new(),
            m1: Vec::new(),
        }
    }

    /// The padded client public key A to send in the M3 PublicKey TLV.
    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Process the server's salt + public key B (from M2), computing the
    /// shared secret, session key K and client proof M1.
    pub fn process(&mut self, salt: &[u8], server_public: &[u8]) -> Result<()> {
        let b = BigUint::from_bytes_be(server_public);
        let c = compute(&self.grp, &self.identity, &self.password, &self.a, salt, &b)?;
        self.session_key = c.session_key;
        self.m1 = c.m1;
        Ok(())
    }

    /// Client proof M1 to send in the M3 Proof TLV.
    pub fn proof(&self) -> &[u8] {
        &self.m1
    }

    /// Verify the server's proof M2 (from M4): M2 = H(A | M1 | K).
    pub fn verify_server(&self, server_proof: &[u8]) -> bool {
        if self.session_key.is_empty() {
            return false;
        }
        let expected = self
            .grp
            .alg
            .digest(&[&self.public, &self.m1, &self.session_key]);
        // Constant-time-ish compare (lengths fixed).
        expected.len() == server_proof.len()
            && expected
                .iter()
                .zip(server_proof.iter())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    /// The 64-byte SRP session key K (= SHA-512(S)). This is the
    /// "shared secret" the AirPlay 2 control + audio keys derive from.
    pub fn session_key(&self) -> &[u8] {
        &self.session_key
    }
}

impl Drop for SrpClient {
    fn drop(&mut self) {
        self.session_key.zeroize();
        self.m1.zeroize();
        // `a` is a BigUint; overwrite via replacement.
        self.a = BigUint::zero();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N_1024_HEX: &str = "\
        EEAF0AB9 ADB38DD6 9C33F80A FA8FC5E8 60726187 75FF3C0B 9EA2314C \
        9C256576 D674DF74 96EA81D3 383B4813 D692C6E0 E0D5D8E2 50B98BE4 \
        8E495C1D 6089DAD1 5DC7D7B4 6154D6B6 CE8EF4AD 69B15D49 82559B29 \
        7BCF1885 C529F566 660E57EC 68EDBC3C 05726CC0 2FD4CBF4 976EAA9A \
        FD5138FE 8376435B 9FC61D2F C0EB06E3";

    fn hx(s: &str) -> BigUint {
        hex_to_biguint(s)
    }

    /// RFC 5054 Appendix B 1024-bit / SHA-1 vector validates the core
    /// SRP maths (k, x, u, S) independent of the production group.
    #[test]
    fn rfc5054_1024_sha1_vector() {
        let grp = Group {
            n: hx(N_1024_HEX),
            g: BigUint::from(2u8),
            n_len: 128,
            alg: Alg::Sha1,
        };
        let a = hx("60975527 035CF2AD 1989806F 0407210B C81EDC04 E2762A56 AFD529DD DA2D4393");
        let b = hx("BD0C6151 2C692C0C B6D041FA 01BB152D 4916A1E7 7AF46AE1 05393011 BAF38964 \
                    DC46A067 0DD125B9 5A981652 236F99D9 B681CBF8 7837EC99 6C6DA044 53728610 \
                    D0C6DDB5 8B318885 D7D82C7F 8DEB75CE 7BD4FBAA 37089E6F 9C6059F3 88838E7A \
                    00030B33 1EB76840 910440B1 B27AAEAE EB4012B7 D7665238 A8E3FB00 4B117B58");
        let salt = hex_bytes("BEB25379 D1A8581E B5A72767 3A2441EE");

        let c = compute(&grp, "alice", "password123", &a, &salt, &b).unwrap();

        let want_x = hx("94B7555A ABE9127C C58CCF49 93DB6CF8 4D16C124");
        let want_u = hx("CE38B959 3487DA98 554ED47D 70A7AE5F 462EF019");
        let want_s = hx("B0DC82BA BCF30674 AE450C02 87745E79 90A3381F 63B387AA F271A10D 233861E3 \
                         59B48220 F7C4693C 9AE12B0A 6F67809F 0876E2D0 13800D6C 41BB59B6 D5979B5C \
                         00A172B4 A2A5903A 0BDCAF8A 709585EB 2AFAFA8F 3499B200 210DCC1F 10EB3394 \
                         3CD67FC8 8A2F39A4 BE5BEC4E C0A3212D C346D7E4 74B29EDE 8A469FFE CA686E5A");
        let want_a = hex_bytes("61D5E490 F6F1B795 47B0704C 436F523D D0E560F0 C64115BB 72557EC4 4352E890 \
                                3211C046 92272D8B 2D1A5358 A2CF1B6E 0BFCF99F 921530EC 8E393561 79EAE45E \
                                42BA92AE ACED8251 71E1E8B9 AF6D9C03 E1327F44 BE087EF0 6530E69F 66615261 \
                                EEF54073 CA11CF58 58F0EDFD FE15EFEA B349EF5D 76988A36 72FAC47B 0769447B");

        assert_eq!(c.x, want_x, "x mismatch");
        assert_eq!(c.u, want_u, "u mismatch");
        assert_eq!(c.s, want_s, "premaster S mismatch");
        assert_eq!(c.big_a, want_a, "client public A mismatch");

        // k = H(N | PAD(g)) from the same vector.
        let k = BigUint::from_bytes_be(&Alg::Sha1.digest(&[&pad(&grp.n, 128), &pad(&grp.g, 128)]));
        assert_eq!(k, hx("7556AA04 5AEF2CDD 07ABAF0F 665C3E81 8913186F"), "k mismatch");
    }

    #[test]
    fn homekit_group_is_3072_bit() {
        let c = SrpClient::new_homekit_transient();
        // Public key is padded to the 384-byte modulus length.
        assert_eq!(c.public_key().len(), 384);
    }

    #[test]
    fn process_yields_64_byte_key_and_proof() {
        // Self-consistency: run the client against a synthetic server B.
        // We don't have a HomePod here, but this exercises the 3072/SHA512
        // path end to end (no panics, correct lengths).
        let mut c = SrpClient::new_homekit_transient();
        let salt = [0x11u8; 16];
        // A plausible non-zero B (not a real server value; just structural).
        let b = vec![0x42u8; 384];
        c.process(&salt, &b).unwrap();
        assert_eq!(c.session_key().len(), 64);
        assert_eq!(c.proof().len(), 64);
        // M2 verification of a wrong proof must fail.
        assert!(!c.verify_server(&[0u8; 64]));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
            .collect()
    }
}
