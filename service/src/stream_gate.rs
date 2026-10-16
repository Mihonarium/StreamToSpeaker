//! The gate for `/stream.raw`: who may pull the audio stream.
//!
//! One rule, applied per request and when pruning open streams:
//!
//! - **This PC** (loopback, or one of this machine's own addresses) is
//!   always admitted.
//! - **Every other device** is admitted only while streaming is enabled —
//!   the same flag as `App::streaming_enabled`, shared, not copied — so
//!   Disable streaming and Forget speaker close the stream to the network
//!   whatever the privacy setting. Nothing that plays to a speaker needs
//!   it then: UPnP sessions are stopped, and AirPlay, Sendspin and the
//!   Music Assistant input take their audio in-process.
//! - With **privacy mode** on, that other device must also be covered by
//!   a live **grant**; with it off, any device on the network may listen
//!   while streaming is enabled.
//!
//! A grant is a set of addresses handed to a speaker for as long as its
//! session lasts: [`StreamGate::grant`] returns a [`Grant`] that the
//! session owns, and dropping the session drops the grant, which revokes
//! the addresses. That gives three properties:
//!
//! - **No race between overlapping bring-ups.** Two speakers being
//!   selected at once (GUI click racing a tray Enable, a remote
//!   `/api/select`, launch auto-reconnect) each hold their own grant;
//!   neither can clear the other's.
//! - **Nothing to forget to clear.** The grant's lifetime *is* the
//!   session's lifetime — a failed bring-up drops it on the error path
//!   by construction, and a replaced/stopped session revokes on drop.
//! - **Never blocks on the session lock.** The gate has one tiny mutex
//!   of its own, so the HTTP accept thread (which consults it per
//!   request) can't be parked behind a SOAP call that another thread is
//!   making while holding `App::session`.
//!
//! Pruning existing connections uses [`StreamGate::snapshot`]: a copy of
//! the allow-set taken under the brief lock, then applied as a pure
//! predicate — so the hub's subscriber lock (taken by the audio thread
//! per packet) is never held while waiting on anything.

use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A refusal from the same address is surfaced to the user at most this
/// often — Sonos re-probes a refused URL every few seconds.
pub const REFUSAL_REPORT_INTERVAL: Duration = Duration::from_secs(60);

pub struct StreamGate {
    /// Mirrors `user_config.privacy_mode`, so the HTTP threads never
    /// touch the config lock.
    privacy: AtomicBool,
    /// `App::streaming_enabled` itself (the same `Arc`): a copy could
    /// drift from it and refuse the speaker being played on, or admit
    /// devices after the user stopped streaming.
    streaming: Arc<AtomicBool>,
    /// This machine's own address(es) besides loopback — the advertised
    /// IP, admitted only after proving the host actually owns it.
    local: Vec<IpAddr>,
    /// Live grants: id → canonical addresses. Held only for lookups.
    grants: Mutex<HashMap<u64, Vec<IpAddr>>>,
    next_id: AtomicU64,
    /// Last time a refusal from each peer was reported (rate limit).
    refusals: Mutex<HashMap<IpAddr, Instant>>,
}

/// Addresses admitted for as long as this value lives. Owned by the
/// speaker session it was issued for; dropping it revokes them.
pub struct Grant {
    gate: Arc<StreamGate>,
    id: u64,
    ips: Vec<IpAddr>,
}

impl Grant {
    pub fn ips(&self) -> &[IpAddr] {
        &self.ips
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.gate.grants.lock().unwrap().remove(&self.id);
    }
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant").field("id", &self.id).field("ips", &self.ips).finish()
    }
}

/// Why a device may not pull the stream right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Streaming is disabled: only this PC may listen.
    StreamingOff,
    /// Privacy mode, and the device isn't one the speaker being played on
    /// (or its group) was granted.
    NotGranted,
}

