//! SRP-6a client for HomeKit / AirPlay 2 pair-setup (transient and PIN).
//!
//! HomeKit pairing runs SRP-6a with the **3072-bit group** (RFC 5054
//! Appendix A, which for ≥2048-bit takes its primes from RFC 3526; the
//! 3072-bit modulus here is RFC 3526 Group 15) and **SHA-512**. The
//! username is the literal `"Pair-Setup"` and, for *transient* pairing,
//! the password is the fixed `"3939"`.
//!
//! Which numbers are hashed padded to the 384-byte modulus length and which
//! at their natural length (big-endian without leading zero bytes, the
//! RFC 2945 conversion) is part of the protocol. Getting one wrong only
//! shows when that value happens to start with a zero byte, about one
//! exchange in 256, and the receiver then answers exactly as it does to a
//! wrong PIN. HomeKit receivers (Apple's HomeKit ADK, and the pair_ap code
//! shairport-sync runs) use:
//!
//! ```text
//!   k  = H(N | PAD(g))
//!   x  = H(s | H(I | ":" | P))                       s exactly as received
//!   A  = g^a mod N                                   (a = random, 256 bits)
//!   u  = H(PAD(A) | PAD(B))
//!   S  = (B - k * g^x) ^ (a + u*x) mod N
//!   K  = H(S)                                        S at natural length
//!   M1 = H( H(N) XOR H(g) | H(I) | s | A | B | K )   A, B at natural length
//!   M2 = H( A | M1 | K )                              (server proof)
//! ```
//!
//! Receivers disagree about A in M2 (the ADK hashes it padded, pair_ap at
//! natural length), so `a` is drawn again until A's first byte is not
//! zero: the 384 bytes sent in M3 are then PAD(A) and A at natural length
//! at once, and no receiver can disagree with us about A. B is the
//! receiver's choice, so M1 hashes it at natural length whatever its length
//! on the wire.
//!
//! The core (`k`, `x`, `u`, `S`) is unit-tested against the published
//! RFC 5054 Appendix B 1024-bit/SHA-1 vector, and K, M1 and M2 against
//! receivers written to the ADK's and pair_ap's conventions, with A, B, S
//! and the salt each starting with a zero byte.

use anyhow::{bail, Result};
use num_bigint::BigUint;
use num_traits::Zero;
use rand::RngCore;
use sha2::{Digest, Sha512};
use zeroize::Zeroize;

/// HomeKit SRP username.
pub const HOMEKIT_USERNAME: &str = "Pair-Setup";
/// Fixed PIN for HomeKit *transient* pairing.
pub const TRANSIENT_PIN: &str = "3939";

/// Longest salt accepted. HomeKit receivers send 16 bytes; pair_ap sends
/// its 16-byte random salt at natural length, so 15 bytes (or fewer) about
/// one time in 256. The salt is hashed exactly as received.
const MAX_SALT_LEN: usize = 64;

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

impl Group {
    /// The HomeKit group: 3072-bit N, g = 5, SHA-512.
    fn homekit() -> Self {
        Group {
            n: hex_to_biguint(N_3072_HEX),
            g: BigUint::from(5u8),
            n_len: 384,
            alg: Alg::Sha512,
        }
    }
}

fn hex_to_biguint(hex: &str) -> BigUint {
    let clean: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    BigUint::parse_bytes(clean.as_bytes(), 16).expect("valid hex group constant")
}

/// RFC 5054 `PAD()`: the big-endian bytes of `x`, left-padded with zeros
/// to exactly `len` bytes.
fn pad(x: &BigUint, len: usize) -> Vec<u8> {
    let b = x.to_bytes_be();
    if b.len() >= len {
        return b;
    }
    let mut out = vec![0u8; len - b.len()];
    out.extend_from_slice(&b);
    out
}

