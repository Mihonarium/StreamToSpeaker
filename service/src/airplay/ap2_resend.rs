//! Deadline-aware retransmission for the AirPlay 2 realtime stream.
//!
//! The receiver asks for lost packets on our control socket (`0x80 0xD5`:
//! bytes 4..6 first seq, 6..8 count). We answer with `0x80 0xD6 <seq>`
//! followed by **the exact original on-wire packet** — same ciphertext,
//! same nonce suffix — never a re-encryption.
//!
//! Unlike the RAOP responder thread ([`crate::airplay::timing::spawn_resend_responder`])
//! this runs inside the media loop, after each slot's new audio has gone
//! out, under a per-slot budget (time, requests read, packets resent) and
//! a bounded pending queue. A request flood therefore can never delay new
//! audio, and a packet is only resent while it can still be played: after
//! its playout deadline (slot time + receiver latency) a resend is
//! pointless and is counted as expired instead.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use byteorder::{BigEndian, ByteOrder};
use log::{debug, info};

use crate::airplay::timing::ResendStats;

/// Packets kept per member for retransmission.
pub const HISTORY_PACKETS: usize = 512;
/// Shortest retention, whatever the latency.
const MIN_RETENTION: Duration = Duration::from_secs(1);
/// Retention beyond the receiver latency.
const RETENTION_SLACK: Duration = Duration::from_millis(250);

/// Per-slot servicing limits.
#[derive(Debug, Clone, Copy)]
pub struct ResendBudget {
    pub time: Duration,
    pub requests: usize,
    pub packets: usize,
    pub pending_cap: usize,
}

impl Default for ResendBudget {
    fn default() -> Self {
        Self { time: Duration::from_millis(1), requests: 16, packets: 32, pending_cap: 512 }
    }
}

struct Entry {
    seq: u16,
    expires: Instant,
    bytes: Arc<[u8]>,
}

/// Result of looking a sequence number up in the history.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Hit(Arc<[u8]>),
    /// Known but past its playout deadline.
    Expired,
    /// Never sent, or already evicted.
    Missing,
}

