//! Realtime (type 96) media scheduler for the HomePod profile: one RTP
//! timeline driving one or more receivers (a single HomePod, or every
//! member of a stereo pair).
//!
//! All members share the initial RTP timestamp, the wall-clock schedule
//! and the audio itself; each has its own sequence number, SSRC, audio key
//! (from its own pairing) and so its own ciphertext. The pair splits
//! left/right itself, so every member gets identical PCM.
//!
//! Per slot (352 frames): a sync packet first when one is due (every
//! 100 ms, and before the very first audio packet), then the new packet to
//! **every** member, and only then each member's retransmit queue. If the
//! sender falls ≥ 60 ms behind (scheduler stall), the expired slots are
//! skipped on every member alike — timestamps and sequence numbers advance
//! by the same count, stale capture audio is dropped and a sync is sent
//! straight away — instead of bursting late audio.

use byteorder::{BigEndian, ByteOrder};
use crossbeam_channel::{Receiver, TryRecvError};
use log::{debug, info, warn};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::airplay::alac::build_uncompressed_alac_frame;
use crate::airplay::ap2_crypto::AudioSealer;
use crate::airplay::ap2_health::{log_notice, Ap2Fault, FaultSlot, SendAction, SendHealth};
use crate::airplay::ap2_ptp::PtpMasterClock;
use crate::airplay::ap2_resend::Retransmitter;
use crate::airplay::ap2_rtsp::RealtimeCodec;
use crate::airplay::rtp::FRAMES_PER_PACKET;
use crate::airplay::timing::build_ptp_sync_homepod;
use crate::http_server::PcmFrame;
use crate::WIRE_SAMPLE_RATE;

/// Sync packet cadence.
pub const SYNC_INTERVAL: Duration = Duration::from_millis(100);
/// Falling this far behind a slot skips the expired slots.
pub const LATE_SKIP_THRESHOLD: Duration = Duration::from_millis(60);

/// Wall-clock duration of one 352-frame packet.
pub fn packet_duration() -> Duration {
    Duration::from_nanos(FRAMES_PER_PACKET as u64 * 1_000_000_000 / WIRE_SAMPLE_RATE as u64)
}

/// Slots to skip when `behind` the current slot's deadline: none under
/// [`LATE_SKIP_THRESHOLD`], else every whole slot already expired, so the
/// next deadline lands within one packet of now.
pub fn slots_to_skip(behind: Duration, packet: Duration) -> u64 {
    if behind < LATE_SKIP_THRESHOLD || packet.is_zero() {
        return 0;
    }
    (behind.as_nanos() / packet.as_nanos()) as u64
}

/// Big-endian interleaved 16-bit PCM payload (1408 bytes per packet).
pub fn pcm_be_payload(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_be_bytes());
    }
    out
}

/// 12-byte RTP header: V=2, PT 0x60, marker on the stream's first packet.
pub fn rtp_header(seq: u16, timestamp: u32, ssrc: u32, first: bool) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = if first { 0xE0 } else { 0x60 };
    BigEndian::write_u16(&mut h[2..4], seq);
    BigEndian::write_u32(&mut h[4..8], timestamp);
    BigEndian::write_u32(&mut h[8..12], ssrc);
    h
}

/// One receiver's end of the shared timeline.
pub struct MemberLink {
    pub name: String,
    pub audio_socket: UdpSocket,
    pub data_addr: SocketAddr,
    /// Non-blocking; sends sync, reads resend requests, sends resends.
    pub control_socket: UdpSocket,
    pub control_addr: SocketAddr,
    pub sealer: AudioSealer,
    pub seq: u16,
    pub ssrc: u32,
    /// Effective receiver latency in samples (reported, else requested).
    pub latency_samples: u32,
    pub retransmit: Retransmitter,
    pub health: SendHealth,
    pub packets: u64,
}

impl MemberLink {
    fn latency(&self) -> Duration {
        Duration::from_nanos(self.latency_samples as u64 * 1_000_000_000 / WIRE_SAMPLE_RATE as u64)
    }
}

/// The shared timeline and its members.
pub struct RealtimeGroup {
    pub members: Vec<MemberLink>,
    /// RTP timestamp of the next packet.
    pub rtptime: u32,
    clock: Arc<PtpMasterClock>,
    clock_id: u64,
    first_packet: bool,
    first_sync: bool,
    last_sync: Option<Instant>,
}

impl RealtimeGroup {
    pub fn new(members: Vec<MemberLink>, initial_rtptime: u32, clock: Arc<PtpMasterClock>, clock_id: u64) -> Self {
        Self {
            members,
            rtptime: initial_rtptime,
            clock,
            clock_id,
            first_packet: true,
            first_sync: true,
            last_sync: None,
        }
    }