/// `x` at its natural length: big-endian with no leading zero byte (the
/// RFC 2945 conversion), the form of every hash input not written `PAD()`.
fn natural(x: &BigUint) -> Vec<u8> {
    x.to_bytes_be()
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

    // RFC 5054 §2.5.3 aborts on B % N == 0; a B of N or more is not a value
    // a receiver can have computed either.
    if b.is_zero() || b >= n {
        bail!("SRP: server public B is not in 1..N (invalid)");
    }

    let big_a = g.modpow(a, n);

    // k = H(N | PAD(g))
    let k = BigUint::from_bytes_be(&alg.digest(&[&pad(n, n_len), &pad(g, n_len)]));

    // x = H(s | H(I | ":" | P)), the salt exactly as received.
    let inner = alg.digest(&[identity.as_bytes(), b":", password.as_bytes()]);
    let x = BigUint::from_bytes_be(&alg.digest(&[salt, &inner]));

    // u = H(PAD(A) | PAD(B))
    let u = BigUint::from_bytes_be(&alg.digest(&[&pad(&big_a, n_len), &pad(b, n_len)]));

    // S = (B - k * g^x) ^ (a + u*x) mod N, with modular subtraction.
    let gx = g.modpow(&x, n);
    let kgx = (&k * &gx) % n;
    let base = (b + n - kgx) % n; // b + n keeps it non-negative before mod
    let exp = a + &u * &x;
    let s = base.modpow(&exp, n);

    // K = H(S), S at natural length.
    let session_key = alg.digest(&[&natural(&s)]);

    // M1 = H( H(N) XOR H(g) | H(I) | s | A | B | K ), every number at its
    // natural length: receivers strip A's and B's leading zero bytes
    // before hashing them here.
    let hn = alg.digest(&[&natural(n)]);
    let hg = alg.digest(&[&natural(g)]);
    let hn_xor_hg: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
    let hi = alg.digest(&[identity.as_bytes()]);
    let m1 = alg.digest(&[&hn_xor_hg, &hi, salt, &natural(&big_a), &natural(b), &session_key]);

    Ok(Computed { big_a: pad(&big_a, n_len), session_key, m1, x, u, s })
}

