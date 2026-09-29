//! Settings and pure helpers for the AirPlay 2 stereo-pair / Apple-TV-group
//! experiments.
//!
//! Everything here applies ONLY to "pair/group sessions": a stereo pair
//! row, an Apple TV row that claims HomePods, a group led by a pair, and
//! the member row of a stereo-pair half whose partner is discovered too
//! (it advertises a `tsid` that another discovered HomePod shares). Every
//! other session is a single receiver — a lone HomePod included, even one
//! folded under an Apple TV row — and the helpers here return the
//! single-receiver decision for it: the settings are `None`, and the
//! session gates each pair/group rule on that ([`ptp_options`],
//! [`session_rules`], [`request_order`], [`setpeers`], [`want_buffered`],
//! [`use_ptp_timing`]). A single receiver therefore sends what it sent
//! before these settings existed: the same RTSP requests in the same
//! order with the same SETUP and SETPEERS bodies and a random
//! per-connection identity, the same stream kind, a random SSRC, sync
//! packets sampled when the sync thread wakes, plain-1588 PTP framing
//! (unless `airplay_force_gptp_framing` is set) with the same rules for
//! following its clock, the NTP timing responder started after RECORD,
//! and a member ended after three failed `/feedback` requests in a row.
//!
//! The settings:
//!
//! | setting | values | what it changes |
//! |---|---|---|
//! | recipe | `ma` \| `apple` | SETUP(session) keys, request order, SETPEERS body + content type |
//! | targets | `both` \| `leader` | both halves of a pair, or only its tight-sync leader |
//! | tv_row | `homepods` \| `tv` | Apple-TV-led row → its HomePods, or one session to the TV |
//! | stream | `realtime` \| `buffered` | stream kind for pair/group sessions |
//! | ptp_role | `master` \| `follow` | we are grandmaster, or we follow a member's clock |
//! | timing | `ptp` \| `ntp` | PTP, or NTP with pyatv's SETUP keys and request order |
//! | split | `off` \| `on` \| `swap` | sender-side L/R channel split per half |
//! | sender relay | on \| off | `senderSupportsRelay` in the apple recipe |
//!
//! Pair/group sessions also present the install's persistent sender
//! identity ([`SenderIdentity`]) and decrypt/answer the event channel.
//!
//! All pure functions here are unit-tested; the session code only
//! executes what they decide.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::net::IpAddr;

use crate::airplay::ap2_ptp::{PtpMode, PtpOptions};
use crate::airplay::discovery::AirPlayRenderer;

/// Content type of a SETPEERS body in the single-receiver path and the
/// `ma` recipe (Music Assistant's airplay-cli sends it this way).
pub const SETPEERS_CT_PLIST: &str = "application/x-apple-binary-plist";
/// Content type OwnTone uses for SETPEERS (airplay.c).
pub const SETPEERS_CT_PEER_LIST_CHANGED: &str = "/peer-list-changed";

/// A string-valued setting enum. Serialises as its lowercase name;
/// deserialises leniently — an unknown or non-string value falls back to
/// the default with a warning instead of failing the whole config file
/// (which `UserConfig::load_from` would quarantine).
macro_rules! setting_enum {
    (
        $(#[$m:meta])*
        $name:ident default $def:ident {
            $( $(#[$vm:meta])* $var:ident => $s:literal ),+ $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vm])* $var ),+
        }

        impl $name {
            /// Every value, in display order (for the GUI dropdown).
            pub const ALL: &'static [$name] = &[$($name::$var),+];

            /// The config / log spelling.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( $name::$var => $s ),+
                }
            }

            /// Case-insensitive parse of the config spelling.
            pub fn parse(s: &str) -> Option<Self> {
                let s = s.trim();
                $( if s.eq_ignore_ascii_case($s) { return Some($name::$var); } )+
                None
            }
        }

        impl Default for $name {
            fn default() -> Self {
                $name::$def
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = serde_json::Value::deserialize(d)?;
                Ok(match v.as_str().and_then($name::parse) {
                    Some(x) => x,
                    None => {
                        log::warn!(
                            "config: {} value {} not recognised; using {}",
                            stringify!($name),
                            v,
                            $name::$def.as_str()
                        );
                        $name::$def
                    }
                })
            }
        }
    };
}

setting_enum! {
    /// SETUP(session) keys, request order and SETPEERS shape for pair/group
    /// sessions.
    PairRecipe default Ma {
        /// Music Assistant (music-assistant/airplay-cli, ap2_client.c):
        /// name, macAddress, a random groupUUID per connection,
        /// groupContainsGroupLeader=false; order SETUP, events, RECORD,
        /// SETUP(stream), SETPEERS [receiver, us] as a binary plist,
        /// volume. OwnTone also sends a random per-connection groupUUID
        /// and groupContainsGroupLeader=false, but sends SETPEERS before
        /// SETUP(stream).
        Ma => "ma",
        /// Keys modelled on Apple senders as pyatv (airplayv2.py) and
        /// phranck/AirplayKit (Protocol-Session.md) describe them:
        /// isMultiSelectAirPlay, sessionCorrelationUUID,
        /// senderSupportsRelay. Every member gets the same groupUUID; that
        /// is this app's choice, not a documented Apple behaviour. The
        /// order is that of OwnTone commit 9de446cadc ("Make RECORD before
        /// SETPEERS, like iOS"): SETUP, events, RECORD, SETPEERS [this,
        /// buddy, us], SETUP(stream).
        Apple => "apple",
    }
}

setting_enum! {
    /// Which halves of a stereo pair a pair unit streams to.
    PairTargets default Both {
        /// One session per half, the same full stereo stream to each.
        Both => "both",
        /// One session to the tight-sync leader half only (flags bit 13,
        /// else the `gid` +0 half), to test whether that half plays the
        /// whole pair.
        Leader => "leader",
    }
}

setting_enum! {
    /// What a row led by an Apple TV streams to.
    TvRow default Homepods {
        /// The TV's HomePods (each pair unit like a bare pair); the TV
        /// itself gets no audio.
        Homepods => "homepods",
        /// One realtime AirPlay 2 session to the Apple TV only.
        Tv => "tv",
    }
}

setting_enum! {
    /// Stream kind for pair/group sessions.
    PairStream default Realtime {
        /// Realtime type 96 (UDP, ALAC, 0xD7 sync) — what MA/OwnTone use.
        Realtime => "realtime",
        /// Buffered type 103 when the receivers support it (the
        /// single-receiver choice); SETRATEANCHORTIME anchoring.
        Buffered => "buffered",
    }
}

setting_enum! {
    /// Clock role for pair/group sessions.
    PtpRole default Master {
        /// We are the PTP grandmaster. With two or more members anchors and
        /// sync packets use our clock and the members' own clocks are only
        /// logged; a session with ONE member (a half's member row,
        /// `targets=leader`, the TV row set to `tv`) also follows that
        /// member's own announced clock, as a single receiver does.
        Master => "master",
        /// We send no Announce/Sync/Signaling, follow the first member
        /// whose Sync/Follow_Up locks and send it Delay_Req, as a PTP
        /// slave of that member would.
        Follow => "follow",
    }
}

setting_enum! {
    /// Timing protocol for pair/group sessions.
    PairTiming default Ptp {
        /// PTP whenever the receivers advertise it (the single-receiver rule).
        Ptp => "ptp",
        /// NTP timing with pyatv 0.18's SETUP(session) shape.
        Ntp => "ntp",
    }
}

setting_enum! {
    /// Sender-side channel split for stereo-pair halves.
    PairSplit default Off {
        /// Both halves get the full stereo stream; each picks its channel
        /// (reported in owntone#1291: "each speaker knows which channel to
        /// play but all speakers receive the same stream").
        Off => "off",
        /// The left half gets the left channel in both channels, the other
        /// half the right channel.
        On => "on",
        /// As `on`, sides reversed.
        Swap => "swap",
    }
}

/// What a row click streams to — the two target settings, as discovery
/// needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TargetPolicy {
    pub pair_targets: PairTargets,
    pub tv_row: TvRow,
}

