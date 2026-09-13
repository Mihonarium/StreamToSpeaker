//! Privacy-mode gate for `/stream.raw`: who may pull the audio stream.
//!
//! With privacy mode off everyone is admitted. With it on, a peer is
//! admitted if it is loopback, one of this machine's own addresses, or
//! covered by a live **grant**. A grant is a set of addresses handed to
//! a speaker for as long as its session lasts: [`StreamGate::grant`]
//! returns a [`Grant`] that the session owns, and dropping the session
//! drops the grant, which revokes the addresses. That gives three
//! properties the earlier single "pending IP" slot did not:
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

/// A point-in-time copy of the allow-set: a pure predicate for pruning.
#[derive(Clone, Debug)]
pub struct AllowSnapshot {
    privacy: bool,
    local: Vec<IpAddr>,
    granted: Vec<IpAddr>,
}

impl AllowSnapshot {
    pub fn privacy(&self) -> bool {
        self.privacy
    }

    pub fn allows(&self, peer: IpAddr) -> bool {
        if !self.privacy {
            return true;
        }
        let peer = canonical_ip(peer);
        peer.is_loopback() || self.local.contains(&peer) || self.granted.contains(&peer)
    }
}

impl StreamGate {
    /// `local`: this host's own advertised address, if it really is
    /// local (see [`is_local_address`]).
    pub fn new(privacy: bool, local: impl IntoIterator<Item = IpAddr>) -> Arc<Self> {
        Arc::new(Self {
            privacy: AtomicBool::new(privacy),
            local: local.into_iter().map(canonical_ip).collect(),
            grants: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            refusals: Mutex::new(HashMap::new()),
        })
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
        if !self.privacy() {
            return true;
        }
        let peer = canonical_ip(peer);
        if peer.is_loopback() || self.local.contains(&peer) {
            return true;
        }
        self.grants
            .lock()
            .unwrap()
            .values()
            .any(|ips| ips.contains(&peer))
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

    #[test]
    fn privacy_off_allows_everyone() {
        let g = StreamGate::new(false, None);
        assert!(g.allows(ip("10.0.0.9")));
        assert!(g.snapshot().allows(ip("10.0.0.9")));
    }

    #[test]
    fn privacy_on_admits_loopback_and_local_only() {
        let g = StreamGate::new(true, Some(ip("192.168.1.10")));
        assert!(g.allows(ip("127.0.0.1")));
        assert!(g.allows(ip("::1")));
        assert!(g.allows(ip("192.168.1.10")));
        // Dual-stack sockets report v4 peers as IPv4-mapped IPv6.
        assert!(g.allows(ip("::ffff:192.168.1.10")));
        assert!(!g.allows(ip("192.168.1.99")));
    }

    #[test]
    fn grant_admits_until_dropped() {
        let g = StreamGate::new(true, None);
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
        let g = StreamGate::new(true, None);
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
        let g = StreamGate::new(true, None);
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
    fn refusals_are_rate_limited_per_peer() {
        let g = StreamGate::new(true, None);
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