/// A live SRP-6a client session for HomeKit pair-setup.
pub struct SrpClient {
    grp: Group,
    identity: String,
    password: String,
    a: BigUint,
    /// Client public key A as sent in M3: `n_len` bytes whose first byte is
    /// never zero, so it is PAD(A) and A at natural length at once.
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
        Self::with_rng(pin, &mut rand::thread_rng())
    }

    /// `a` is a 256-bit random exponent (RFC 5054 asks for at least 256
    /// bits), drawn again whenever A = g^a mod N would start with a zero
    /// byte: one draw in 256 does, and skipping those costs well under a
    /// hundredth of a bit of the exponent's entropy.
    fn with_rng<R: RngCore + ?Sized>(pin: &str, rng: &mut R) -> Self {
        let grp = Group::homekit();
        let (a, public) = loop {
            let mut a_bytes = [0u8; 32];
            rng.fill_bytes(&mut a_bytes);
            let a = BigUint::from_bytes_be(&a_bytes);
            a_bytes.zeroize();
            let public = pad(&grp.g.modpow(&a, &grp.n), grp.n_len);
            if public[0] != 0 {
                break (a, public);
            }
        };
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

    /// The client public key A to send in the M3 PublicKey TLV (384 bytes,
    /// first byte not zero).
    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Process the server's salt + public key B (from M2), computing the
    /// shared secret, session key K and client proof M1. M2's fields are
    /// checked here, so transient and PIN pair-setup refuse the same ones:
    /// a salt of 1 to [`MAX_SALT_LEN`] bytes, and B of at most 384 bytes
    /// with 1 ≤ B < N.
    pub fn process(&mut self, salt: &[u8], server_public: &[u8]) -> Result<()> {
        if salt.is_empty() || salt.len() > MAX_SALT_LEN {
            bail!("pair-setup M2 Salt is {} bytes, expected 1 to {}", salt.len(), MAX_SALT_LEN);
        }
        if server_public.len() > self.grp.n_len {
            bail!(
                "pair-setup M2 PublicKey is {} bytes, expected at most {}",
                server_public.len(),
                self.grp.n_len
            );
        }
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

    /// Verify the server's proof M2 (from M4): M2 = H(A | M1 | K). `public`
    /// doesn't start with a zero byte, so this is the padded and the
    /// natural-length convention at once.
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

/// Wipes the session key K, and the proof M1 (which went over the wire
/// anyway). Nothing else is: the PIN copy is freed as it is, and the
/// private exponent `a` and the big numbers worked out along the way (x, S,
/// a + u·x) are num-bigint values, which can't clear their limbs, so they
/// are freed with their contents — as are the copies num-bigint makes while
/// computing.
impl Drop for SrpClient {
    fn drop(&mut self) {
        self.session_key.zeroize();
        self.m1.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::srp_test_receiver::{
        pad384, vectors as v, vectors::bytes, Convention, M3Reply, Receiver,
    };

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

    const BOTH: [Convention; 2] = [Convention::Adk, Convention::PairAp];

    /// Hands out the given 32-byte draws in order.
    struct Draws(Vec<[u8; 32]>);

    impl RngCore for Draws {
        fn next_u32(&mut self) -> u32 {
            unreachable!()
        }
        fn next_u64(&mut self) -> u64 {
            unreachable!()
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            dest.copy_from_slice(&self.0.remove(0));
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), rand::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    fn client(a: &str) -> SrpClient {
        SrpClient::with_rng(TRANSIENT_PIN, &mut Draws(vec![bytes(a)]))
    }

    fn receiver(conv: Convention, salt: &str, b: &str) -> Receiver {
        Receiver::new(conv, TRANSIENT_PIN, bytes(salt), bytes(b))
    }

    /// One exchange: the client takes M2 as `b_wire`, the receiver checks
    /// M1, the client checks M2. Returns whether both sides accepted and
    /// agree on K.
    fn exchange(c: &mut SrpClient, rx: &Receiver, b_wire: &[u8]) -> bool {
        c.process(rx.salt(), b_wire).expect("client accepts M2");
        match rx.m3(c.public_key(), c.proof()) {
            M3Reply::Proof { m2, key } => c.verify_server(&m2) && c.session_key() == &key[..],
            M3Reply::AuthError => false,
        }
    }

    fn a_starts_with_zero(a: &str) -> bool {
        let grp = Group::homekit();
        pad(&grp.g.modpow(&BigUint::from_bytes_be(&bytes::<32>(a)), &grp.n), 384)[0] == 0
    }

    #[test]
    fn homekit_group_is_3072_bit() {
        let c = SrpClient::new_homekit_transient();
        assert_eq!(c.public_key().len(), 384);
        assert_ne!(c.public_key()[0], 0);
    }

    #[test]
    fn a_starting_with_a_zero_byte_is_drawn_again() {
        assert!(a_starts_with_zero(v::A_SHORT) && !a_starts_with_zero(v::A_FULL));
        let mut draws = Draws(vec![bytes(v::A_SHORT), bytes(v::A_FULL)]);
        let c = SrpClient::with_rng(TRANSIENT_PIN, &mut draws);
        assert!(draws.0.is_empty(), "the second draw was taken");
        assert_eq!(c.public_key(), &client(v::A_FULL).public_key()[..]);
    }

    /// Both receiver conventions, with B starting with a zero byte or not,
    /// and the short B on the wire at natural length (as receivers send
    /// it) or padded.
    #[test]
    fn receivers_of_both_conventions_accept_us() {
        for conv in BOTH {
            for (b, short) in [(v::B_FULL, false), (v::B_SHORT, true)] {
                let rx = receiver(conv, v::SALT, b);
                assert_eq!(rx.b_top_byte_zero(), short);
                let wire = rx.b_wire();
                assert_eq!(wire.len(), if short { 383 } else { 384 });
                assert!(exchange(&mut client(v::A_FULL), &rx, &wire), "{conv:?} short B={short}");
                let mut padded = vec![0u8; 384 - wire.len()];
                padded.extend_from_slice(&wire);
                assert!(exchange(&mut client(v::A_FULL), &rx, &padded), "{conv:?} padded B");
            }
        }
    }

    #[test]
    fn premaster_starting_with_a_zero_byte() {
        for conv in BOTH {
            let rx = receiver(conv, v::SALT, v::B_FOR_SHORT_S);
            let mut c = client(v::A_FULL);
            assert_eq!(pad384(&rx.premaster(c.public_key()))[0], 0, "vector: S is short");
            assert!(exchange(&mut c, &rx, &rx.b_wire()), "{conv:?}");
        }
    }

    #[test]
    fn short_salt_from_pair_ap() {
        let rx = receiver(Convention::PairAp, v::SALT_SHORT, v::B_FULL);
        assert_eq!(rx.salt().len(), 15);
        assert!(exchange(&mut client(v::A_FULL), &rx, &rx.b_wire()));
    }

    #[test]
    fn a_wrong_pin_is_refused() {
        let rx = Receiver::new(Convention::Adk, "123-45-678", bytes(v::SALT), bytes(v::B_FULL));
        let mut c = SrpClient::new_homekit_with_pin("123-45-679");
        c.process(rx.salt(), &rx.b_wire()).unwrap();
        assert!(matches!(rx.m3(c.public_key(), c.proof()), M3Reply::AuthError));
    }

    #[test]
    fn malformed_m2_is_refused() {
        let mut c = SrpClient::new_homekit_transient();
        let n = hex_to_biguint(N_3072_HEX).to_bytes_be();
        let n_minus_1 = (hex_to_biguint(N_3072_HEX) - 1u32).to_bytes_be();
        let cases: [(&[u8], &[u8], &str); 7] = [
            (&[], &[0x42; 384], "empty salt"),
            (&[0x11; 65], &[0x42; 384], "salt too long"),
            (&[0x11; 16], &[0x42; 385], "B too long"),
            (&[0x11; 16], &[0; 384], "B = 0"),
            (&[0x11; 16], &[], "B empty"),
            (&[0x11; 16], &n, "B = N"),
            (&[0x11; 16], &[0xFF; 384], "B > N"),
        ];
        for (salt, b, what) in cases {
            assert!(c.process(salt, b).is_err(), "{what}");
        }
        assert!(c.process(&[0x11; 64], &n_minus_1).is_ok(), "B = N - 1, 64-byte salt");
        assert!(c.process(&[0x11], &[0x01]).is_ok(), "B = 1, 1-byte salt");
    }

    #[test]
    fn process_yields_64_byte_key_and_proof() {
        // Structural run against a synthetic B: lengths, and a wrong
        // server proof is refused.
        let mut c = SrpClient::new_homekit_transient();
        c.process(&[0x11u8; 16], &[0x42u8; 384]).unwrap();
        assert_eq!(c.session_key().len(), 64);
        assert_eq!(c.proof().len(), 64);
        assert!(!c.verify_server(&[0u8; 64]));
    }

    /// A and M1 as a client that hashes the padded encoding sends them,
    /// for exponent `a`: A padded to 384 bytes even when it starts with a
    /// zero byte, and M1 over PAD(A) and PAD(B).
    fn padded_m1_exchange(a: [u8; 32], rx: &Receiver) -> (Vec<u8>, Vec<u8>) {
        let grp = Group::homekit();
        let a = BigUint::from_bytes_be(&a);
        let b = BigUint::from_bytes_be(&rx.b_wire());
        let c = compute(&grp, HOMEKIT_USERNAME, TRANSIENT_PIN, &a, rx.salt(), &b).unwrap();
        let hn = Alg::Sha512.digest(&[&natural(&grp.n)]);
        let hg = Alg::Sha512.digest(&[&natural(&grp.g)]);
        let hng: Vec<u8> = hn.iter().zip(&hg).map(|(x, y)| x ^ y).collect();
        let hi = Alg::Sha512.digest(&[HOMEKIT_USERNAME.as_bytes()]);
        let m1 = Alg::Sha512.digest(&[&hng, &hi, rx.salt(), &c.big_a, &pad(&b, 384), &c.session_key]);
        (c.big_a, m1)
    }

    /// The padded encoding agrees with HomeKit receivers only while
    /// neither A nor B starts with a zero byte — about 127 exchanges in
    /// 128.
    #[test]
    fn padded_m1_is_refused_when_a_or_b_starts_with_a_zero_byte() {
        for conv in BOTH {
            for (a, b, accepted) in [
                (v::A_FULL, v::B_FULL, true),
                (v::A_SHORT, v::B_FULL, false),
                (v::A_FULL, v::B_SHORT, false),
            ] {
                let rx = receiver(conv, v::SALT, b);
                let (a_wire, m1) = padded_m1_exchange(bytes(a), &rx);
                let got = matches!(rx.m3(&a_wire, &m1), M3Reply::Proof { .. });
                assert_eq!(got, accepted, "{conv:?} A={a} B={b}");
            }
        }
    }

    /// Random exchanges against both conventions: how often the padded
    /// encoding fails, and that this client never does. Slow; run with
    /// `STS_SRP_TRIALS=2000 cargo test --release --lib
    /// srp::tests::random_exchanges -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn random_exchanges() {
        let trials: usize = std::env::var("STS_SRP_TRIALS").ok().and_then(|s| s.parse().ok()).unwrap_or(500);
        let mut rng = rand::thread_rng();
        let (mut a_short, mut b_short, mut padded_failed, mut failed) = (0usize, 0usize, 0usize, 0usize);
        for _ in 0..trials {
            let (mut salt, mut b, mut a) = ([0u8; 16], [0u8; 32], [0u8; 32]);
            rng.fill_bytes(&mut salt);
            rng.fill_bytes(&mut b);
            rng.fill_bytes(&mut a);
            for conv in BOTH {
                let rx = Receiver::new(conv, TRANSIENT_PIN, salt, b);
                let (a_wire, m1) = padded_m1_exchange(a, &rx);
                let short = (a_wire[0] == 0, rx.b_top_byte_zero());
                a_short += short.0 as usize;
                b_short += short.1 as usize;
                let padded_ok = matches!(rx.m3(&a_wire, &m1), M3Reply::Proof { .. });
                // The padded encoding fails exactly when A or B is short.
                assert_eq!(padded_ok, !(short.0 || short.1), "{conv:?}");
                padded_failed += !padded_ok as usize;
                let mut c = SrpClient::with_rng(TRANSIENT_PIN, &mut rng);
                if !exchange(&mut c, &rx, &rx.b_wire()) {
                    failed += 1;
                }
            }
        }
        println!(
            "{} exchanges: A short {}, B short {}; the padded encoding failed {}, this client failed {}",
            trials * 2,
            a_short,
            b_short,
            padded_failed,
            failed
        );
        assert_eq!(failed, 0);
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
            .collect()
    }
}