impl Refusal {
    /// The reason in a few words, for the refused device's 403 and the
    /// log: the two rules are told apart, so a device cut off by Disable
    /// isn't taken for one privacy mode refused.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::StreamingOff => "streaming is disabled on this PC",
            Refusal::NotGranted => {
                "privacy mode is on: only the speaker being played on (and its group) may listen"
            }
        }
    }
}

/// The gate's rule (see the module docs), over its inputs at one moment.
/// `granted` is asked only when privacy mode decides.
fn decide(
    peer: IpAddr,
    privacy: bool,
    streaming: bool,
    local: &[IpAddr],
    granted: impl FnOnce(IpAddr) -> bool,
) -> Result<(), Refusal> {
    let peer = canonical_ip(peer);
    if peer.is_loopback() || local.contains(&peer) {
        return Ok(());
    }
    if !streaming {
        return Err(Refusal::StreamingOff);
    }
    if !privacy || granted(peer) {
        return Ok(());
    }
    Err(Refusal::NotGranted)
}

/// A point-in-time copy of the allow-set: a pure predicate for pruning.
#[derive(Clone, Debug)]
pub struct AllowSnapshot {
    privacy: bool,
    streaming: bool,
    local: Vec<IpAddr>,
    granted: Vec<IpAddr>,
}

impl AllowSnapshot {
    pub fn privacy(&self) -> bool {
        self.privacy
    }

    pub fn streaming(&self) -> bool {
        self.streaming
    }

    pub fn allows(&self, peer: IpAddr) -> bool {
        decide(peer, self.privacy, self.streaming, &self.local, |p| self.granted.contains(&p)).is_ok()
    }
}

impl StreamGate {
    /// `streaming`: the app's streaming-enabled flag itself.
    /// `local`: this host's own advertised address, if it really is
    /// local (see [`is_local_address`]).
    pub fn new(
        privacy: bool,
        streaming: Arc<AtomicBool>,
        local: impl IntoIterator<Item = IpAddr>,
    ) -> Arc<Self> {
        Arc::new(Self {
            privacy: AtomicBool::new(privacy),
            streaming,
            local: local.into_iter().map(canonical_ip).collect(),
            grants: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            refusals: Mutex::new(HashMap::new()),
        })
    }

    fn streaming(&self) -> bool {
        self.streaming.load(Ordering::Acquire)
    }

    pub fn privacy(&self) -> bool {
        self.privacy.load(Ordering::Acquire)
    }

    /// Returns whether the value changed.
    pub fn set_privacy(&self, on: bool) -> bool {
        self.privacy.swap(on, Ordering::AcqRel) != on
    }

    /// Admit `ips` until the returned [`Grant`] is dropped. Addresses are
    /// canonicalised and de-duplicated; an empty grant is harmless.
    pub fn grant(self: &Arc<Self>, ips: impl IntoIterator<Item = IpAddr>) -> Grant {
        let mut list: Vec<IpAddr> = ips.into_iter().map(canonical_ip).collect();
        list.sort();
        list.dedup();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.grants.lock().unwrap().insert(id, list.clone());
        Grant { gate: self.clone(), id, ips: list }
    }

    /// May `peer` pull the stream right now? Brief lock only.
    pub fn allows(&self, peer: IpAddr) -> bool {
        self.check(peer).is_ok()
    }

    /// [`allows`](Self::allows), saying why not.
    pub fn check(&self, peer: IpAddr) -> Result<(), Refusal> {
        decide(peer, self.privacy(), self.streaming(), &self.local, |p| self.is_granted(p))
    }

    /// Is `peer` one of the addresses a live session granted, whatever
    /// the privacy setting? (The selected speaker and its group.)
    pub fn is_granted(&self, peer: IpAddr) -> bool {
        let peer = canonical_ip(peer);
        self.grants.lock().unwrap().values().any(|ips| ips.contains(&peer))
    }

    pub fn snapshot(&self) -> AllowSnapshot {
        let granted = self
            .grants
            .lock()
            .unwrap()
            .values()
            .flat_map(|v| v.iter().copied())
            .collect();
        AllowSnapshot {
            privacy: self.privacy(),
            streaming: self.streaming(),
            local: self.local.clone(),
            granted,
        }
    }