/// Ring of recently-sent packets with their resend deadlines.
pub struct History {
    entries: VecDeque<Entry>,
    cap: usize,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self { entries: VecDeque::with_capacity(cap), cap }
    }

    /// Record a sent packet. `slot` is when it was due; it stays
    /// resendable until `slot + latency` (its playout deadline), and never
    /// beyond `max(1 s, latency + 250 ms)` after sending.
    pub fn record(&mut self, seq: u16, bytes: Arc<[u8]>, slot: Instant, latency: Duration) {
        if self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        let playout = slot + latency;
        let retention = slot + MIN_RETENTION.max(latency + RETENTION_SLACK);
        self.entries.push_back(Entry { seq, expires: playout.min(retention), bytes });
    }

    pub fn lookup(&self, seq: u16, now: Instant) -> Lookup {
        match self.entries.iter().rev().find(|e| e.seq == seq) {
            Some(e) if now < e.expires => Lookup::Hit(e.bytes.clone()),
            Some(_) => Lookup::Expired,
            None => Lookup::Missing,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Parse a resend request: `(first seq, count)`. Payload type is matched
/// with the marker bit masked off (`0x55`).
pub fn parse_request(buf: &[u8]) -> Option<(u16, u16)> {
    if buf.len() < 8 || buf[1] & 0x7F != 0x55 {
        return None;
    }
    Some((BigEndian::read_u16(&buf[4..6]), BigEndian::read_u16(&buf[6..8])))
}

/// `0x80 0xD6 <seq BE>` + the original packet.
pub fn build_response(seq: u16, original: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + original.len());
    out.extend_from_slice(&[0x80, 0xD6]);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(original);
    out
}

/// Per-member retransmit state, owned by the media loop.
pub struct Retransmitter {
    member_ip: IpAddr,
    pub history: History,
    pending: VecDeque<(u16, SocketAddr)>,
    budget: ResendBudget,
    stats: Arc<ResendStats>,
    name: String,
}

impl Retransmitter {
    pub fn new(member_ip: IpAddr, stats: Arc<ResendStats>, name: String) -> Self {
        Self {
            member_ip,
            history: History::new(HISTORY_PACKETS),
            pending: VecDeque::new(),
            budget: ResendBudget::default(),
            stats,
            name,
        }
    }

    #[cfg(test)]
    fn with_budget(mut self, budget: ResendBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Queue one inbound datagram if it is a resend request from this
    /// member. Returns whether it was accepted.
    pub fn intake(&mut self, buf: &[u8], from: SocketAddr) -> bool {
        if from.ip() != self.member_ip {
            return false;
        }
        let Some((first, count)) = parse_request(buf) else { return false };
        let total = self.stats.requests.fetch_add(1, Ordering::Relaxed) + 1;
        if total == 1 {
            info!("AirPlay 2 {}: receiver is requesting retransmissions (first request)", self.name);
        }
        self.stats.requested.fetch_add(count as u64, Ordering::Relaxed);
        for i in 0..count {
            if self.pending.len() >= self.budget.pending_cap {
                self.stats.queue_drops.fetch_add((count - i) as u64, Ordering::Relaxed);
                break;
            }
            self.pending.push_back((first.wrapping_add(i), from));
        }
        true
    }

    /// One slot's worth of servicing: read up to the request budget from
    /// `socket` (non-blocking, or with a very short read timeout), then resend within the packet and time
    /// budgets. Call only after the slot's new audio has been sent.
    pub fn service(&mut self, socket: &UdpSocket, now: Instant) {
        let mut buf = [0u8; 64];
        for _ in 0..self.budget.requests {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    self.intake(&buf[..n], from);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                // Other errors (e.g. Windows reporting an earlier ICMP
                // port-unreachable) consume one read; nothing to act on.
                Err(_) => continue,
            }
        }
        self.drain(now, |pkt, to| socket.send_to(pkt, to).map(|_| ()));
    }

    /// Resend pending packets within budget via `send`.
    pub fn drain<F>(&mut self, now: Instant, mut send: F)
    where
        F: FnMut(&[u8], SocketAddr) -> std::io::Result<()>,
    {
        let started = Instant::now();
        let mut sent = 0usize;
        let mut processed = 0usize;
        while sent < self.budget.packets {
            if processed > 0 && started.elapsed() >= self.budget.time {
                break;
            }
            processed += 1;
            let Some((seq, to)) = self.pending.pop_front() else { break };
            match self.history.lookup(seq, now) {
                Lookup::Hit(bytes) => {
                    sent += 1;
                    match send(&build_response(seq, &bytes), to) {
                        Ok(()) => {
                            self.stats.packets.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                            debug!("AirPlay 2 {}: resend of seq {} failed: {}", self.name, seq, e);
                        }
                    }
                }
                Lookup::Expired => {
                    self.stats.expired.fetch_add(1, Ordering::Relaxed);
                }
                Lookup::Missing => {
                    self.stats.missing.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn member() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 6001)
    }

    fn request(first: u16, count: u16) -> [u8; 8] {
        let mut r = [0x80, 0xD5, 0, 1, 0, 0, 0, 0];
        r[4..6].copy_from_slice(&first.to_be_bytes());
        r[6..8].copy_from_slice(&count.to_be_bytes());
        r
    }

    fn packet(seq: u16) -> Arc<[u8]> {
        let mut p = vec![0x80, 0x60];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&[0xAB; 20]);
        p.into()
    }

    fn rt() -> Retransmitter {
        Retransmitter::new(member().ip(), Arc::new(ResendStats::default()), "test".into())
    }

    #[test]
    fn exact_bytes_across_seq_wrap_and_history_cap() {
        let now = Instant::now();
        let mut r = rt();
        let lat = Duration::from_millis(200);
        // 600 packets ending past the wrap: only the last 512 are kept.
        let mut seq: u16 = 65_535u16.wrapping_sub(299);
        for _ in 0..600 {
            r.history.record(seq, packet(seq), now, lat);
            seq = seq.wrapping_add(1);
        }
        assert_eq!(r.history.len(), HISTORY_PACKETS);
        r.intake(&request(65_534, 4), member());
        let mut out = Vec::new();
        r.drain(now, |p, _| {
            out.push(p.to_vec());
            Ok(())
        });
        assert_eq!(out.len(), 4);
        for (i, want) in [65_534u16, 65_535, 0, 1].iter().enumerate() {
            assert_eq!(&out[i][..2], &[0x80, 0xD6]);
            assert_eq!(&out[i][2..4], &want.to_be_bytes());
            assert_eq!(&out[i][4..], &packet(*want)[..]);
        }
        // The oldest 88 were evicted.
        assert_eq!(r.history.lookup(65_535u16.wrapping_sub(299), now), Lookup::Missing);
    }

    #[test]
    fn resendable_until_playout_deadline() {
        let slot = Instant::now();
        let mut h = History::new(8);
        let lat = Duration::from_millis(120);
        h.record(7, packet(7), slot, lat);
        assert!(matches!(h.lookup(7, slot + lat - Duration::from_millis(1)), Lookup::Hit(_)));
        assert_eq!(h.lookup(7, slot + lat), Lookup::Expired);
    }

    #[test]
    fn flood_is_bounded_per_slot() {
        let now = Instant::now();
        let stats = Arc::new(ResendStats::default());
        let mut r = Retransmitter::new(member().ip(), stats.clone(), "t".into())
            .with_budget(ResendBudget { time: Duration::from_secs(1), ..ResendBudget::default() });
        let lat = Duration::from_millis(500);
        let mut seq: u16 = 65_500;
        for _ in 0..100 {
            r.history.record(seq, packet(seq), now, lat);
            seq = seq.wrapping_add(1);
        }
        // 64 single-packet requests across the wrap, mostly unknown seqs,
        // plus one oversized run that overflows the queue.
        for i in 0..64u16 {
            let first = if i % 4 == 0 { 65_530u16.wrapping_add(i) } else { 20_000 + i };
            r.intake(&request(first, 1), member());
        }
        r.intake(&request(30_000, 1000), member());
        assert_eq!(r.pending_len(), 512);
        assert_eq!(stats.queue_drops.load(Ordering::Relaxed), 64 + 1000 - 512);
        let mut resent = 0;
        r.drain(now, |_, _| {
            resent += 1;
            Ok(())
        });
        assert!(resent <= 32);
        assert_eq!(resent, 16); // the 16 known seqs among the first 64
        assert_eq!(stats.requests.load(Ordering::Relaxed), 65);
    }

    #[test]
    fn requests_from_other_hosts_are_ignored() {
        let mut r = rt();
        let stranger = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 6001);
        assert!(!r.intake(&request(1, 1), stranger));
        assert!(!r.intake(&[0x80, 0xD4, 0, 0, 0, 0, 0, 0], member()));
        assert!(r.intake(&request(1, 1), member()));
    }

    #[test]
    fn service_reads_at_most_the_request_budget_per_slot() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_nonblocking(true).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut r = Retransmitter::new(tx.local_addr().unwrap().ip(), Arc::new(ResendStats::default()), "t".into());
        for i in 0..40u16 {
            tx.send_to(&request(i, 1), rx.local_addr().unwrap()).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        let stats = r.stats.clone();
        r.service(&rx, Instant::now());
        assert_eq!(stats.requests.load(Ordering::Relaxed), 16);
        r.service(&rx, Instant::now());
        assert_eq!(stats.requests.load(Ordering::Relaxed), 32);
    }
}
