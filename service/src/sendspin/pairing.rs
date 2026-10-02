//! Pairing exchanges over an established (encrypted) connection, both as
//! the client (Music Assistant pairing with our source) and as the server
//! (us pairing with a player).
//!
//! * **Pairing PSK** — the connection is keyed with the client's pairing
//!   PSK (the server got it from a pairing token); the client simply sends
//!   a fresh long-term PSK in `client/pair-finalize`.
//! * **Pairing code (dynamic / static)** — over a Sentinel-keyed
//!   connection: a CPace run on the code authenticates both sides, then
//!   the long-term PSK crosses the wire wrapped under the CPace output.
//!   For the dynamic code the client commits to `nonce_B` first, the server
//!   sends `nonce_A`, and the code is derived from the Noise handshake hash
//!   and both nonces — the client displays it, the operator types it into
//!   the server.
//!
//! After `server/pair-finalize` both sides store the record and the server
//! re-handshakes onto the new long-term PSK.

use anyhow::{anyhow, bail, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

use super::channel::{envelope, msg_type, payload, ChannelWriter};
use super::cpace::{self, CPace};
use super::keys::{b64url, b64url_decode};
use super::noise::Suite;
use super::proto::Dialect;
use super::pump::Event;

const PAKE_SID_LABEL: &[u8] = b"sendspin-pair-pake-v1";
const COMMIT_LABEL: &[u8] = b"sendspin-pair-commit-v1";
const PIN_DERIVE_LABEL_V9: &[u8] = b"sendspin-pin-derive-v1";
const CODE_DERIVE_LABEL_SPEC: &[u8] = b"sendspin-pairing-code-derive-v1";
const PSK_WRAP_LABEL: &[u8] = b"sendspin-pair-psk-wrap-v1";
const NONCE_WRAP_LABEL: &[u8] = b"sendspin-pair-nonce-wrap-v1";
const AD_SERVER: &[u8] = b"server";
const AD_CLIENT: &[u8] = b"client";

/// Client-side bound on one attempt (spec: recommended 2 minutes).
pub const CLIENT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Server-side bound while waiting for the operator / the client.
pub const SERVER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(180);

/// Shared context of one attempt.
pub struct PairingCtx<'a> {
    pub dialect: Dialect,
    pub writer: &'a ChannelWriter,
    pub events: &'a Receiver<Event>,
    pub handshake_hash: [u8; 32],
    /// Number of pairing `server/activate`s since the last handshake
    /// (this attempt's index, ≥ 1).
    pub pairing_index: u32,
    pub suite: Suite,
}

#[derive(Debug)]
pub enum PairOutcome {
    /// Both sides agreed on this new long-term PSK.
    Finalized { long_term_psk: [u8; 32] },
    /// The server ended pairing with this `server/activate` instead.
    Left(Value),
    /// A `pair/abort` was sent or received.
    Aborted(String),
}

fn sid(d: Dialect, h: &[u8; 32], pairing_index: u32) -> Vec<u8> {
    let mut s = PAKE_SID_LABEL.to_vec();
    s.extend_from_slice(h);
    s.extend_from_slice(&pairing_index.to_be_bytes());
    if d == Dialect::Spec {
        s.extend_from_slice(&1u32.to_be_bytes()); // round 1
    }
    s
}

pub fn commit(nonce_b: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(COMMIT_LABEL);
    h.update(nonce_b);
    h.finalize().into()
}

/// Derive the dynamic pairing code from the handshake hash and nonces.
pub fn derive_code(d: Dialect, h: &[u8; 32], nonce_a: &[u8; 32], nonce_b: &[u8; 32], digits: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(match d {
        Dialect::V9 => PIN_DERIVE_LABEL_V9,
        Dialect::Spec => CODE_DERIVE_LABEL_SPEC,
    });
    hasher.update(h);
    hasher.update(nonce_a);
    hasher.update(nonce_b);
    let digest = hasher.finalize();
    let digits = digits.clamp(4, 12);
    // uint256_be(digest) mod 10^digits, by long division over the bytes.
    let modulus = 10u128.pow(digits);
    let mut rem: u128 = 0;
    for b in digest.iter() {
        rem = (rem * 256 + *b as u128) % modulus;
    }
    format!("{:0width$}", rem, width = digits as usize)
}

fn wrap_key(label: &[u8], sid: &[u8], isk: &[u8; 64]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(label);
    h.update(sid);
    h.update(isk);
    h.finalize().into()
}

fn wrap(suite: Suite, key: &[u8; 32], value: &[u8; 32]) -> Result<Vec<u8>> {
    suite.seal(key, &[0u8; 12], &[], value)
}