    pub fn sync_due(&self, now: Instant) -> bool {
        self.last_sync.map_or(true, |t| now.saturating_duration_since(t) >= SYNC_INTERVAL)
    }

    /// Send a sync packet to every member's control port, mapping the
    /// next packet's RTP timestamp to our clock's "now".
    pub fn send_sync(&mut self, now: Instant) {
        let ptp_now = self.clock.now_ns();
        for m in &self.members {
            let pkt = build_ptp_sync_homepod(self.first_sync, self.rtptime, m.latency_samples, ptp_now, self.clock_id);
            if let Err(e) = m.control_socket.send_to(&pkt, m.control_addr) {
                debug!("AirPlay 2 {}: sync send failed: {}", m.name, e);
            }
        }
        self.first_sync = false;
        self.last_sync = Some(now);
    }

    /// Send one slot: the same payload, sealed per member, to every member,
    /// then service each member's retransmit queue. `slot` is the packet's
    /// scheduled time (its playout deadline is `slot` + latency). Returns
    /// a fault when a member's sends have failed past the grace period.
    pub fn send_slot(&mut self, payload: &[u8], slot: Instant) -> Result<(), Ap2Fault> {
        let first = std::mem::replace(&mut self.first_packet, false);
        let rtptime = self.rtptime;
        for m in &mut self.members {
            let header = rtp_header(m.seq, rtptime, m.ssrc, first);
            let mut packet = Vec::with_capacity(12 + payload.len() + 24);
            packet.extend_from_slice(&header);
            packet.extend_from_slice(&m.sealer.seal(&header, payload));
            let action = match m.audio_socket.send_to(&packet, m.data_addr) {
                Ok(_) => m.health.on_ok(Instant::now()),
                Err(e) => {
                    debug!("AirPlay 2 {}: RTP send failed: {}", m.name, e);
                    m.health.on_err(Instant::now())
                }
            };
            match action {
                SendAction::None => {}
                SendAction::Delayed => log_notice(&m.name, "audio send failing", false),
                SendAction::Recovered => log_notice(&m.name, "audio send", true),
                SendAction::Fault(mut f) => {
                    f.message = format!("{}: {}", m.name, f.message);
                    return Err(f);
                }
            }
            let latency = m.latency();
            m.retransmit.history.record(m.seq, packet.into(), slot, latency);
            m.seq = m.seq.wrapping_add(1);
            m.packets += 1;
        }
        let now = Instant::now();
        for m in &mut self.members {
            m.retransmit.service(&m.control_socket, now);
        }
        self.rtptime = rtptime.wrapping_add(FRAMES_PER_PACKET as u32);
        Ok(())
    }

    /// Skip `slots` expired slots on every member alike. Each member's
    /// nonce counter jumps with its sequence number, so the nonce suffix
    /// keeps tracking it (and still never repeats).
    pub fn skip(&mut self, slots: u64) {
        let n = slots as u32;
        self.rtptime = self.rtptime.wrapping_add(n.wrapping_mul(FRAMES_PER_PACKET as u32));
        for m in &mut self.members {
            m.seq = m.seq.wrapping_add(n as u16);
            m.sealer.advance(n as u64);
        }
    }
}

/// Everything the media thread owns.
pub struct RealtimeRun {
    pub group: RealtimeGroup,
    pub codec: RealtimeCodec,
    pub samples_rx: Receiver<PcmFrame>,
    pub stop_flag: Arc<AtomicBool>,
    pub faults: FaultSlot,
    pub name: String,
}

pub fn spawn(run: RealtimeRun) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("stream-to-speaker-ap2-realtime:{}", run.name))
        .spawn(move || run_realtime(run))
}

fn append_samples(ring: &mut Vec<i16>, frame: &PcmFrame) {
    let bytes = &**frame.0;
    ring.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
}