/// Snapshot of every pair/group setting, taken at connect time (changes
/// apply on the next connect).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairSettings {
    pub recipe: PairRecipe,
    pub targets: PairTargets,
    pub tv_row: TvRow,
    pub stream: PairStream,
    pub ptp_role: PtpRole,
    pub timing: PairTiming,
    pub split: PairSplit,
    /// `senderSupportsRelay=true` in the apple recipe (false otherwise).
    pub sender_relay: bool,
    /// The user's "left HomePod" choice per pair: `tsid` → stable id of
    /// the left half. Absent = the pair's `gid` +0 half.
    pub left_halves: HashMap<String, String>,
}

impl PairSettings {
    pub fn target_policy(&self) -> TargetPolicy {
        TargetPolicy { pair_targets: self.targets, tv_row: self.tv_row }
    }

    /// The experiment line every pair/group session logs at its start:
    /// every switch in one place, plus what the session actually runs —
    /// its clock behaviour ([`clock_label`]) and request order
    /// ([`order_label`]) — and, on NTP, what of pyatv's shape is left out
    /// on purpose.
    pub fn experiment_line(&self, row: &str, members: &[String], clock: &str, order: &str) -> String {
        format!(
            "AP2 experiment: recipe={} targets={} tv-row={} stream={} ptp-role={} timing={} split={} relay={} identity=persistent clock={} order={}{} row={} members=[{}]",
            self.recipe,
            self.targets,
            self.tv_row,
            self.stream,
            self.ptp_role,
            self.timing,
            self.split,
            if self.sender_relay { "on" } else { "off" },
            clock,
            order,
            if clock == CLOCK_NTP { format!(" ntp-omits=[{}]", NTP_PYATV_OMITTED.join(",")) } else { String::new() },
            row,
            members.join(", "),
        )
    }
}

/// The sender identity pair/group sessions present on every connection:
/// one per install, like OwnTone and Music Assistant, whose users report
/// working pairs (single receivers keep a random identity per
/// connection). `Active-Remote`, `sessionUUID` and the RTSP session id
/// stay per connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderIdentity {
    /// `deviceID` and `macAddress`, colon hex (`AA:BB:CC:DD:EE:FF`).
    pub device_mac: String,
    /// `DACP-ID` and `Client-Instance`, 16 upper-case hex digits.
    pub dacp_id: String,
}

impl SenderIdentity {
    /// A fresh random identity. The MAC is a locally administered unicast
    /// address (bit 1 of the first octet set, bit 0 clear).
    pub fn generate() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut mac: [u8; 6] = rng.gen();
        mac[0] = (mac[0] & 0xFC) | 0x02;
        Self {
            device_mac: mac.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(":"),
            dacp_id: format!("{:016X}", rng.gen::<u64>()),
        }
    }

    /// True if both fields have the shape the RTSP layer sends verbatim
    /// (a hand-edited config must not put arbitrary bytes into headers).
    pub fn is_valid(&self) -> bool {
        let mac_ok = self.device_mac.len() == 17
            && self.device_mac.split(':').count() == 6
            && self
                .device_mac
                .split(':')
                .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()));
        let dacp_ok = self.dacp_id.len() == 16 && self.dacp_id.chars().all(|c| c.is_ascii_hexdigit());
        mac_ok && dacp_ok
    }
}

/// Which channels one receiver gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelMap {
    /// The full stereo stream (the default for everyone).
    Stereo,
    /// The left channel in both channels.
    Left,
    /// The right channel in both channels.
    Right,
}

impl ChannelMap {
    pub fn label(self) -> &'static str {
        match self {
            ChannelMap::Stereo => "L+R",
            ChannelMap::Left => "L",
            ChannelMap::Right => "R",
        }
    }

    /// Map interleaved 16-bit stereo samples (L, R, L, R, …); `Stereo`
    /// borrows the input unchanged.
    pub fn apply_cow(self, interleaved: &[i16]) -> std::borrow::Cow<'_, [i16]> {
        match self {
            ChannelMap::Stereo => std::borrow::Cow::Borrowed(interleaved),
            other => std::borrow::Cow::Owned(other.apply(interleaved)),
        }
    }

    /// Map interleaved 16-bit stereo samples (L, R, L, R, …).
    pub fn apply(self, interleaved: &[i16]) -> Vec<i16> {
        match self {
            ChannelMap::Stereo => interleaved.to_vec(),
            ChannelMap::Left => interleaved
                .chunks_exact(2)
                .flat_map(|f| [f[0], f[0]])
                .collect(),
            ChannelMap::Right => interleaved
                .chunks_exact(2)
                .flat_map(|f| [f[1], f[1]])
                .collect(),
        }
    }
}

