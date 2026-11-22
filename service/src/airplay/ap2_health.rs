//! AirPlay 2 session health: the fault model and the small state machines
//! that decide when a live session is degraded, recovered or dead.
//!
//! Everything here is pure (time is passed in), so the policies are unit
//! tested without sockets:
//!
//! - [`FeedbackMonitor`] — the `POST /feedback` keepalive: one request in
//!   flight at a time, a *soft* timeout that only notifies ("delayed;
//!   audio continues") and keeps waiting on the same request, a *hard*
//!   timeout that faults, and status-code classification.
//! - [`SendHealth`] — UDP media sends: a failed send is counted as loss,
//!   not death; only a sustained run with no successful send faults.
//! - [`VolumeTracker`] — device-volume read-back (`GET /info`
//!   `initialVolume`) against the last commanded value.
//!
//! A fault ends the session. Retryable faults set the session's `dead`
//! flag, which the app's drop watchdog turns into one reconnect;
//! non-retryable ones (authentication, a receiver that rejected the
//! session outright) end it without a retry.

use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Which part of the session raised a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultChannel {
    Control,
    Events,
    Feedback,
    Media,
}

impl FaultChannel {
    pub fn label(self) -> &'static str {
        match self {
            FaultChannel::Control => "control",
            FaultChannel::Events => "events",
            FaultChannel::Feedback => "feedback",
            FaultChannel::Media => "media",
        }
    }
}

/// A session-ending failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ap2Fault {
    /// Stable machine-readable code (`feedback_timeout`, `peer_closed`, …).
    pub code: &'static str,
    pub channel: FaultChannel,
    /// Whether a reconnect may help.
    pub retryable: bool,
    pub message: String,
}

impl Ap2Fault {
    pub fn new(code: &'static str, channel: FaultChannel, retryable: bool, message: impl Into<String>) -> Self {
        Self { code, channel, retryable, message: message.into() }
    }

    /// For a fault that means the session to `name` couldn't play (the app
    /// reconnects after it): the diagnostics' fault line and the window's
    /// notice while reconnecting. None for faults that are a lost
    /// connection, whose text is the app's own.
    pub fn reconnect_text(&self, name: &str) -> Option<(String, String)> {
        match self.code {
            "anchor_failed" => {
                Some((format!("{name} didn't start playing"), format!("{name} didn't start playing — reconnecting…")))
            }
            "encoder_failed" => Some((
                format!("Windows' AAC encoder stopped working ({name})"),
                format!("Windows' AAC encoder stopped working — reconnecting to {name}…"),
            )),
            _ => None,
        }
    }
}

/// The message alone: for a fault that ends the session for good, it is
/// what the window shows after "Connection to <speaker> ended:".
impl std::fmt::Display for Ap2Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// How soon after a fault the next session to that speaker must start to
/// be the reconnect that followed it: the watchdog reconnects about 5 s
/// after a drop; the person picking the speaker again within this time
/// counts too.
pub const REPEAT_WINDOW: Duration = Duration::from_secs(120);

/// Notes older than this are dropped: no reconnect that is still failing
/// can follow them.
const REPEAT_MEMORY_LIMIT: Duration = Duration::from_secs(3600);

/// Recent faults per receiver of the kinds that mean a session cannot play
/// (the encoder, the anchor), for [`FaultSlot::raise_unless_repeated`].
#[derive(Debug, Default)]
pub struct RepeatMemory {
    recent: Vec<(String, &'static str, Instant)>,
}

impl RepeatMemory {
    pub const fn new() -> Self {
        Self { recent: Vec::new() }
    }