    /// Record a refusal from `peer`; true if it should be shown to the
    /// user (first refusal, or the first after
    /// [`REFUSAL_REPORT_INTERVAL`]).
    pub fn note_refusal(&self, peer: IpAddr) -> bool {
        let peer = canonical_ip(peer);
        let now = Instant::now();
        let mut map = self.refusals.lock().unwrap();
        // Keep the map from growing without bound on a hostile LAN.
        map.retain(|_, t| now.duration_since(*t) < REFUSAL_REPORT_INTERVAL * 10);
        match map.get(&peer) {
            Some(t) if now.duration_since(*t) < REFUSAL_REPORT_INTERVAL => false,
            _ => {
                map.insert(peer, now);
                true
            }
        }
    }
}

/// Fold an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) back to plain
/// IPv4 so comparisons work regardless of what family the listening
/// socket handed us the peer in. Discovery records speakers as IPv4, but
/// a dual-stack bind can report v4 peers mapped.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        IpAddr::V4(_) => ip,
    }
}

/// Does this host own `ip`? Binding a socket to an address succeeds only
/// for addresses assigned to a local interface, on every OS we run on —
/// no interface-enumeration crate needed. Used to decide whether the
/// user-supplied `--advertise-ip` may be admitted as "this machine": an
/// advertise IP pointing at a proxy or NAT box must not open the gate to
/// everything behind it.
pub fn is_local_address(ip: IpAddr) -> bool {
    UdpSocket::bind((ip, 0)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A gate with streaming enabled; the tests that need the flag keep
    /// their own handle on it.
    fn gate(privacy: bool, local: Option<IpAddr>) -> Arc<StreamGate> {
        StreamGate::new(privacy, Arc::new(AtomicBool::new(true)), local)
    }

    #[test]
    fn is_granted_ignores_privacy_setting() {
        let g = gate(false, None);
        assert!(!g.is_granted(ip("192.168.1.50")));
        let grant = g.grant([ip("192.168.1.50")]);
        assert!(g.is_granted(ip("::ffff:192.168.1.50")));
        drop(grant);
        assert!(!g.is_granted(ip("192.168.1.50")));
    }

    #[test]
    fn privacy_off_allows_everyone() {
        let g = gate(false, None);
        assert!(g.allows(ip("10.0.0.9")));
        assert!(g.snapshot().allows(ip("10.0.0.9")));
    }

    #[test]
    fn privacy_on_admits_loopback_and_local_only() {
        let g = gate(true, Some(ip("192.168.1.10")));
        assert!(g.allows(ip("127.0.0.1")));
        assert!(g.allows(ip("::1")));
        assert!(g.allows(ip("192.168.1.10")));
        // Dual-stack sockets report v4 peers as IPv4-mapped IPv6.
        assert!(g.allows(ip("::ffff:192.168.1.10")));
        assert!(!g.allows(ip("192.168.1.99")));
    }

    #[test]
    fn grant_admits_until_dropped() {
        let g = gate(true, None);
        let grant = g.grant([ip("192.168.1.50"), ip("192.168.1.51"), ip("192.168.1.50")]);
        assert_eq!(grant.ips(), &[ip("192.168.1.50"), ip("192.168.1.51")]);
        assert!(g.allows(ip("192.168.1.50")));
        assert!(g.allows(ip("::ffff:192.168.1.51")));
        assert!(!g.allows(ip("192.168.1.52")));
        drop(grant);
        assert!(!g.allows(ip("192.168.1.50")));
        assert!(!g.allows(ip("192.168.1.51")));
    }

    #[test]
    fn overlapping_grants_are_independent() {
        // The race the old single slot had: two bring-ups at once.
        let g = gate(true, None);
        let a = g.grant([ip("192.168.1.50")]);
        let b = g.grant([ip("192.168.1.60"), ip("192.168.1.50")]);
        drop(a);
        // b's addresses survive a's revocation, including the shared one.
        assert!(g.allows(ip("192.168.1.60")));
        assert!(g.allows(ip("192.168.1.50")));
        drop(b);
        assert!(!g.allows(ip("192.168.1.50")));
        assert!(!g.allows(ip("192.168.1.60")));
    }

    #[test]
    fn snapshot_is_a_point_in_time_predicate() {
        let g = gate(true, None);
        let before = g.snapshot();
        let _grant = g.grant([ip("192.168.1.50")]);
        let after = g.snapshot();
        assert!(!before.allows(ip("192.168.1.50")));
        assert!(after.allows(ip("192.168.1.50")));
        assert!(after.allows(ip("127.0.0.1")));
        assert!(!after.allows(ip("192.168.1.99")));
        assert!(after.privacy());
    }

    #[test]
    fn with_streaming_off_only_this_pc_may_listen_in_both_modes() {
        for privacy in [false, true] {
            let streaming = Arc::new(AtomicBool::new(false));
            let g = StreamGate::new(privacy, streaming.clone(), Some(ip("192.168.1.10")));
            let _grant = g.grant([ip("192.168.1.50")]);
            let snap = g.snapshot();
            assert!(!snap.streaming());
            for this_pc in ["127.0.0.1", "::1", "192.168.1.10", "::ffff:192.168.1.10"] {
                assert_eq!(g.check(ip(this_pc)), Ok(()), "{this_pc}, privacy {privacy}");
                assert!(snap.allows(ip(this_pc)), "{this_pc}, privacy {privacy}");
            }
            // Strangers, and the speaker the session was granted for.
            for other in ["192.168.1.99", "192.168.1.50", "::ffff:192.168.1.50"] {
                assert_eq!(g.check(ip(other)), Err(Refusal::StreamingOff), "{other}, privacy {privacy}");
                assert!(!snap.allows(ip(other)), "{other}, privacy {privacy}");
            }
        }
    }

    #[test]
    fn with_streaming_on_privacy_decides() {
        let off = gate(false, None);
        assert_eq!(off.check(ip("192.168.1.99")), Ok(()));
        let on = gate(true, None);
        let _grant = on.grant([ip("192.168.1.50")]);
        assert_eq!(on.check(ip("192.168.1.50")), Ok(()));
        assert_eq!(on.check(ip("192.168.1.99")), Err(Refusal::NotGranted));
        assert!(on.snapshot().streaming());
    }

    #[test]
    fn the_gate_follows_the_apps_streaming_flag_itself() {
        // The flag the app flips, not a copy taken at construction.
        let streaming = Arc::new(AtomicBool::new(true));
        let g = StreamGate::new(false, streaming.clone(), None);
        assert!(g.allows(ip("192.168.1.99")));
        streaming.store(false, Ordering::Release);
        assert!(!g.allows(ip("192.168.1.99")));
        assert!(!g.snapshot().allows(ip("192.168.1.99")));
        streaming.store(true, Ordering::Release);
        assert!(g.allows(ip("192.168.1.99")));
    }

    #[test]
    fn refusals_are_rate_limited_per_peer() {
        let g = gate(true, None);
        assert!(g.note_refusal(ip("192.168.1.99")));
        assert!(!g.note_refusal(ip("192.168.1.99")));
        assert!(!g.note_refusal(ip("::ffff:192.168.1.99")));
        assert!(g.note_refusal(ip("192.168.1.98")));
    }

    #[test]
    fn canonical_ip_folds_mapped_v6_only() {
        assert_eq!(ip("192.168.1.2"), canonical_ip(ip("::ffff:192.168.1.2")));
        assert_eq!(ip("192.168.1.2"), canonical_ip(ip("192.168.1.2")));
        assert_eq!(ip("fe80::1"), canonical_ip(ip("fe80::1")));
    }

    #[test]
    fn local_address_check_is_bind_based() {
        assert!(is_local_address(ip("127.0.0.1")));
        // TEST-NET-3 is never assigned to a real interface.
        assert!(!is_local_address(ip("203.0.113.7")));
    }
}