/// Every discovered half of the stereo pair `r` belongs to (same `tsid`),
/// `r` included, in pair order: the `gid` +0 half first, then +1, unknown
/// index last, ties by MAC. Empty when `r` is not a pair half.
pub fn pair_halves(r: &AirPlayRenderer, all: &[AirPlayRenderer]) -> Vec<AirPlayRenderer> {
    let Some(tsid) = r.group.tight_sync_id.as_deref() else {
        return Vec::new();
    };
    let mut halves: Vec<AirPlayRenderer> = all
        .iter()
        .filter(|o| o.group.tight_sync_id.as_deref() == Some(tsid))
        .cloned()
        .collect();
    if !halves.iter().any(|h| h.mac_id == r.mac_id) {
        halves.push(r.clone());
    }
    halves.sort_by(|a, b| {
        a.group
            .tight_sync_index
            .unwrap_or(u8::MAX)
            .cmp(&b.group.tight_sync_index.unwrap_or(u8::MAX))
            .then_with(|| a.mac_id.cmp(&b.mac_id))
    });
    halves
}

/// The other halves of `r`'s pair (its tight-sync buddies), by IP — the
/// apple recipe names them in SETPEERS whether or not they get a session.
pub fn pair_buddy_ips(r: &AirPlayRenderer, all: &[AirPlayRenderer]) -> Vec<IpAddr> {
    pair_halves(r, all)
        .into_iter()
        .filter(|h| h.mac_id != r.mac_id)
        .map(|h| h.ip)
        .collect()
}

/// The left half of a pair: the user's choice when it names one of these
/// halves, else the `gid` +0 half, else the first half in pair order
/// (lowest MAC — a guess, which is why the choice exists). `halves` must
/// be in [`pair_halves`] order.
pub fn left_half<'a>(halves: &'a [AirPlayRenderer], choice: Option<&str>) -> Option<&'a AirPlayRenderer> {
    if let Some(id) = choice {
        if let Some(h) = halves.iter().find(|h| h.stable_id() == id) {
            return Some(h);
        }
    }
    halves
        .iter()
        .find(|h| h.group.tight_sync_index == Some(0))
        .or_else(|| halves.first())
}

/// Per-target channel map for the split setting: `off` (or a target that
/// is not a pair half) = full stereo; `on` = left half gets L, the other
/// half R; `swap` = the reverse. A half is only split when its partner is
/// one of the targets too — a half streamed alone (`leader`, or its own
/// member row) keeps the full stereo stream.
pub fn channel_maps(
    targets: &[AirPlayRenderer],
    all: &[AirPlayRenderer],
    split: PairSplit,
    left_choice: &HashMap<String, String>,
) -> Vec<ChannelMap> {
    targets
        .iter()
        .map(|t| {
            if split == PairSplit::Off {
                return ChannelMap::Stereo;
            }
            let halves = pair_halves(t, all);
            let targeted = halves
                .iter()
                .filter(|h| targets.iter().any(|x| x.mac_id == h.mac_id))
                .count();
            if halves.len() < 2 || targeted < 2 {
                return ChannelMap::Stereo;
            }
            let tsid = t.group.tight_sync_id.as_deref().unwrap_or_default();
            let is_left = left_half(&halves, left_choice.get(tsid).map(String::as_str))
                .map(|l| l.mac_id == t.mac_id)
                .unwrap_or(false);
            match (split, is_left) {
                (PairSplit::On, true) | (PairSplit::Swap, false) => ChannelMap::Left,
                _ => ChannelMap::Right,
            }
        })
        .collect()
}

/// One phase of an AirPlay 2 session bring-up, after pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// SETUP(session): timing protocol + (for groups) the group keys.
    SetupSession,
    /// TCP connection to the receiver's eventPort.
    Events,
    /// SETUP(stream): the audio stream (with the buffered→realtime fallbacks).
    SetupStream,
    /// Buffered only: the TCP data connection.
    DataConnection,
    /// SETPEERS (PTP only).
    SetPeers,
    /// RECORD.
    Record,
    /// SET_PARAMETER volume.
    Volume,
}

/// The single-receiver order: SETUP(session), events, SETUP(stream), data
/// connection, SETPEERS, RECORD, volume.
const SINGLE_ORDER: &[Step] = &[
    Step::SetupSession,
    Step::Events,
    Step::SetupStream,
    Step::DataConnection,
    Step::SetPeers,
    Step::Record,
    Step::Volume,
];

/// Music Assistant's airplay-cli order (ap2_client.c): SETUP(session),
/// events, RECORD, SETUP(stream), data connection, SETPEERS, volume.
const MA_ORDER: &[Step] = &[
    Step::SetupSession,
    Step::Events,
    Step::Record,
    Step::SetupStream,
    Step::DataConnection,
    Step::SetPeers,
    Step::Volume,
];

/// OwnTone's airplay.c sequence table as of commit 9de446cadc ("Make
/// RECORD before SETPEERS, like iOS"): SETUP(session), RECORD, SETPEERS,
/// SETUP(stream), volume — events right after the session SETUP (the
/// receiver holds RECORD until the channel exists).
const APPLE_ORDER: &[Step] = &[
    Step::SetupSession,
    Step::Events,
    Step::Record,
    Step::SetPeers,
    Step::SetupStream,
    Step::DataConnection,
    Step::Volume,
];

/// pyatv 0.18's order (airplayv2.py `setup`, stream_client.py
/// `send_audio`): SETUP(session), events, SETUP(stream), RECORD, volume —
/// the single-receiver order, whose data-connection and SETPEERS steps
/// are no-ops under NTP. Used by pair/group sessions on NTP, pyatv's
/// timing. pyatv's FLUSH after RECORD is not sent (see
/// [`NTP_PYATV_OMITTED`]).
const PYATV_ORDER: &[Step] = SINGLE_ORDER;