    /// Note fault `code`, raised at `now` by a session to `host` that
    /// finished its bring-up at `started` and had or had not `played`.
    /// True when this is the same thing again straight after the
    /// reconnect: the same fault on that host is noted, this session
    /// started within [`REPEAT_WINDOW`] after it (it is the reconnect), and
    /// it failed before it played. That note is then forgotten, so a later
    /// attempt starts over; otherwise this fault replaces it.
    pub fn repeated(&mut self, host: &str, code: &'static str, started: Instant, played: bool, now: Instant) -> bool {
        self.recent.retain(|(_, _, at)| now.saturating_duration_since(*at) < REPEAT_MEMORY_LIMIT);
        let earlier = self.recent.iter().position(|(h, c, _)| h == host && *c == code);
        let repeat = earlier.is_some_and(|i| {
            let at = self.recent[i].2;
            !played && started >= at && started.saturating_duration_since(at) <= REPEAT_WINDOW
        });
        if let Some(i) = earlier {
            self.recent.swap_remove(i);
        }
        if !repeat {
            self.recent.push((host.to_string(), code, now));
        }
        repeat
    }
}

static REPEATS: Mutex<RepeatMemory> = Mutex::new(RepeatMemory::new());

/// First-fault-wins slot shared by a session's threads. Raising a fault
/// also sets the session's `dead` flag so the watchdog notices.
#[derive(Clone)]
pub struct FaultSlot {
    fault: Arc<Mutex<Option<Ap2Fault>>>,
    dead: Arc<AtomicBool>,
    host: String,
    /// When the session finished its bring-up; None until then.
    started: Arc<Mutex<Option<Instant>>>,
}

impl FaultSlot {
    pub fn new(dead: Arc<AtomicBool>, host: String) -> Self {
        Self { fault: Arc::new(Mutex::new(None)), dead, host, started: Arc::new(Mutex::new(None)) }
    }

    /// The session finished its bring-up at `started`: from now on its
    /// faults count for the repeat rule. A bring-up that fails never arms
    /// its slot, so a session that never existed leaves nothing to repeat.
    pub fn arm(&self, started: Instant) {
        *self.started.lock().unwrap() = Some(started);
    }

    pub fn raise(&self, fault: Ap2Fault) {
        let mut slot = self.fault.lock().unwrap();
        if slot.is_none() {
            log_fault(&self.host, &fault);
            *slot = Some(fault);
        }
        self.dead.store(true, Ordering::Release);
    }

    /// Raise a fault that means this session cannot play (the encoder, the
    /// anchor): retryable with `detail`, so the app reconnects; final, with
    /// `final_message` (a sentence for the person: what stopped working,
    /// the likely cause, what to try), when this session is the reconnect
    /// that followed the same fault on this receiver and failed before it
    /// `played` ([`RepeatMemory::repeated`]).
    pub fn raise_unless_repeated(
        &self,
        code: &'static str,
        channel: FaultChannel,
        detail: String,
        played: bool,
        final_message: impl FnOnce() -> String,
    ) {
        let mut slot = self.fault.lock().unwrap();
        if slot.is_none() {
            let started = *self.started.lock().unwrap();
            let repeated = started.is_some_and(|started| {
                REPEATS.lock().unwrap_or_else(|e| e.into_inner()).repeated(&self.host, code, started, played, Instant::now())
            });
            let fault = if repeated {
                warn!(
                    "AirPlay 2 session to {}: {} again straight after reconnecting ({}); not retrying",
                    self.host, code, detail
                );
                Ap2Fault::new(code, channel, false, final_message())
            } else {
                Ap2Fault::new(code, channel, true, detail)
            };
            log_fault(&self.host, &fault);
            *slot = Some(fault);
        }
        self.dead.store(true, Ordering::Release);
    }