fn run_realtime(mut run: RealtimeRun) {
    let stereo = FRAMES_PER_PACKET * 2;
    let packet = packet_duration();
    let mut ring: Vec<i16> = Vec::with_capacity(stereo * 4);
    let mut slot: u64 = 0;
    let mut silence_packets: u64 = 0;
    let mut late_recoveries: u64 = 0;
    let mut got_real_audio = false;
    let mut idle_warned = false;
    let mut disconnected = false;

    // Drop audio queued during the handshake — paced sending never drains
    // a backlog, so it would be permanent latency.
    while run.samples_rx.try_recv().is_ok() {}

    let start = Instant::now();
    // Anchor before audio.
    run.group.send_sync(start);
    info!(
        "AirPlay 2 realtime {} stream to {} member(s) started (rtptime {})",
        run.codec.label(),
        run.group.members.len(),
        run.group.rtptime
    );

    while !run.stop_flag.load(Ordering::Acquire) {
        let mut deadline = start + packet.saturating_mul(slot as u32);

        // Scheduler stall: skip the expired slots on every member.
        let behind = Instant::now().saturating_duration_since(deadline);
        let skip = slots_to_skip(behind, packet);
        if skip > 0 {
            run.group.skip(skip);
            slot += skip;
            deadline = start + packet.saturating_mul(slot as u32);
            ring.clear();
            while run.samples_rx.try_recv().is_ok() {}
            late_recoveries += 1;
            warn!(
                "AirPlay 2 {}: sender_late_recovered — {:.0} ms behind, skipped {} slot(s) on every member",
                run.name,
                behind.as_secs_f64() * 1000.0,
                skip
            );
            run.group.send_sync(Instant::now());
        }

        // Gather a packet's worth of audio, or give up at the deadline and
        // fill silence so the timeline stays glued to wall-clock.
        loop {
            loop {
                match run.samples_rx.try_recv() {
                    Ok(f) => append_samples(&mut ring, &f),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected || ring.len() >= stereo {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match run.samples_rx.recv_timeout((deadline - now).min(Duration::from_millis(2))) {
                Ok(f) => append_samples(&mut ring, &f),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => disconnected = true,
            }
            if run.stop_flag.load(Ordering::Acquire) {
                break;
            }
        }
        if run.stop_flag.load(Ordering::Acquire) || (disconnected && ring.len() < stereo) {
            break;
        }
        if !got_real_audio && !idle_warned && start.elapsed() > Duration::from_secs(3) {
            warn!(
                "AirPlay 2: no audio from the source after 3s — is something playing with \
                 Stream To Speaker selected as the Windows output device? (streaming silence \
                 to keep the timeline anchored)"
            );
            idle_warned = true;
        }
        // Bound the capture backlog (~32 ms) so underrun-inserted silence
        // can't make latency creep.
        if ring.len() > stereo * 4 {
            let drop = ring.len() - stereo * 2;
            ring.drain(..drop);
        }
        let samples: Vec<i16> = if ring.len() >= stereo {
            got_real_audio = true;
            ring.drain(..stereo).collect()
        } else {
            silence_packets += 1;
            vec![0i16; stereo]
        };
        let payload = match run.codec {
            RealtimeCodec::Pcm => pcm_be_payload(&samples),
            RealtimeCodec::Alac => build_uncompressed_alac_frame(&samples),
        };

        // Pace to the slot in short steps.
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
        }
        let now = Instant::now();
        if run.group.sync_due(now) {
            run.group.send_sync(now);
        }
        if let Err(f) = run.group.send_slot(&payload, deadline) {
            if !run.stop_flag.load(Ordering::Acquire) {
                run.faults.raise(f);
            }
            break;
        }
        slot += 1;
    }
    for m in &run.group.members {
        info!(
            "AirPlay 2 {}: realtime sender stopped — {} packets, {} send errors",
            m.name, m.packets, m.health.errors
        );
    }
    info!(
        "AirPlay 2 {}: {} silence-filled packets, {} late recoveries",
        run.name, silence_packets, late_recoveries
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::timing::ResendStats;

    fn member(name: &str, seq: u16) -> (MemberLink, UdpSocket, UdpSocket) {
        let data_rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let ctrl_rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let control_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        control_socket.set_nonblocking(true).unwrap();
        let link = MemberLink {
            name: name.into(),
            audio_socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            data_addr: data_rx.local_addr().unwrap(),
            control_socket,
            control_addr: ctrl_rx.local_addr().unwrap(),
            sealer: AudioSealer::new(&[name.len() as u8; 32], seq),
            seq,
            ssrc: seq as u32 * 7,
            latency_samples: 8820,
            retransmit: Retransmitter::new(
                "127.0.0.1".parse().unwrap(),
                Arc::new(ResendStats::default()),
                name.into(),
            ),
            health: SendHealth::new(Instant::now()),
            packets: 0,
        };
        (link, data_rx, ctrl_rx)
    }

    fn recv(sock: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let n = sock.recv(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn skip_math() {
        let p = packet_duration();
        assert_eq!(slots_to_skip(Duration::from_millis(59), p), 0);
        for stall_ms in [80u64, 200, 500] {
            let behind = Duration::from_millis(stall_ms);
            let k = slots_to_skip(behind, p);
            // After skipping, the next deadline is within 8 ms of now.
            let next_deadline_lag = behind.saturating_sub(p.saturating_mul(k as u32));
            assert!(next_deadline_lag < p, "{stall_ms} ms → {k} slots");
            assert!(p.as_millis() <= 8);
        }
    }

    #[test]
    fn pcm_payload_is_big_endian_1408_bytes() {
        let samples: Vec<i16> = (0..704).map(|i| i as i16 - 300).collect();
        let p = pcm_be_payload(&samples);
        assert_eq!(p.len(), 1408);
        assert_eq!(&p[..2], &(-300i16).to_be_bytes());
        // On the wire: 12 header + 1408 + 16 tag + 8 nonce.
        let mut s = AudioSealer::new(&[1; 32], 0);
        assert_eq!(12 + s.seal(&rtp_header(0, 0, 0, true), &p).len(), 1444);
    }

    /// Two members: identical timestamps, independent seq/SSRC/ciphertext;
    /// a stall skips both by the same count, nonce counters included (the
    /// suffix keeps tracking the sequence number); sync precedes audio and
    /// maps the unshifted next timestamp.
    #[test]
    fn two_members_share_timeline_and_skip_together() {
        let (a, a_data, a_ctrl) = member("left", 65_534);
        let (b, b_data, b_ctrl) = member("right-member", 100);
        let mut g = RealtimeGroup::new(vec![a, b], 0xFFFF_FE00, PtpMasterClock::new(), 0x1234);
        let now = Instant::now();
        g.send_sync(now);
        let payload = vec![0x11u8; 1408];
        g.send_slot(&payload, now).unwrap();
        let skip = slots_to_skip(Duration::from_millis(200), packet_duration());
        g.skip(skip);
        g.send_sync(now);
        g.send_slot(&payload, now).unwrap();

        for (ctrl, data, seq0) in [(&a_ctrl, &a_data, 65_534u16), (&b_ctrl, &b_data, 100)] {
            let s1 = recv(ctrl);
            assert_eq!((s1[0], s1[1]), (0x90, 0xD7));
            assert_eq!(u32::from_be_bytes(s1[4..8].try_into().unwrap()), 0xFFFF_FE00);
            let p1 = recv(data);
            assert_eq!(p1[1], 0xE0);
            assert_eq!(u16::from_be_bytes([p1[2], p1[3]]), seq0);
            assert_eq!(u32::from_be_bytes(p1[4..8].try_into().unwrap()), 0xFFFF_FE00);
            assert_eq!(&p1[p1.len() - 8..], &(seq0 as u64).to_le_bytes());
            let s2 = recv(ctrl);
            assert_eq!(s2[0], 0x80);
            let ts2 = 0xFFFF_FE00u32.wrapping_add((1 + skip as u32) * 352);
            assert_eq!(u32::from_be_bytes(s2[4..8].try_into().unwrap()), ts2);
            let p2 = recv(data);
            assert_eq!(p2[1], 0x60);
            assert_eq!(u16::from_be_bytes([p2[2], p2[3]]), seq0.wrapping_add(1 + skip as u16));
            assert_eq!(u32::from_be_bytes(p2[4..8].try_into().unwrap()), ts2);
            assert_eq!(&p2[p2.len() - 8..], &(seq0 as u64 + 1 + skip).to_le_bytes());
        }
        assert_ne!(g.members[0].ssrc, g.members[1].ssrc);
    }

    /// A resend request on a member's control socket is answered with the
    /// original bytes after that slot's new audio, without moving the live
    /// sequence number.
    #[test]
    fn resend_is_served_after_new_audio_with_original_bytes() {
        let (a, a_data, a_ctrl) = member("solo", 10);
        let mut g = RealtimeGroup::new(vec![a], 1000, PtpMasterClock::new(), 1);
        let now = Instant::now();
        g.send_slot(&[1u8; 8], now).unwrap();
        let original = recv(&a_data);
        // Receiver asks for seq 10 from its control socket.
        let mut req = [0x80, 0xD5, 0, 1, 0, 0, 0, 1];
        req[4..6].copy_from_slice(&10u16.to_be_bytes());
        a_ctrl
            .send_to(&req, g.members[0].control_socket.local_addr().unwrap())
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        g.send_slot(&[2u8; 8], now).unwrap();
        let live = recv(&a_data);
        assert_eq!(u16::from_be_bytes([live[2], live[3]]), 11);
        let resent = recv(&a_ctrl);
        assert_eq!(&resent[..2], &[0x80, 0xD6]);
        assert_eq!(&resent[4..], &original[..]);
        assert_eq!(g.members[0].seq, 12);
    }
}