/// Bring-up order: `None` = a single receiver (the single-receiver
/// order); a pair/group session on NTP takes pyatv's order, on PTP the
/// recipe's.
pub fn request_order(recipe: Option<PairRecipe>, use_ptp: bool) -> &'static [Step] {
    match (recipe, use_ptp) {
        (None, _) => SINGLE_ORDER,
        (Some(_), false) => PYATV_ORDER,
        (Some(PairRecipe::Ma), true) => MA_ORDER,
        (Some(PairRecipe::Apple), true) => APPLE_ORDER,
    }
}

/// What pair/group sessions on NTP leave out of pyatv 0.18's shape, on
/// purpose: pyatv's SETUP(session) descriptor keys describe an iPhone
/// (`model` iPhone14,3, `osName` iPhone OS, …), which this sender is not,
/// and its FLUSH with RTP-Info after RECORD is a request no other session
/// of ours sends. Logged on the experiment line of every NTP session.
pub const NTP_PYATV_OMITTED: &[&str] = &[
    "model",
    "osName",
    "osVersion",
    "osBuildVersion",
    "sourceVersion",
    "statsCollectionEnabled",
    "FLUSH",
];

/// Short name of one bring-up step, for the log.
fn step_label(s: Step) -> &'static str {
    match s {
        Step::SetupSession => "setup",
        Step::Events => "events",
        Step::SetupStream => "stream",
        Step::DataConnection => "data",
        Step::SetPeers => "setpeers",
        Step::Record => "record",
        Step::Volume => "volume",
    }
}

/// The request order as the experiment line prints it. Steps that do nothing in
/// this session are left out: SETPEERS under NTP, the data connection
/// unless the stream may be buffered.
pub fn order_label(order: &[Step], use_ptp: bool, may_buffer: bool) -> String {
    order
        .iter()
        .filter(|s| match s {
            Step::SetPeers => use_ptp,
            Step::DataConnection => may_buffer,
            _ => true,
        })
        .map(|s| step_label(*s))
        .collect::<Vec<_>>()
        .join(",")
}

/// SETPEERS body and content type for one receiver:
/// * single receiver / `ma`: `[receiver, us]`, binary-plist content type;
/// * `apple`: `[receiver, its pair buddies…, us]`, `/peer-list-changed`.
pub fn setpeers(
    recipe: Option<PairRecipe>,
    receiver: IpAddr,
    buddies: &[IpAddr],
    local: IpAddr,
) -> (Vec<IpAddr>, &'static str) {
    match recipe {
        None | Some(PairRecipe::Ma) => (vec![receiver, local], SETPEERS_CT_PLIST),
        Some(PairRecipe::Apple) => {
            let mut v = vec![receiver];
            v.extend(buddies.iter().copied().filter(|ip| *ip != receiver && *ip != local));
            v.push(local);
            (v, SETPEERS_CT_PEER_LIST_CHANGED)
        }
    }
}

/// True for the receivers the pair/group settings apply to when streamed
/// alone: a stereo-pair half whose partner is discovered too (at least two
/// receivers in `all`, `r` counted, share its `tsid`). A half whose buddy
/// is unplugged, asleep or not yet resolved — or any receiver advertising
/// a `tsid` nobody else in range shares — is a single receiver.
pub fn is_pair_half(r: &AirPlayRenderer, all: &[AirPlayRenderer]) -> bool {
    pair_halves(r, all).len() >= 2
}

/// One discovered stereo pair, for the GUI's "left HomePod" choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairChoice {
    /// The pair's `tsid` (the key of `airplay_pair_left`).
    pub tsid: String,
    /// The pair's `gpn`, else "A + B".
    pub label: String,
    /// `(stable id, name)` of every half, in pair order.
    pub halves: Vec<(String, String)>,
    /// Stable id of the half used as left when the user has not chosen:
    /// the `gid` +0 half, else the first in pair order.
    pub default_left: String,
}

/// Every stereo pair with at least two discovered halves, sorted by label.
pub fn discovered_pairs(all: &[AirPlayRenderer]) -> Vec<PairChoice> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<PairChoice> = Vec::new();
    for r in all {
        let Some(tsid) = r.group.tight_sync_id.as_deref() else { continue };
        if seen.contains(&tsid) {
            continue;
        }
        seen.push(tsid);
        let halves = pair_halves(r, all);
        if halves.len() < 2 {
            continue;
        }
        let label = halves
            .iter()
            .find_map(|h| h.group.group_name.as_deref().map(str::trim).filter(|n| !n.is_empty()))
            .map(str::to_string)
            .unwrap_or_else(|| halves.iter().map(|h| h.friendly_name.clone()).collect::<Vec<_>>().join(" + "));
        let default_left = left_half(&halves, None).map(|h| h.stable_id()).unwrap_or_default();
        out.push(PairChoice {
            tsid: tsid.to_string(),
            label,
            halves: halves.iter().map(|h| (h.stable_id(), h.friendly_name.clone())).collect(),
            default_left,
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.tsid.cmp(&b.tsid)));
    out
}

/// Stream kind decision. `pair_stream` is `None` for a single receiver:
/// buffered when on PTP, every receiver supports it, and neither the
/// realtime preference nor a sub-second latency asks for realtime —
/// exactly the single-receiver rule. A pair/group session set to
/// `realtime` always streams realtime; `buffered` applies that same rule.
pub fn want_buffered(
    pair_stream: Option<PairStream>,
    use_ptp: bool,
    all_buffered: bool,
    prefer_realtime: bool,
    low_latency: bool,
) -> bool {
    match pair_stream {
        Some(PairStream::Realtime) => false,
        None | Some(PairStream::Buffered) => use_ptp && all_buffered && !prefer_realtime && !low_latency,
    }
}

/// Timing decision. `pair_timing` is `None` for a single receiver: PTP
/// when the receiver advertises it (feature bit 41) — the single-receiver
/// rule, which `ptp` keeps for pair/group sessions; `ntp` forces NTP.
pub fn use_ptp_timing(pair_timing: Option<PairTiming>, advertises_ptp: bool) -> bool {
    match pair_timing {
        Some(PairTiming::Ntp) => false,
        None | Some(PairTiming::Ptp) => advertises_ptp,
    }
}