    pub fn get(&self) -> Option<Ap2Fault> {
        self.fault.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub(crate) fn armed(&self) -> bool {
        self.started.lock().unwrap().is_some()
    }
}

fn log_fault(host: &str, fault: &Ap2Fault) {
    warn!("AirPlay 2 session to {} failed: {} ({}): {}", host, fault.code, fault.channel.label(), fault.message);
}

// ---------------------------------------------------------------------------
// /feedback
// ---------------------------------------------------------------------------

/// `/feedback` cadence (iOS senders post it every 2 s).
pub const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// No reply by now: notify "delayed", keep waiting on the same request.
pub const FEEDBACK_SOFT_TIMEOUT: Duration = Duration::from_secs(4);
/// No reply by now: the control connection is considered dead.
pub const FEEDBACK_HARD_TIMEOUT: Duration = Duration::from_secs(12);
/// Consecutive failed `/feedback` rounds (error replies, transport errors,
/// or — outside strict mode — unanswered requests) before faulting.
const FEEDBACK_MAX_FAILURES: u32 = 3;

/// What the feedback loop should do after an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackAction {
    /// Nothing to report.
    None,
    /// Soft timeout crossed: the receiver is slow, audio continues.
    Delayed,
    /// A reply arrived after a delay notice (or after failures).
    Recovered,
    Fault(Ap2Fault),
}

/// `/feedback` policy. Feed it `on_sent` when a request goes out,
/// `on_status` when its reply arrives, `on_error` on a transport error
/// and `on_tick` while waiting.
///
/// Every receiver: any non-200 reply or transport error counts as a
/// failure, and [`FEEDBACK_MAX_FAILURES`] in a row is a retryable fault.
/// Strict mode (HomePod sessions) adds: 401/403 end the session for good,
/// 454 is an immediate retryable fault, and no reply by
/// [`FEEDBACK_HARD_TIMEOUT`] is a retryable fault; otherwise an unanswered
/// request just counts as one failure and the next one is sent.
#[derive(Debug, Default)]
pub struct FeedbackMonitor {
    strict: bool,
    sent_at: Option<Instant>,
    delayed: bool,
    consecutive: u32,
    /// Total failed replies or timeouts (diagnostics).
    pub failures: u64,
    /// Round-trip of the last answered request.
    pub last_rtt: Option<Duration>,
}

impl FeedbackMonitor {
    pub fn new(strict: bool) -> Self {
        Self { strict, ..Self::default() }
    }

    pub fn outstanding(&self) -> bool {
        self.sent_at.is_some()
    }

    pub fn on_sent(&mut self, now: Instant) {
        self.sent_at = Some(now);
    }

    fn fail(&mut self, what: String) -> FeedbackAction {
        self.failures += 1;
        self.consecutive += 1;
        if self.consecutive >= FEEDBACK_MAX_FAILURES {
            FeedbackAction::Fault(Ap2Fault::new(
                "feedback_rejected",
                FaultChannel::Feedback,
                true,
                format!("{} consecutive /feedback failures (last: {})", self.consecutive, what),
            ))
        } else {
            FeedbackAction::None
        }
    }

    pub fn on_tick(&mut self, now: Instant) -> FeedbackAction {
        let Some(sent) = self.sent_at else { return FeedbackAction::None };
        let waited = now.saturating_duration_since(sent);
        if waited >= FEEDBACK_HARD_TIMEOUT {
            self.sent_at = None;
            if self.strict {
                self.failures += 1;
                return FeedbackAction::Fault(Ap2Fault::new(
                    "feedback_timeout",
                    FaultChannel::Feedback,
                    true,
                    format!("no /feedback reply for {} s", waited.as_secs()),
                ));
            }
            return self.fail(format!("no reply for {} s", waited.as_secs()));
        }
        if waited >= FEEDBACK_SOFT_TIMEOUT && !self.delayed {
            self.delayed = true;
            return FeedbackAction::Delayed;
        }
        FeedbackAction::None
    }

    /// A transport error on the control connection.
    pub fn on_error(&mut self, what: String) -> FeedbackAction {
        self.sent_at = None;
        self.fail(what)
    }