fn unwrap(suite: Suite, key: &[u8; 32], sealed: &[u8]) -> Result<[u8; 32]> {
    let pt = suite.open(key, &[0u8; 12], &[], sealed)?;
    pt.try_into().map_err(|_| anyhow!("wrapped value has the wrong length"))
}

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

fn field_bytes(v: &Value, key: &str, len: usize) -> Result<Vec<u8>> {
    let s = payload(v)
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{} missing {}", msg_type(v), key))?;
    let b = b64url_decode(s)?;
    if b.len() != len {
        bail!("{}: {} must be {} bytes, got {}", msg_type(v), key, len, b.len());
    }
    Ok(b)
}

fn field32(v: &Value, key: &str) -> Result<[u8; 32]> {
    Ok(field_bytes(v, key, 32)?.try_into().expect("checked length"))
}

/// Wait for the next pairing-relevant message: one of `expected`, a
/// `pair/abort`, or a `server/activate` (leave). Unrelated traffic (late
/// `server/time` replies, state pushes) is skipped.
enum Next {
    Msg(Value),
    Abort(String),
    Leave(Value),
}

fn next(ctx: &PairingCtx<'_>, expected: &[&str], deadline: Instant) -> Result<Next> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = match ctx.events.recv_timeout(left) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for {}", expected.join(" or ")),
            Err(RecvTimeoutError::Disconnected) => bail!("connection closed during pairing"),
        };
        match ev {
            Event::Json { value, received_at } => {
                let received_us = super::instant_to_us(received_at);
                let t = msg_type(&value);
                if expected.contains(&t) {
                    return Ok(Next::Msg(value));
                }
                match t {
                    // Server side: keep answering the player's clock sync
                    // while the pairing exchange runs.
                    "client/time" => {
                        if let Some(t1) = payload(&value).get("client_transmitted").and_then(Value::as_i64) {
                            let _ = ctx.writer.send_json(&super::proto::server_time(t1, received_us, super::now_us()));
                        }
                        continue;
                    }
                    "pair/abort" => {
                        let reason = payload(&value).get("reason").and_then(Value::as_str).unwrap_or("unknown");
                        return Ok(Next::Abort(reason.to_string()));
                    }
                    "server/activate" => return Ok(Next::Leave(value)),
                    _ => continue,
                }
            }
            Event::Binary { .. } => continue,
            Event::Rehandshake(_) => bail!("unexpected re-handshake during pairing"),
            Event::Closed(e) => bail!("connection closed during pairing{}", e.map(|e| format!(": {}", e)).unwrap_or_default()),
        }
    }
}

fn send_abort(ctx: &PairingCtx<'_>, reason: &str) -> PairOutcome {
    let _ = ctx.writer.send_json(&envelope("pair/abort", json!({ "reason": reason })));
    PairOutcome::Aborted(reason.to_string())
}

fn code_mismatch_reason(d: Dialect) -> &'static str {
    match d {
        Dialect::V9 => "pin_mismatch",
        Dialect::Spec => "pairing_code_mismatch",
    }
}

/// Client: wait for `server/pair-finalize` after sending ours.
fn client_await_finalize(ctx: &PairingCtx<'_>, psk: [u8; 32], deadline: Instant) -> Result<PairOutcome> {
    match next(ctx, &["server/pair-finalize"], deadline)? {
        Next::Msg(_) => Ok(PairOutcome::Finalized { long_term_psk: psk }),
        Next::Abort(r) => Ok(PairOutcome::Aborted(r)),
        Next::Leave(v) => Ok(PairOutcome::Left(v)),
    }
}

/// Client side of the Pairing PSK flow (the connection is keyed with our
/// pairing PSK).
pub fn client_pairing_psk(ctx: &PairingCtx<'_>) -> Result<PairOutcome> {
    let deadline = Instant::now() + CLIENT_ATTEMPT_TIMEOUT;
    let psk = random32();
    if ctx.dialect == Dialect::Spec {
        // Current spec: pair-init then pair-finalize, back to back.
        // (aiosendspin 9.x servers expect only the finalize.)
        ctx.writer
            .send_json(&envelope("client/pair-init", json!({ "pairing_index": ctx.pairing_index })))?;
    }
    ctx.writer
        .send_json(&envelope("client/pair-finalize", json!({ "long_term_psk": b64url(&psk) })))?;
    client_await_finalize(ctx, psk, deadline)
}

/// Client side of the dynamic pairing code flow. `show` is called with
/// the code to display (and with `None` once the attempt ends).
pub fn client_dynamic_code(ctx: &PairingCtx<'_>, digits: u32, show: &dyn Fn(Option<String>)) -> Result<PairOutcome> {
    let result = client_dynamic_code_inner(ctx, digits, show);
    show(None);
    result
}