/// The PTP layer's options: `role` is `None` for a single receiver
/// (follow a lone receiver's own clock with the single-receiver rules,
/// plain-1588 framing unless the config forces gPTP). Pair/group sessions
/// always use gPTP framing and the pair rules for a followed clock, and
/// either serve as grandmaster or follow a member. `master` with exactly
/// one member (a half's member row, `targets=leader`, the TV row set to
/// `tv`) serves our clock AND follows the member's own announced one, as
/// a single receiver does — but counts only that member's own PTP
/// packets, keeps a followed clock's id and offset together across a
/// grandmaster change, and takes a confirmed offset step at once.
pub fn ptp_options(role: Option<PtpRole>, members: usize, force_gptp_for_single: bool) -> PtpOptions {
    let pair = |mode| PtpOptions { mode, gptp_framing: true, pair_session: true };
    match role {
        None => PtpOptions { gptp_framing: force_gptp_for_single, ..PtpOptions::SINGLE },
        Some(PtpRole::Master) if members == 1 => pair(PtpMode::Single),
        Some(PtpRole::Master) => pair(PtpMode::Master),
        Some(PtpRole::Follow) => pair(PtpMode::Follow),
    }
}

/// [`clock_label`] of an NTP session.
pub const CLOCK_NTP: &str = "ntp";

/// The clock behaviour a session runs, for the experiment line: `None` =
/// NTP.
pub fn clock_label(ptp: Option<PtpOptions>) -> &'static str {
    match ptp.map(|o| o.mode) {
        None => CLOCK_NTP,
        Some(PtpMode::Single) => "grandmaster+follows-lone-receiver",
        Some(PtpMode::Master) => "grandmaster",
        Some(PtpMode::Follow) => "follows-a-member",
    }
}

/// The session-level rules a pair/group session runs and a single
/// receiver does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRules {
    /// Start each member's NTP timing responder right after pairing,
    /// before SETUP(session) advertises its port. A single receiver starts
    /// it after RECORD.
    pub early_ntp_responder: bool,
    /// End a member on the first `/feedback` that finds its control
    /// connection closed or reset. A single receiver ends after three
    /// failed `/feedback` requests in a row, whatever the failure.
    pub end_on_first_eof: bool,
    /// Two or more members sharing one RTP timeline: sync packets are
    /// stamped from the realtime sender's schedule (the due time of the
    /// rtptime they name, one value for every member), and the realtime
    /// stream over PTP sends SSRC 0 (see [`ssrc_for`]). A single receiver,
    /// and a pair/group session with one member, sample the clock when the
    /// sync thread wakes and send a random SSRC.
    pub shared_timeline: bool,
}

/// The [`SessionRules`] of a session: `group_session` = it runs with the
/// pair/group settings, `members` = how many receivers it streams to.
pub fn session_rules(group_session: bool, members: usize) -> SessionRules {
    SessionRules {
        early_ntp_responder: group_session,
        end_on_first_eof: group_session,
        shared_timeline: group_session && members >= 2,
    }
}

/// RTP SSRC of the session's audio packets. Pair/group sessions of two or
/// more members ([`SessionRules::shared_timeline`]) on the realtime
/// stream over PTP send 0, as Music Assistant (airplay-cli ap2_client.c:
/// `ssrc = use_ptp ? 0 : session_id`) and OwnTone (rtp_common.c: "ssrc_id
/// is zero if it's a ptp session") do. Everything else keeps `random()`:
/// single receivers and one-member sessions, buffered streams (they keep
/// the single-receiver path's SSRC: shairport-sync, for one, picks its
/// buffered decoder from a packet's SSRC in
/// ap2_buffered_audio_processor.c), and NTP (MA's per-member session id
/// cannot be shared by one header).
pub fn ssrc_for(shared_timeline: bool, use_ptp: bool, buffered: bool, random: impl FnOnce() -> u32) -> u32 {
    if shared_timeline && use_ptp && !buffered {
        0
    } else {
        random()
    }
}