    pub fn on_status(&mut self, status: u16, now: Instant) -> FeedbackAction {
        if let Some(sent) = self.sent_at.take() {
            self.last_rtt = Some(now.saturating_duration_since(sent));
        }
        match status {
            200 => {
                let was_degraded = self.delayed || self.consecutive > 0;
                self.delayed = false;
                self.consecutive = 0;
                if was_degraded {
                    FeedbackAction::Recovered
                } else {
                    FeedbackAction::None
                }
            }
            401 | 403 if self.strict => FeedbackAction::Fault(Ap2Fault::new(
                "authentication_failed",
                FaultChannel::Feedback,
                false,
                format!("the speaker refused this PC's connection (/feedback → {}). Select it again.", status),
            )),
            454 if self.strict => FeedbackAction::Fault(Ap2Fault::new(
                "session_rejected",
                FaultChannel::Feedback,
                true,
                "/feedback → 454 (session not found)",
            )),
            other => self.fail(format!("status {}", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// Buffered-stream anchor
// ---------------------------------------------------------------------------

/// How long a buffered session keeps re-sending SETRATEANCHORTIME, with
/// audio waiting, after the first attempt failed. Long enough for the PTP
/// layer to lock onto a receiver's own clock: a receiver that only takes
/// anchors on its own timeline (the Sonos case) refuses the ones on ours
/// until then.
pub const ANCHOR_GIVE_UP: Duration = Duration::from_secs(15);

/// Why one SETRATEANCHORTIME attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorFailure {
    /// The receiver answered with this status.
    Refused { status: u16, detail: String },
    /// No usable answer: a timeout or a connection error.
    NoAnswer(String),
    /// The receiver's clock reading can't be turned into a playback time.
    BadClock(String),
}

impl std::fmt::Display for AnchorFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnchorFailure::Refused { detail, .. } | AnchorFailure::NoAnswer(detail) | AnchorFailure::BadClock(detail) => {
                f.write_str(detail)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorAction {
    /// Try again; `warn`: log this failure as a warning (the first one, or
    /// one unlike the last), otherwise at debug.
    Retry { warn: bool },
    /// The bound has passed: end the session.
    GiveUp,
}

/// Retry policy for the buffered stream's anchor: retried until
/// [`ANCHOR_GIVE_UP`] after the first failure.
#[derive(Debug)]
pub struct AnchorRetry {
    give_up: Duration,
    first_failure: Option<Instant>,
    last: Option<AnchorFailure>,
}

impl AnchorRetry {
    pub fn new() -> Self {
        Self::giving_up_after(ANCHOR_GIVE_UP)
    }

    pub fn giving_up_after(give_up: Duration) -> Self {
        Self { give_up, first_failure: None, last: None }
    }

    pub fn on_failure(&mut self, failure: &AnchorFailure, now: Instant) -> AnchorAction {
        let first = *self.first_failure.get_or_insert(now);
        if now.saturating_duration_since(first) >= self.give_up {
            return AnchorAction::GiveUp;
        }
        let warn = self.last.as_ref() != Some(failure);
        self.last = Some(failure.clone());
        AnchorAction::Retry { warn }
    }
}

impl Default for AnchorRetry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// UDP media sends
// ---------------------------------------------------------------------------

/// How long UDP sends may keep failing before the session faults.
pub const MEDIA_SEND_GRACE: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendAction {
    None,
    /// First failure of a streak.
    Delayed,
    /// A send succeeded after a failure streak.
    Recovered,
    Fault(Ap2Fault),
}

/// Tolerates transient UDP send errors (Wi-Fi roam, adapter reset): each
/// failure is counted as media loss; only [`MEDIA_SEND_GRACE`] without a
/// single successful send ends the session.
#[derive(Debug)]
pub struct SendHealth {
    last_ok: Instant,
    failing: bool,
    pub errors: u64,
}

impl SendHealth {
    pub fn new(now: Instant) -> Self {
        Self { last_ok: now, failing: false, errors: 0 }
    }

    pub fn on_ok(&mut self, now: Instant) -> SendAction {
        self.last_ok = now;
        if std::mem::take(&mut self.failing) {
            SendAction::Recovered
        } else {
            SendAction::None
        }
    }

    pub fn on_err(&mut self, now: Instant) -> SendAction {
        self.errors += 1;
        let silent = now.saturating_duration_since(self.last_ok);
        if silent >= MEDIA_SEND_GRACE {
            return SendAction::Fault(Ap2Fault::new(
                "media_send_timeout",
                FaultChannel::Media,
                true,
                format!("no successful audio send for {} s", silent.as_secs()),
            ));
        }
        if !self.failing {
            self.failing = true;
            return SendAction::Delayed;
        }
        SendAction::None
    }
}

// ---------------------------------------------------------------------------
// Device volume read-back
// ---------------------------------------------------------------------------

/// How long a volume write may stay unmatched before it's "unconfirmed".
pub const VOLUME_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// Reported state of the receiver's own volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSync {
    /// A write is under [`VOLUME_CONFIRM_WINDOW`] old and not matched yet.
    Pending,
    /// The receiver reads back the commanded value (or, after that, a
    /// value changed on the device itself, accepted as the new truth).
    Confirmed,
    /// The write failed, or the window passed without a match.
    Unconfirmed,
    /// The receiver doesn't report its volume.
    Unsynced,
}

/// Device volume as last read back. `pct` is `None` whenever the receiver
/// gave no value — it is never invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceVolume {
    pub pct: Option<u32>,
    pub sync: VolumeSync,
}

/// Inverse of [`crate::airplay::session::volume_pct_to_raop_db`]: the
/// receiver's dB back to 0..=100. At or below −30 dB (and the −144 mute
/// sentinel) reads as 0 %.
pub fn raop_db_to_volume_pct(db: f64) -> u32 {
    if !db.is_finite() || db < -30.0 {
        return 0;
    }
    let pct = 1.0 + (db.min(0.0) + 30.0) * 99.0 / 30.0;
    (pct.round() as u32).clamp(1, 100)
}

#[derive(Debug)]
pub struct VolumeTracker {
    /// `(sequence, pct, written at)` of the latest command.
    command: Option<(u64, u32, Instant)>,
    write_failed: bool,
    confirmed: bool,
    last_read: Option<u32>,
    readable: bool,
    next_seq: u64,
}

impl Default for VolumeTracker {
    fn default() -> Self {
        Self { command: None, write_failed: false, confirmed: false, last_read: None, readable: true, next_seq: 1 }
    }
}

impl VolumeTracker {
    /// Record a volume command; returns its sequence number (latest wins).
    pub fn on_write(&mut self, pct: u32, now: Instant) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.command = Some((seq, pct.min(100), now));
        self.write_failed = false;
        self.confirmed = false;
        seq
    }

    /// The write for `seq` failed on the wire.
    pub fn on_write_failed(&mut self, seq: u64) {
        if self.command.map(|(s, _, _)| s) == Some(seq) {
            self.write_failed = true;
        }
    }

    /// A read-back: `Some(pct)` from `initialVolume`, `None` when the
    /// receiver answered without one or the read failed.
    pub fn on_read(&mut self, pct: Option<u32>) {
        match pct {
            Some(p) => {
                self.readable = true;
                self.last_read = Some(p);
                if let Some((_, want, _)) = self.command {
                    if p == want {
                        self.confirmed = true;
                    }
                }
            }
            None => {
                self.readable = false;
                self.last_read = None;
            }
        }
    }

    pub fn state(&self, now: Instant) -> DeviceVolume {
        if !self.readable {
            return DeviceVolume { pct: None, sync: VolumeSync::Unsynced };
        }
        let sync = match self.command {
            None => {
                if self.last_read.is_some() {
                    VolumeSync::Confirmed
                } else {
                    VolumeSync::Unsynced
                }
            }
            Some(_) if self.write_failed => VolumeSync::Unconfirmed,
            // Once confirmed, later physical changes are the new truth.
            Some(_) if self.confirmed => VolumeSync::Confirmed,
            Some((_, _, at)) if now.saturating_duration_since(at) < VOLUME_CONFIRM_WINDOW => VolumeSync::Pending,
            Some(_) => VolumeSync::Unconfirmed,
        };
        DeviceVolume { pct: self.last_read, sync }
    }
}

/// Log a feedback or send notice once, at the right level.
pub fn log_notice(host: &str, what: &str, recovered: bool) {
    if recovered {
        info!("AirPlay 2 {}: {} recovered", host, what);
    } else {
        warn!("AirPlay 2 {}: {}; audio continues", host, what);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// "Again" means what the final sentence says: the session raising the
    /// fault is the reconnect that followed the same fault on the same
    /// speaker (it started within the window after it), and it failed
    /// before it played. Everything else is retryable.
    #[test]
    fn a_fault_is_final_only_when_the_reconnect_fails_before_playing() {
        let t = t0() + Duration::from_secs(3600);
        let s = Duration::from_secs;
        let mut m = RepeatMemory::new();
        // A session that had played for a minute: first time, reconnect.
        assert!(!m.repeated("Kitchen", "anchor_failed", t - s(60), true, t));
        // Its reconnect, started 6 s later, fails before playing: final.
        assert!(m.repeated("Kitchen", "anchor_failed", t + s(6), false, t + s(25)));
        // A later attempt starts over.
        assert!(!m.repeated("Kitchen", "anchor_failed", t + s(40), false, t + s(60)));

        // A reconnect that played for 85 s before the same fault: retryable.
        let mut m = RepeatMemory::new();
        assert!(!m.repeated("Kitchen", "encoder_failed", t - s(60), true, t));
        assert!(!m.repeated("Kitchen", "encoder_failed", t + s(5), true, t + s(90)));

        // A session that started too long after the fault isn't its reconnect.
        let mut m = RepeatMemory::new();
        assert!(!m.repeated("Kitchen", "anchor_failed", t - s(20), false, t));
        assert!(!m.repeated("Kitchen", "anchor_failed", t + REPEAT_WINDOW + s(1), false, t + REPEAT_WINDOW + s(20)));

        // Nor one that was already running when the fault came.
        let mut m = RepeatMemory::new();
        assert!(!m.repeated("Kitchen", "anchor_failed", t - s(20), false, t));
        assert!(!m.repeated("Kitchen", "anchor_failed", t - s(5), false, t + s(10)));

        // Another fault, or the same fault on another speaker, is no repeat.
        let mut m = RepeatMemory::new();
        assert!(!m.repeated("Kitchen", "anchor_failed", t - s(20), false, t));
        assert!(!m.repeated("Kitchen", "encoder_failed", t + s(6), false, t + s(7)));
        assert!(!m.repeated("Bedroom", "anchor_failed", t + s(6), false, t + s(8)));
    }

    /// The final fault is what the window shows after "Connection to …
    /// ended:", so it reads as the sentence written for the person. A
    /// session that never finished its bring-up (an unarmed slot) raises
    /// its fault but leaves nothing to repeat.
    #[test]
    fn a_repeated_fault_ends_the_session_with_its_sentence() {
        let raise = |slot: &FaultSlot| {
            slot.raise_unless_repeated("test_fault", FaultChannel::Media, "detail for the log".into(), false, || {
                "it stopped; select it again.".into()
            })
        };
        // Never finished bring-up: retryable, nothing remembered.
        let unarmed = FaultSlot::new(Arc::new(AtomicBool::new(false)), "Repeat test speaker".into());
        raise(&unarmed);
        assert!(unarmed.get().unwrap().retryable);
        for (round, retryable) in [(1, true), (2, false)] {
            let dead = Arc::new(AtomicBool::new(false));
            let slot = FaultSlot::new(dead.clone(), "Repeat test speaker".into());
            slot.arm(Instant::now());
            raise(&slot);
            assert!(dead.load(Ordering::Acquire), "round {round}");
            let f = slot.get().unwrap();
            assert_eq!(f.retryable, retryable, "round {round}");
            if !retryable {
                assert_eq!(f.to_string(), "it stopped; select it again.");
            }
        }
        // A slot that already holds a fault keeps it and remembers nothing.
        let slot = FaultSlot::new(Arc::new(AtomicBool::new(false)), "Busy test speaker".into());
        slot.arm(Instant::now());
        slot.raise(Ap2Fault::new("first", FaultChannel::Feedback, true, "first"));
        slot.raise_unless_repeated("test_fault", FaultChannel::Media, "x".into(), false, || "x".into());
        assert_eq!(slot.get().unwrap().code, "first");
        let fresh = FaultSlot::new(Arc::new(AtomicBool::new(false)), "Busy test speaker".into());
        fresh.arm(Instant::now());
        fresh.raise_unless_repeated("test_fault", FaultChannel::Media, "x".into(), false, || "x".into());
        assert!(fresh.get().unwrap().retryable);
    }

    /// A refused or unanswered anchor is retried for a bounded time, with
    /// the warning logged once and again only when the failure changes.
    #[test]
    fn anchor_failures_warn_once_per_change_and_give_up_after_the_bound() {
        let now = t0();
        let s = Duration::from_secs;
        let refused = || AnchorFailure::Refused { status: 400, detail: "SETRATEANCHORTIME → 400 Bad Request".into() };
        let mut r = AnchorRetry::new();
        assert_eq!(r.on_failure(&refused(), now), AnchorAction::Retry { warn: true });
        assert_eq!(r.on_failure(&refused(), now + s(1)), AnchorAction::Retry { warn: false });
        let silent = AnchorFailure::NoAnswer("timed out after 5s".into());
        assert_eq!(r.on_failure(&silent, now + s(7)), AnchorAction::Retry { warn: true });
        assert_eq!(r.on_failure(&refused(), now + s(8)), AnchorAction::Retry { warn: true });
        assert_eq!(r.on_failure(&refused(), now + ANCHOR_GIVE_UP - s(1)), AnchorAction::Retry { warn: false });
        assert_eq!(r.on_failure(&refused(), now + ANCHOR_GIVE_UP), AnchorAction::GiveUp);
    }

    /// While the app reconnects after a first fault, the window says what
    /// happened: a session that couldn't play isn't reported as a lost
    /// connection; faults that are connection losses keep the app's own
    /// "Lost connection" text (no text here).
    #[test]
    fn a_session_that_could_not_play_is_not_reported_as_a_lost_connection() {
        let fault = |code| Ap2Fault::new(code, FaultChannel::Media, true, "detail");
        let (line, notice) = fault("anchor_failed").reconnect_text("Kitchen").unwrap();
        assert_eq!(notice, "Kitchen didn't start playing — reconnecting…");
        assert_eq!(line, "Kitchen didn't start playing");
        let (line, notice) = fault("encoder_failed").reconnect_text("Kitchen").unwrap();
        assert_eq!(notice, "Windows' AAC encoder stopped working — reconnecting to Kitchen…");
        assert!(!line.contains("dropped") && line.contains("Kitchen"), "{line}");
        for code in ["peer_closed", "feedback_rejected", "feedback_timeout", "media_send_timeout", "media_write_failed"] {
            assert_eq!(fault(code).reconnect_text("Kitchen"), None, "{code}");
        }
    }

    #[test]
    fn feedback_503_503_200_recovers_without_fault() {
        let now = t0();
        let mut m = FeedbackMonitor::new(true);
        m.on_sent(now);
        assert_eq!(m.on_status(503, now), FeedbackAction::None);
        m.on_sent(now);
        assert_eq!(m.on_status(503, now), FeedbackAction::None);
        m.on_sent(now);
        assert_eq!(m.on_status(200, now), FeedbackAction::Recovered);
        assert_eq!(m.failures, 2);
    }

    #[test]
    fn feedback_three_failures_fault_retryable_in_both_modes() {
        let now = t0();
        for strict in [false, true] {
            // Unknown statuses are ordinary failures too.
            for statuses in [[503, 502, 504], [404, 501, 400], [403, 500, 418]] {
                if strict && statuses.contains(&403) {
                    continue;
                }
                let mut m = FeedbackMonitor::new(strict);
                for st in &statuses[..2] {
                    m.on_sent(now);
                    assert_eq!(m.on_status(*st, now), FeedbackAction::None);
                }
                m.on_sent(now);
                match m.on_status(statuses[2], now) {
                    FeedbackAction::Fault(f) => assert!(f.retryable && f.code == "feedback_rejected"),
                    other => panic!("{other:?}"),
                }
            }
        }
    }

    #[test]
    fn feedback_strict_status_classes() {
        let now = t0();
        let fault = |strict, st| match FeedbackMonitor::new(strict).on_status(st, now) {
            FeedbackAction::Fault(f) => Some((f.code, f.retryable)),
            _ => None,
        };
        assert_eq!(fault(true, 403), Some(("authentication_failed", false)));
        // Final, so the window shows it: it says what to try.
        match FeedbackMonitor::new(true).on_status(403, now) {
            FeedbackAction::Fault(f) => assert!(f.to_string().ends_with("Select it again."), "{f}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(fault(true, 401), Some(("authentication_failed", false)));
        assert_eq!(fault(true, 454), Some(("session_rejected", true)));
        assert_eq!(fault(true, 400), None);
        // Outside strict mode a single reply never ends the session.
        assert_eq!(fault(false, 403), None);
        assert_eq!(fault(false, 454), None);
    }

    #[test]
    fn feedback_transport_errors_count_as_failures() {
        let mut m = FeedbackMonitor::new(false);
        assert_eq!(m.on_error("reset".into()), FeedbackAction::None);
        assert_eq!(m.on_error("reset".into()), FeedbackAction::None);
        assert!(matches!(m.on_error("reset".into()), FeedbackAction::Fault(f) if f.retryable));
    }

    #[test]
    fn feedback_late_reply_is_delayed_then_recovered_without_resend() {
        let now = t0();
        let mut m = FeedbackMonitor::new(true);
        m.on_sent(now);
        assert_eq!(m.on_tick(now + Duration::from_secs(1)), FeedbackAction::None);
        assert_eq!(m.on_tick(now + FEEDBACK_SOFT_TIMEOUT), FeedbackAction::Delayed);
        // Still the same outstanding request — the loop must not send a new one.
        assert!(m.outstanding());
        assert_eq!(m.on_tick(now + Duration::from_secs(6)), FeedbackAction::None);
        assert_eq!(m.on_status(200, now + Duration::from_secs(7)), FeedbackAction::Recovered);
        assert_eq!(m.last_rtt, Some(Duration::from_secs(7)));
        assert!(!m.outstanding());
    }

    #[test]
    fn feedback_hard_timeout() {
        let now = t0();
        let mut m = FeedbackMonitor::new(true);
        m.on_sent(now);
        assert_eq!(m.on_tick(now + FEEDBACK_SOFT_TIMEOUT), FeedbackAction::Delayed);
        match m.on_tick(now + FEEDBACK_HARD_TIMEOUT) {
            FeedbackAction::Fault(f) => assert!(f.retryable && f.code == "feedback_timeout"),
            other => panic!("{other:?}"),
        }
        // Lenient: one unanswered request is one failure; the next goes out.
        let mut m = FeedbackMonitor::new(false);
        m.on_sent(now);
        assert_eq!(m.on_tick(now + FEEDBACK_HARD_TIMEOUT), FeedbackAction::None);
        assert!(!m.outstanding());
    }

    #[test]
    fn transient_send_error_two_notices_persistent_faults() {
        let now = t0();
        let mut h = SendHealth::new(now);
        assert_eq!(h.on_err(now + Duration::from_millis(8)), SendAction::Delayed);
        assert_eq!(h.on_err(now + Duration::from_millis(16)), SendAction::None);
        assert_eq!(h.on_ok(now + Duration::from_millis(24)), SendAction::Recovered);
        assert_eq!(h.on_ok(now + Duration::from_millis(32)), SendAction::None);
        // Persistent: errors only, from the last success onward.
        let base = now + Duration::from_millis(32);
        assert_eq!(h.on_err(base + Duration::from_secs(1)), SendAction::Delayed);
        assert_eq!(h.on_err(base + Duration::from_secs(11)), SendAction::None);
        match h.on_err(base + MEDIA_SEND_GRACE) {
            SendAction::Fault(f) => assert_eq!(f.code, "media_send_timeout"),
            other => panic!("{other:?}"),
        }
        assert_eq!(h.errors, 5);
    }

    #[test]
    fn volume_pending_then_confirmed_after_stale_read() {
        let now = t0();
        let mut v = VolumeTracker::default();
        v.on_read(Some(40));
        assert_eq!(v.state(now).sync, VolumeSync::Confirmed);
        v.on_write(70, now);
        // First read after the write still shows the old value.
        v.on_read(Some(40));
        assert_eq!(v.state(now + Duration::from_millis(500)), DeviceVolume { pct: Some(40), sync: VolumeSync::Pending });
        v.on_read(Some(70));
        assert_eq!(v.state(now + Duration::from_secs(1)).sync, VolumeSync::Confirmed);
        // A later physical change on the device is accepted.
        v.on_read(Some(55));
        assert_eq!(v.state(now + Duration::from_secs(2)), DeviceVolume { pct: Some(55), sync: VolumeSync::Confirmed });
    }

    #[test]
    fn volume_unconfirmed_and_unsynced() {
        let now = t0();
        let mut v = VolumeTracker::default();
        v.on_read(Some(10));
        v.on_write(90, now);
        assert_eq!(v.state(now + VOLUME_CONFIRM_WINDOW).sync, VolumeSync::Unconfirmed);
        let s = v.on_write(20, now);
        v.on_write_failed(s);
        assert_eq!(v.state(now).sync, VolumeSync::Unconfirmed);
        // Missing initialVolume: no value is invented.
        v.on_read(None);
        assert_eq!(v.state(now), DeviceVolume { pct: None, sync: VolumeSync::Unsynced });
    }

    #[test]
    fn db_to_pct_inverts_the_write_mapping() {
        use crate::airplay::session::volume_pct_to_raop_db;
        for pct in 0..=100u32 {
            assert_eq!(raop_db_to_volume_pct(volume_pct_to_raop_db(pct) as f64), pct);
        }
        assert_eq!(raop_db_to_volume_pct(-144.0), 0);
        assert_eq!(raop_db_to_volume_pct(f64::NAN), 0);
    }
}