fn client_dynamic_code_inner(ctx: &PairingCtx<'_>, digits: u32, show: &dyn Fn(Option<String>)) -> Result<PairOutcome> {
    let deadline = Instant::now() + CLIENT_ATTEMPT_TIMEOUT;
    let d = ctx.dialect;
    let sid = sid(d, &ctx.handshake_hash, ctx.pairing_index);
    let nonce_b = random32();
    ctx.writer.send_json(&envelope(
        "client/pair-init",
        json!({ "pairing_index": ctx.pairing_index, "commit_B": b64url(&commit(&nonce_b)) }),
    ))?;
    let init = match next(ctx, &["server/pair-init"], deadline)? {
        Next::Msg(v) => v,
        Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
        Next::Leave(v) => return Ok(PairOutcome::Left(v)),
    };
    let nonce_a = field32(&init, "nonce_A")?;
    let digits = if d == Dialect::Spec { 6 } else { digits };
    let code = derive_code(d, &ctx.handshake_hash, &nonce_a, &nonce_b, digits);
    show(Some(code.clone()));
    let mut pake = CPace::start(cpace::Role::Responder, code.as_bytes(), &sid, b"", AD_CLIENT)?;

    let auth = match next(ctx, &["server/pair-auth"], deadline)? {
        Next::Msg(v) => v,
        Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
        Next::Leave(v) => return Ok(PairOutcome::Left(v)),
    };
    ctx.writer
        .send_json(&envelope("client/pair-auth", json!({ "pake_msg_2": b64url(&pake.public_share) })))?;
    pake.derive(&field_bytes(&auth, "pake_msg_1", 32)?, AD_SERVER)?;

    let confirm = match next(ctx, &["server/pair-confirm"], deadline)? {
        Next::Msg(v) => v,
        Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
        Next::Leave(v) => return Ok(PairOutcome::Left(v)),
    };
    if !pake.verify(&field_bytes(&confirm, "server_kc", 64)?) {
        return Ok(send_abort(ctx, code_mismatch_reason(d)));
    }
    let isk = pake.isk()?;
    let mut confirm_payload = json!({ "client_kc": b64url(&pake.tag()?) });
    match d {
        Dialect::V9 => confirm_payload["nonce_B"] = json!(b64url(&nonce_b)),
        Dialect::Spec => {
            let k = wrap_key(NONCE_WRAP_LABEL, &sid, &isk);
            confirm_payload["wrapped_nonce_B"] = json!(b64url(&wrap(ctx.suite, &k, &nonce_b)?));
        }
    }
    ctx.writer.send_json(&envelope("client/pair-confirm", confirm_payload))?;
    let psk = random32();
    let k = wrap_key(PSK_WRAP_LABEL, &sid, &isk);
    ctx.writer.send_json(&envelope(
        "client/pair-finalize",
        json!({ "wrapped_psk": b64url(&wrap(ctx.suite, &k, &psk)?) }),
    ))?;
    client_await_finalize(ctx, psk, deadline)
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// Server side of the Pairing PSK flow (connection keyed with the
/// player's pairing PSK, activation already sent).
pub fn server_pairing_psk(ctx: &PairingCtx<'_>) -> Result<PairOutcome> {
    let deadline = Instant::now() + SERVER_ATTEMPT_TIMEOUT;
    loop {
        match next(ctx, &["client/pair-init", "client/pair-pending", "client/pair-finalize"], deadline)? {
            Next::Msg(v) if msg_type(&v) == "client/pair-finalize" => {
                let psk = field32(&v, "long_term_psk")?;
                return Ok(PairOutcome::Finalized { long_term_psk: psk });
            }
            Next::Msg(_) => continue, // pair-init / pair-pending precede the finalize
            Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
            Next::Leave(_) => bail!("client sent server/activate"),
        }
    }
}

/// Server side of a pairing-code flow. `ask_code` blocks until the
/// operator typed the code (or returns `None` = cancelled); it gets
/// `true` while the client reported the attempt held back (pending).
pub fn server_code(ctx: &PairingCtx<'_>, dynamic: bool, ask_code: &dyn Fn() -> Option<String>) -> Result<PairOutcome> {
    let deadline = Instant::now() + SERVER_ATTEMPT_TIMEOUT;
    let d = ctx.dialect;
    let sid = sid(d, &ctx.handshake_hash, ctx.pairing_index);
    // client/pair-init (possibly after a pair-pending). A compliant client
    // answers the activation at once, so don't hold the caller's
    // "connecting" state for minutes; a pending (gesture-gated) attempt
    // gets the full window.
    let mut init_deadline = Instant::now() + Duration::from_secs(20);
    let init = loop {
        match next(ctx, &["client/pair-init", "client/pair-pending"], init_deadline.min(deadline))? {
            Next::Msg(v) if msg_type(&v) == "client/pair-init" => {
                let idx = payload(&v).get("pairing_index").and_then(Value::as_u64).unwrap_or(0) as u32;
                if idx < ctx.pairing_index {
                    continue; // leftover from a superseded attempt
                }
                break v;
            }
            Next::Msg(_) => {
                init_deadline = deadline; // pair-pending: wait for the gesture
                continue;
            }
            Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
            Next::Leave(_) => bail!("client sent server/activate"),
        }
    };
    let (nonce_a, commit_b) = if dynamic {
        let commit_b = field32(&init, "commit_B")?;
        let nonce_a = random32();
        ctx.writer
            .send_json(&envelope("server/pair-init", json!({ "nonce_A": b64url(&nonce_a) })))?;
        (Some(nonce_a), Some(commit_b))
    } else {
        (None, None)
    };
    let Some(code) = ask_code() else {
        return Ok(send_abort(ctx, "user_cancelled"));
    };
    let code: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if code.is_empty() {
        return Ok(send_abort(ctx, "user_cancelled"));
    }
    let mut pake = CPace::start(cpace::Role::Initiator, code.as_bytes(), &sid, b"", AD_SERVER)?;
    ctx.writer
        .send_json(&envelope("server/pair-auth", json!({ "pake_msg_1": b64url(&pake.public_share) })))?;
    let auth = match next(ctx, &["client/pair-auth"], deadline)? {
        Next::Msg(v) => v,
        Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
        Next::Leave(_) => bail!("client sent server/activate"),
    };
    pake.derive(&field_bytes(&auth, "pake_msg_2", 32)?, AD_CLIENT)?;
    ctx.writer
        .send_json(&envelope("server/pair-confirm", json!({ "server_kc": b64url(&pake.tag()?) })))?;
    let confirm = match next(ctx, &["client/pair-confirm"], deadline)? {
        Next::Msg(v) => v,
        Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
        Next::Leave(_) => bail!("client sent server/activate"),
    };
    let isk = pake.isk()?;
    if !pake.verify(&field_bytes(&confirm, "client_kc", 64)?) {
        return Ok(send_abort(ctx, code_mismatch_reason(d)));
    }
    if let (Some(nonce_a), Some(commit_b)) = (nonce_a, commit_b) {
        let nonce_b: [u8; 32] = if let Ok(n) = field32(&confirm, "nonce_B") {
            n
        } else {
            let sealed = field_bytes(&confirm, "wrapped_nonce_B", 48)?;
            unwrap(ctx.suite, &wrap_key(NONCE_WRAP_LABEL, &sid, &isk), &sealed)?
        };
        if commit(&nonce_b) != commit_b {
            bail!("revealed nonce does not match the commitment");
        }
        if derive_code(d, &ctx.handshake_hash, &nonce_a, &nonce_b, code.len() as u32) != code {
            bail!("pairing code does not match this connection");
        }
    }
    let fin = match next(ctx, &["client/pair-finalize"], deadline)? {
        Next::Msg(v) => v,
        Next::Abort(r) => return Ok(PairOutcome::Aborted(r)),
        Next::Leave(_) => bail!("client sent server/activate"),
    };
    let sealed = field_bytes(&fin, "wrapped_psk", 48)?;
    let psk = unwrap(ctx.suite, &wrap_key(PSK_WRAP_LABEL, &sid, &isk), &sealed)?;
    Ok(PairOutcome::Finalized { long_term_psk: psk })
}

/// Server: acknowledge a finalized pairing (after persisting the record).
pub fn server_send_finalize(writer: &ChannelWriter) -> Result<()> {
    writer.send_json(&envelope("server/pair-finalize", json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_derivation_matches_aiosendspin() {
        // Values from aiosendspin 9.1.1 `pin.derive_pin` (h = 00.., nonce_A
        // = 01.., nonce_B = 02.., 6 digits) and the commitment of 02...
        let h = [0u8; 32];
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_eq!(derive_code(Dialect::V9, &h, &a, &b, 6), super::super::pairing_vectors::V9_PIN_6);
        assert_eq!(derive_code(Dialect::V9, &h, &a, &b, 8), super::super::pairing_vectors::V9_PIN_8);
        assert_eq!(hex(&commit(&b)), super::super::pairing_vectors::COMMIT_02);
        let c = derive_code(Dialect::Spec, &h, &a, &b, 6);
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn wrap_roundtrip() {
        let k = [5u8; 32];
        let v = [6u8; 32];
        for s in [Suite::ChaChaPoly, Suite::AesGcm] {
            let sealed = wrap(s, &k, &v).unwrap();
            assert_eq!(sealed.len(), 48);
            assert_eq!(unwrap(s, &k, &sealed).unwrap(), v);
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }
}