/// The `groupUUID` one member's SETUP(session) carries: `ma` = a fresh
/// random one per connection (Music Assistant / OwnTone), `apple` = the
/// session's one shared value.
pub fn group_uuid_for(recipe: PairRecipe, session_group_uuid: &str, fresh: impl FnOnce() -> String) -> String {
    match recipe {
        PairRecipe::Ma => fresh(),
        PairRecipe::Apple => session_group_uuid.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::discovery::GroupHints;

    fn half(name: &str, mac: &str, ip: &str, tsid: Option<&str>, index: Option<u8>) -> AirPlayRenderer {
        let mut group = GroupHints::default();
        group.tight_sync_id = tsid.map(str::to_string);
        group.tight_sync_index = index;
        AirPlayRenderer {
            friendly_name: name.into(),
            mac_id: mac.into(),
            ip: ip.parse().unwrap(),
            port: 7000,
            airplay_port: Some(7000),
            encryption_types: vec![0],
            codecs: vec![1],
            password_protected: false,
            encryption_key_required: false,
            features: None,
            pk: None,
            model: Some("AudioAccessory1,1".into()),
            group,
        }
    }

    #[test]
    fn enums_parse_leniently_and_serialise_lowercase() {
        assert_eq!(PairRecipe::parse("MA"), Some(PairRecipe::Ma));
        assert_eq!(PairRecipe::parse(" apple "), Some(PairRecipe::Apple));
        assert_eq!(PairRecipe::parse("roon"), None);
        assert_eq!(serde_json::to_string(&PairSplit::Swap).unwrap(), "\"swap\"");
        // Unknown / wrong-typed values fall back to the default instead of
        // failing the whole config.
        let r: PairRecipe = serde_json::from_str("\"nonsense\"").unwrap();
        assert_eq!(r, PairRecipe::Ma);
        let t: PairTiming = serde_json::from_str("42").unwrap();
        assert_eq!(t, PairTiming::Ptp);
        let s: PtpRole = serde_json::from_str("\"Follow\"").unwrap();
        assert_eq!(s, PtpRole::Follow);
    }

    #[test]
    fn defaults_and_the_experiment_line() {
        let s = PairSettings::default();
        assert_eq!(s.recipe, PairRecipe::Ma);
        assert_eq!(s.targets, PairTargets::Both);
        assert_eq!(s.tv_row, TvRow::Homepods);
        assert_eq!(s.stream, PairStream::Realtime);
        assert_eq!(s.ptp_role, PtpRole::Master);
        assert_eq!(s.timing, PairTiming::Ptp);
        assert_eq!(s.split, PairSplit::Off);
        assert!(!s.sender_relay);
        let line = s.experiment_line("stereo pair", &["Links".into(), "Rechts".into()], "grandmaster", "setup,events");
        assert!(line.starts_with("AP2 experiment: recipe=ma targets=both tv-row=homepods stream=realtime ptp-role=master timing=ptp split=off"), "{line}");
        assert!(line.contains(" clock=grandmaster order=setup,events row=stereo pair"), "{line}");
        assert!(!line.contains("ntp-omits"), "{line}");
        // On NTP the line says what of pyatv's shape is left out.
        let line = s.experiment_line("stereo pair", &[], CLOCK_NTP, "setup");
        assert!(
            line.contains("clock=ntp order=setup ntp-omits=[model,osName,osVersion,osBuildVersion,sourceVersion,statsCollectionEnabled,FLUSH] row="),
            "{line}"
        );
    }

    #[test]
    fn sender_identity_is_well_formed_and_stable_through_serde() {
        let id = SenderIdentity::generate();
        assert!(id.is_valid(), "{id:?}");
        let first = u8::from_str_radix(&id.device_mac[..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02, "locally administered unicast");
        let back: SenderIdentity = serde_json::from_str(&serde_json::to_string(&id).unwrap()).unwrap();
        assert_eq!(back, id);
        let bad = SenderIdentity { device_mac: "AA:BB\r\nX".into(), dacp_id: "12".into() };
        assert!(!bad.is_valid());
    }

    #[test]
    fn request_order_per_recipe() {
        use Step::*;
        // Single receivers keep the pre-experiment order: stream SETUP
        // and data connection before SETPEERS and RECORD — on either timing.
        for use_ptp in [true, false] {
            assert_eq!(
                request_order(None, use_ptp),
                &[SetupSession, Events, SetupStream, DataConnection, SetPeers, Record, Volume]
            );
        }
        // ma: RECORD before SETUP(stream), SETPEERS after the stream.
        assert_eq!(
            request_order(Some(PairRecipe::Ma), true),
            &[SetupSession, Events, Record, SetupStream, DataConnection, SetPeers, Volume]
        );
        // apple: RECORD, then SETPEERS, then SETUP(stream).
        assert_eq!(
            request_order(Some(PairRecipe::Apple), true),
            &[SetupSession, Events, Record, SetPeers, SetupStream, DataConnection, Volume]
        );
        // NTP, either recipe: pyatv's order — the stream is set up BEFORE
        // RECORD (airplayv2.py setup, then stream_client.py send_audio).
        for recipe in [PairRecipe::Ma, PairRecipe::Apple] {
            let o = request_order(Some(recipe), false);
            let pos = |s: Step| o.iter().position(|x| *x == s).unwrap();
            assert!(pos(SetupStream) < pos(Record), "{recipe}: {o:?}");
            assert_eq!(order_label(o, false, false), "setup,events,stream,record,volume");
        }
        // Every order runs every step exactly once, events before RECORD
        // and the data connection straight after the stream SETUP.
        for (r, ptp) in [(None, true), (Some(PairRecipe::Ma), true), (Some(PairRecipe::Apple), true), (Some(PairRecipe::Apple), false)] {
            let o = request_order(r, ptp);
            assert_eq!(o.len(), 7);
            let pos = |s: Step| o.iter().position(|x| *x == s).unwrap();
            assert!(pos(Events) < pos(Record));
            assert_eq!(pos(DataConnection), pos(SetupStream) + 1);
            assert_eq!(pos(SetupSession), 0);
            assert_eq!(*o.last().unwrap(), Volume);
        }
        // The log names only the steps that do something.
        assert_eq!(
            order_label(request_order(Some(PairRecipe::Ma), true), true, false),
            "setup,events,record,stream,setpeers,volume"
        );
        assert_eq!(
            order_label(request_order(Some(PairRecipe::Apple), true), true, true),
            "setup,events,record,setpeers,stream,data,volume"
        );
    }

    #[test]
    fn setpeers_list_and_content_type_per_recipe() {
        let rx: IpAddr = "192.0.2.11".parse().unwrap();
        let buddy: IpAddr = "192.0.2.12".parse().unwrap();
        let us: IpAddr = "192.0.2.2".parse().unwrap();
        assert_eq!(setpeers(None, rx, &[buddy], us), (vec![rx, us], SETPEERS_CT_PLIST));
        assert_eq!(setpeers(Some(PairRecipe::Ma), rx, &[buddy], us), (vec![rx, us], SETPEERS_CT_PLIST));
        assert_eq!(
            setpeers(Some(PairRecipe::Apple), rx, &[buddy], us),
            (vec![rx, buddy, us], SETPEERS_CT_PEER_LIST_CHANGED)
        );
        // No buddy known (a lone HomePod in a TV group): [receiver, us].
        assert_eq!(setpeers(Some(PairRecipe::Apple), rx, &[], us).0, vec![rx, us]);
        // Never lists the receiver or us twice.
        assert_eq!(setpeers(Some(PairRecipe::Apple), rx, &[rx, buddy, us], us).0, vec![rx, buddy, us]);
    }

    #[test]
    fn channel_map_apply() {
        let s = [1i16, 2, 3, 4];
        assert_eq!(ChannelMap::Stereo.apply(&s), vec![1, 2, 3, 4]);
        assert_eq!(ChannelMap::Left.apply(&s), vec![1, 1, 3, 3]);
        assert_eq!(ChannelMap::Right.apply(&s), vec![2, 2, 4, 4]);
    }

    #[test]
    fn channel_split_maps_the_plus0_half_left_unless_chosen() {
        const T: &str = "eafa36aa-9785-54b2-a537-d9ee2a55cf1c";
        let links = half("Links", "D4A33D7A28D8", "192.0.2.46", Some(T), Some(0));
        let rechts = half("Rechts", "50BC9607E86D", "192.0.2.56", Some(T), Some(1));
        let solo = half("Kitchen", "E02B9696FB77", "192.0.2.50", None, None);
        let all = vec![links.clone(), rechts.clone(), solo.clone()];
        let targets = vec![rechts.clone(), links.clone(), solo.clone()];
        let none = HashMap::new();

        assert_eq!(
            channel_maps(&targets, &all, PairSplit::Off, &none),
            vec![ChannelMap::Stereo; 3]
        );
        // +0 half (Links) is left by default, whatever the target order.
        assert_eq!(
            channel_maps(&targets, &all, PairSplit::On, &none),
            vec![ChannelMap::Right, ChannelMap::Left, ChannelMap::Stereo]
        );
        assert_eq!(
            channel_maps(&targets, &all, PairSplit::Swap, &none),
            vec![ChannelMap::Left, ChannelMap::Right, ChannelMap::Stereo]
        );
        // The user's choice wins.
        let chosen: HashMap<String, String> = [(T.to_string(), rechts.stable_id())].into();
        assert_eq!(
            channel_maps(&targets, &all, PairSplit::On, &chosen),
            vec![ChannelMap::Left, ChannelMap::Right, ChannelMap::Stereo]
        );
        // A stale choice (not a half of this pair) falls back to +0.
        let stale: HashMap<String, String> = [(T.to_string(), "airplay:FFFFFFFFFFFF".to_string())].into();
        assert_eq!(
            channel_maps(&targets, &all, PairSplit::On, &stale),
            vec![ChannelMap::Right, ChannelMap::Left, ChannelMap::Stereo]
        );
        // A half whose partner is not discovered plays stereo.
        assert_eq!(
            channel_maps(&[links.clone()], &[links.clone()], PairSplit::On, &none),
            vec![ChannelMap::Stereo]
        );
        // A half streamed alone (targets=leader, or its member row) keeps
        // the full stereo stream even though its partner is discovered.
        assert_eq!(
            channel_maps(&[links.clone()], &all, PairSplit::On, &none),
            vec![ChannelMap::Stereo]
        );
        assert_eq!(
            channel_maps(&[links.clone(), solo.clone()], &all, PairSplit::Swap, &none),
            vec![ChannelMap::Stereo, ChannelMap::Stereo]
        );
        // apply_cow borrows for stereo and maps otherwise.
        let s = [1i16, 2, 3, 4];
        assert!(matches!(ChannelMap::Stereo.apply_cow(&s), std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*ChannelMap::Right.apply_cow(&s), &[2, 2, 4, 4]);
    }

    #[test]
    fn discovered_pairs_list_both_halves_with_the_plus0_default() {
        const T: &str = "eafa36aa-9785-54b2-a537-d9ee2a55cf1c";
        let mut links = half("Links", "D4A33D7A28D8", "192.0.2.46", Some(T), Some(0));
        let mut rechts = half("Rechts", "50BC9607E86D", "192.0.2.56", Some(T), Some(1));
        links.group.group_name = Some("Büro 2".into());
        rechts.group.group_name = Some("Büro 2".into());
        let lone = half("Lone", "0A0B0C0D0E0F", "192.0.2.60", Some("other"), None);
        let solo = half("Kitchen", "E02B9696FB77", "192.0.2.50", None, None);
        let pairs = discovered_pairs(&[solo.clone(), rechts.clone(), lone, links.clone()]);
        assert_eq!(pairs.len(), 1, "a half without its partner is not a pair");
        let p = &pairs[0];
        assert_eq!(p.tsid, T);
        assert_eq!(p.label, "Büro 2");
        assert_eq!(
            p.halves,
            vec![
                (links.stable_id(), "Links".to_string()),
                (rechts.stable_id(), "Rechts".to_string())
            ]
        );
        assert_eq!(p.default_left, links.stable_id());
        // No gpn: "A + B".
        links.group.group_name = None;
        rechts.group.group_name = None;
        assert_eq!(discovered_pairs(&[links, rechts])[0].label, "Links + Rechts");
        assert!(discovered_pairs(&[solo]).is_empty());
    }

    #[test]
    fn single_receivers_keep_the_single_receiver_decisions() {
        // Stream kind: `None` (single receiver) is exactly the rule a
        // single receiver always had.
        for use_ptp in [false, true] {
            for all_buffered in [false, true] {
                for prefer_rt in [false, true] {
                    for low in [false, true] {
                        let single = want_buffered(None, use_ptp, all_buffered, prefer_rt, low);
                        assert_eq!(single, use_ptp && all_buffered && !prefer_rt && !low);
                        // `buffered` for a pair applies the same rule …
                        assert_eq!(want_buffered(Some(PairStream::Buffered), use_ptp, all_buffered, prefer_rt, low), single);
                        // … `realtime` (the pair default) never buffers.
                        assert!(!want_buffered(Some(PairStream::Realtime), use_ptp, all_buffered, prefer_rt, low));
                    }
                }
            }
        }
        // Timing: single = what the receiver advertises.
        assert!(use_ptp_timing(None, true));
        assert!(!use_ptp_timing(None, false));
        assert!(use_ptp_timing(Some(PairTiming::Ptp), true));
        assert!(!use_ptp_timing(Some(PairTiming::Ntp), true));
        // PTP: single = follow a lone receiver's clock with the
        // single-receiver rules, plain 1588 framing (transportSpecific 0)
        // unless the config forces gPTP — which changes nothing else.
        assert_eq!(ptp_options(None, 1, false), PtpOptions::SINGLE);
        assert!(!PtpOptions::SINGLE.pair_session);
        assert_eq!(
            ptp_options(None, 1, true),
            PtpOptions { mode: PtpMode::Single, gptp_framing: true, pair_session: false }
        );
        assert_eq!(
            ptp_options(Some(PtpRole::Master), 2, false),
            PtpOptions { mode: PtpMode::Master, gptp_framing: true, pair_session: true }
        );
        assert_eq!(
            ptp_options(Some(PtpRole::Follow), 2, false),
            PtpOptions { mode: PtpMode::Follow, gptp_framing: true, pair_session: true }
        );
        // Order and SETPEERS: None is the single-receiver behaviour (see
        // request_order_per_recipe / setpeers_list_and_content_type_per_recipe).
        assert_eq!(request_order(None, true)[2], Step::SetupStream);
        // Session rules: a single receiver runs none of the pair/group ones.
        let single = session_rules(false, 1);
        assert_eq!(
            single,
            SessionRules { early_ntp_responder: false, end_on_first_eof: false, shared_timeline: false }
        );
        // SSRC: single receivers always get a random one.
        for (ptp, buffered) in [(true, false), (true, true), (false, false)] {
            assert_eq!(ssrc_for(single.shared_timeline, ptp, buffered, || 0xDEAD_BEEF), 0xDEAD_BEEF);
        }
    }

    #[test]
    fn session_rules_gate_the_pair_group_behaviour() {
        // Any pair/group session: early NTP responder, first EOF ends a
        // member. Only two or more members share the scheduled sync and
        // SSRC 0.
        for members in [1, 2, 3] {
            let r = session_rules(true, members);
            assert!(r.early_ntp_responder && r.end_on_first_eof, "{members}");
            assert_eq!(r.shared_timeline, members >= 2, "{members}");
        }
        // A single receiver: none of them, whatever the member count.
        for members in [0, 1, 2] {
            assert_eq!(
                session_rules(false, members),
                SessionRules { early_ntp_responder: false, end_on_first_eof: false, shared_timeline: false }
            );
        }
        // A one-member pair/group session keeps a random SSRC, like a
        // single receiver.
        let one = session_rules(true, 1);
        assert_eq!(ssrc_for(one.shared_timeline, true, false, || 7), 7);
        assert_eq!(ssrc_for(session_rules(true, 2).shared_timeline, true, false, || 7), 0);
    }

    #[test]
    fn a_one_member_master_session_serves_and_follows_like_a_single_receiver() {
        // A half's member row, `targets=leader`, the TV row set to `tv`:
        // serve our clock AND follow the member's own, with the pair
        // sessions' gPTP framing and pair rules for the followed clock.
        let one = ptp_options(Some(PtpRole::Master), 1, false);
        assert_eq!(one, PtpOptions { mode: PtpMode::Single, gptp_framing: true, pair_session: true });
        assert_eq!(clock_label(Some(one)), "grandmaster+follows-lone-receiver");
        // Two or more members: grandmaster only; follow stays follow.
        assert_eq!(clock_label(Some(ptp_options(Some(PtpRole::Master), 2, false))), "grandmaster");
        assert_eq!(ptp_options(Some(PtpRole::Follow), 1, false).mode, PtpMode::Follow);
        assert_eq!(clock_label(Some(ptp_options(Some(PtpRole::Follow), 1, false))), "follows-a-member");
        assert_eq!(clock_label(None), CLOCK_NTP);
    }

    #[test]
    fn pair_realtime_on_ptp_sends_ssrc_zero_like_ma_and_owntone() {
        let random = || 0x1234_5678;
        assert_eq!(ssrc_for(true, true, false, random), 0);
        // Buffered and NTP keep it random.
        assert_eq!(ssrc_for(true, true, true, random), 0x1234_5678);
        assert_eq!(ssrc_for(true, false, false, random), 0x1234_5678);
        // So does a session without a shared timeline.
        assert_eq!(ssrc_for(false, true, false, random), 0x1234_5678);
    }

    #[test]
    fn a_tsid_nobody_else_shares_is_not_a_pair_half() {
        const T: &str = "eafa36aa-9785-54b2-a537-d9ee2a55cf1c";
        let links = half("Links", "D4A33D7A28D8", "192.0.2.46", Some(T), Some(0));
        let rechts = half("Rechts", "50BC9607E86D", "192.0.2.56", Some(T), Some(1));
        let solo = half("Kitchen", "E02B9696FB77", "192.0.2.50", None, None);
        // Both halves discovered: each is a pair half.
        let all = vec![links.clone(), rechts.clone(), solo.clone()];
        assert!(is_pair_half(&links, &all));
        assert!(is_pair_half(&rechts, &all));
        assert!(!is_pair_half(&solo, &all));
        // The partner is offline (or a HomePod advertises a tsid nobody
        // shares): a single receiver.
        let alone = vec![links.clone(), solo];
        assert!(!is_pair_half(&links, &alone));
        assert!(!is_pair_half(&links, &[]));
    }

    #[test]
    fn group_uuid_is_per_connection_for_ma_and_shared_for_apple() {
        let mut n = 0;
        let mut fresh = || {
            n += 1;
            format!("fresh-{n}")
        };
        assert_eq!(group_uuid_for(PairRecipe::Ma, "shared", &mut fresh), "fresh-1");
        assert_eq!(group_uuid_for(PairRecipe::Ma, "shared", &mut fresh), "fresh-2");
        assert_eq!(group_uuid_for(PairRecipe::Apple, "shared", &mut fresh), "shared");
        assert_eq!(group_uuid_for(PairRecipe::Apple, "shared", &mut fresh), "shared");
    }

    #[test]
    fn left_half_without_gid_index_is_the_lowest_mac() {
        // Behind an Apple TV the halves' gid is the TV's (no +N+ index).
        const T: &str = "t";
        let a = half("A", "D4A33D7A28D8", "192.0.2.46", Some(T), None);
        let b = half("B", "50BC9607E86D", "192.0.2.56", Some(T), None);
        let halves = pair_halves(&a, &[a.clone(), b.clone()]);
        assert_eq!(halves[0].friendly_name, "B");
        assert_eq!(left_half(&halves, None).unwrap().friendly_name, "B");
        assert_eq!(pair_buddy_ips(&a, &[a.clone(), b.clone()]), vec![b.ip]);
        assert!(pair_buddy_ips(&half("S", "01", "192.0.2.9", None, None), &[a, b]).is_empty());
    }
}
