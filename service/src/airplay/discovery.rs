//! mDNS-based discovery of AirPlay receivers.
//!
//! AirPlay devices advertise (up to) two Bonjour services:
//!
//!   * `_raop._tcp.local.` — the classic **R**emote **A**udio **O**utput
//!     **P**rotocol (AirPlay 1) audio endpoint. Service name is
//!     `<MAC>@<FriendlyName>`. Carries the `et`/`cn`/`sr`/… TXT keys.
//!   * `_airplay._tcp.local.` — the AirPlay **2** endpoint. Service name
//!     is just `<FriendlyName>`; the MAC lives in the `deviceid` TXT key.
//!     Carries the 64-bit `features`/`ft` flag word, `pk` (the device's
//!     Ed25519 HomeKit public key), `model`, `srcvers`, etc.
//!
//! A modern receiver (HomePod, Apple TV, AirPort Express, Sonos in AP2
//! mode) advertises **both**. We browse both service types and correlate
//! records by MAC so a single [`AirPlayRenderer`] carries everything we
//! need to decide *how* to talk to it:
//!
//!   * **Legacy RAOP** — for devices that accept the unencrypted (`et=0`)
//!     or Apple-RSA (`et=1`) AirPlay-1 flow. Handled by `rtsp.rs`/`rtp.rs`.
//!   * **AirPlay 2 / HomeKit** — for HomePod and AP2-only receivers that
//!     require HomeKit pairing + ChaCha20-Poly1305. Handled by the
//!     `pairing` / `ap2` path.
//!
//! ## RAOP TXT keys (`_raop._tcp`)
//!
//! | Key | Meaning                                              |
//! |-----|------------------------------------------------------|
//! | `cn`      | Comma-separated codecs: 0=PCM 1=ALAC 2=AAC ... |
//! | `et`      | Comma-separated encryption types: 0=none 1=RSA 3=FairPlay 4=FairPlay-SAPv2.5 5=MFi |
//! | `vn`      | RSA version (3 = the published Apple key)       |
//! | `pw`      | Password protected                              |
//! | `am`      | Apple model — purely informational              |
//!
//! ## AirPlay 2 TXT keys (`_airplay._tcp`)
//!
//! | Key | Meaning                                                  |
//! |-----|----------------------------------------------------------|
//! | `deviceid`  | MAC address `AA:BB:CC:DD:EE:FF` — our correlation key |
//! | `features`/`ft` | 64-bit capability bitfield (see [`features`])     |
//! | `flags`     | status flags                                         |
//! | `pk`        | device HomeKit Ed25519 public key (hex)              |
//! | `model`     | e.g. `AudioAccessory5,1` (HomePod), `AppleTV6,2`     |
//! | `srcvers`   | AirPlay source version, e.g. `366.0`                 |
//! | `gid`       | group UUID (see [`GroupHints`])                      |
//! | `igl`       | is group leader (`1`/`0`)                            |
//! | `gcgl`      | group contains (discoverable) group leader           |
//! | `gpn`       | group public name — what the iPhone picker shows     |
//! | `pgid`/`pgcgl` | parent group UUID / parent contains leader        |
//! | `tsid`      | tight-sync (stereo pair) UUID                        |

use anyhow::Result;
use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperties};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::airplay::pair_experiments::{is_pair_half, PairTargets, TargetPolicy, TvRow};

/// Service type for AirPlay 1 / RAOP audio receivers.
pub const RAOP_SERVICE: &str = "_raop._tcp.local.";
/// Service type for AirPlay 2 receivers.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

// ---------------------------------------------------------------------------
// Feature-flag bits (subset we care about). Bit numbers verified against
// OwnTone's `outputs/airplay.c` features map.
// ---------------------------------------------------------------------------

/// Bit 9 — `SupportsAirPlayAudio`. Set by every AirPlay-2 audio receiver.
pub const FEAT_AUDIO: u64 = 1 << 9;
/// Bit 40 — `SupportsBufferedAudio` (srcvers ≥ 354.54.6).
pub const FEAT_BUFFERED_AUDIO: u64 = 1 << 40;
/// Bit 41 — `SupportsPTP`. Devices with this expect IEEE-1588 PTP timing.
pub const FEAT_PTP: u64 = 1 << 41;
/// Bit 27 — `SupportsLegacyPairing`.
pub const FEAT_LEGACY_PAIRING: u64 = 1 << 27;
/// Bit 43 — `SupportsSystemPairing`.
pub const FEAT_SYSTEM_PAIRING: u64 = 1 << 43;
/// Bit 46 — `SupportsHKPairingAndAccessControl`.
pub const FEAT_HK_PAIRING: u64 = 1 << 46;
/// Bit 48 — `SupportsCoreUtilsPairingAndEncryption`. When set the device
/// accepts HomeKit *transient* pairing (PIN-less, the path we use).
pub const FEAT_TRANSIENT_PAIRING: u64 = 1 << 48;

/// Which audio transport we'll use to talk to a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// AirPlay 1 / RAOP: unencrypted (`et=0`) or RSA-AES (`et=1`), no
    /// pairing. The existing `rtsp.rs` + `rtp.rs` path handles this.
    RaopLegacy,
    /// AirPlay 2 with HomeKit transient pairing + ChaCha20-Poly1305.
    /// Required by HomePod and AP2-only receivers.
    AirPlay2,
}

/// One resolved AirPlay receiver, merged across both service types.
#[derive(Debug, Clone)]
pub struct AirPlayRenderer {
    /// Friendly name shown to the user.
    pub friendly_name: String,
    /// Stable ID — the MAC address, normalised to upper-case hex with no
    /// separators (`B8E93735AC11`). Survives device renames / DHCP churn.
    pub mac_id: String,
    /// IPv4 of the receiver (we prefer v4 over v6 for socket simplicity).
    pub ip: IpAddr,
    /// RTSP port advertised by `_raop._tcp` (the legacy audio endpoint).
    /// 0 if the device only advertises `_airplay._tcp`.
    pub port: u16,
    /// RTSP port advertised by `_airplay._tcp` (the AirPlay-2 endpoint),
    /// if present (usually 7000).
    pub airplay_port: Option<u16>,
    /// Encryption types from RAOP `et=` (0=none 1=RSA 3/4/5=FairPlay).
    pub encryption_types: Vec<u8>,
    /// Codec ids from RAOP `cn=` (1=ALAC).
    pub codecs: Vec<u8>,
    /// True if the RAOP service set `pw=true` (password protected). We
    /// don't speak RAOP HTTP-digest auth, so these are legacy-unsupported.
    pub password_protected: bool,
    /// The RAOP `ek=1` flag — "encryption key (expected)". Classic
    /// AirPort Express advertises it and genuinely wants an RSA-wrapped
    /// key; shairport advertises it too but also accepts plaintext. We
    /// use it (with the model) to decide whether to encrypt (see
    /// [`AirPlayRenderer::prefers_rsa_encryption`]).
    pub encryption_key_required: bool,
    /// 64-bit AirPlay-2 `features`/`ft` bitfield, if the device advertised
    /// `_airplay._tcp`.
    pub features: Option<u64>,
    /// Device HomeKit public key (`pk`, hex) from `_airplay._tcp`.
    pub pk: Option<String>,
    /// Model string (`am=` on RAOP or `model` on AirPlay), e.g.
    /// `AudioAccessory5,1` for a HomePod.
    pub model: Option<String>,
    /// Group-membership hints from `_airplay._tcp` (all optional).
    pub group: GroupHints,
}

/// AirPlay-2 group hints, straight from the `_airplay._tcp` TXT record.
///
/// Field semantics per the openairplay service-discovery table, cross-
/// checked against the captures in owntone#1413 (HomePod stereo pair and
/// a user-built multi-room group) and the test fixture in
/// cyrahs/spoticonn#1 (Apple TV with HomePods as its default audio
/// output):
///
/// * `gid` — the group UUID. Every AP2 receiver advertises one, even
///   standalone (Sonos / AirPort Express set `gid = pi`, `gcgl=0`), so a
///   shared `gid` — not its presence — is what makes a group. A HomePod
///   stereo pair suffixes it (`<tsid>+0+<uuid>` / `<tsid>+1+<uuid>`);
///   we strip everything from the first `+` so the halves match.
/// * `pgid` — parent group UUID when a pair is nested in a larger group;
///   it takes precedence as the grouping key.
/// * `igl` — "is group leader". An idle Apple TV advertises `igl=1
///   gcgl=1`; HomePods set as that TV's default output share its `gid`
///   with `igl=0 gcgl=1`. **Both** halves of a stereo pair advertise
///   `igl=1` (the tight-sync sub-group), and a record may lack the
///   hints altogether, so `igl` alone never decides a leader —
///   see [`pick_group_leader`]. Sonos spells the key `isGroupLeader`.
/// * `gcgl` — group contains a discoverable leader.
/// * `gpn` — the group's public name (the iPhone picker's label).
/// * `tsid` — tight-sync UUID, present on stereo-pair members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupHints {
    /// `gid`, normalised (lower-case, `+` suffix stripped); `None` when
    /// absent, empty or the all-zero UUID.
    pub group_id: Option<String>,
    /// `pgid`, normalised like `group_id`.
    pub parent_group_id: Option<String>,
    /// `igl` / `isGroupLeader`.
    pub is_group_leader: Option<bool>,
    /// `gcgl`.
    pub group_contains_leader: Option<bool>,
    /// `pgcgl`.
    pub parent_group_contains_leader: Option<bool>,
    /// `gpn`.
    pub group_name: Option<String>,
    /// `tsid`.
    pub tight_sync_id: Option<String>,
    /// The half index a stereo pair encodes in its `gid`
    /// (`<tsid>+<index>+<uuid>` → 0 or 1); `None` for anything else.
    pub tight_sync_index: Option<u8>,
    /// `gid` exactly as advertised, before normalisation (diagnostics).
    pub raw_gid: Option<String>,
    /// `tsm`, as advertised (diagnostics; meaning unknown).
    pub tsm: Option<String>,
    /// The status `flags` word (see [`STATUS_TIGHT_SYNC_LEADER`] and
    /// friends); `None` when absent or unparseable.
    pub status_flags: Option<u64>,
    /// `features`/`ft` exactly as advertised (diagnostics).
    pub raw_features: Option<String>,
}

/// Status flag bit 11 — `DeviceSupportsRelay` (remote-control relay, not
/// audio).
pub const STATUS_SUPPORTS_RELAY: u64 = 1 << 11;
/// Status flag bit 13 — `TightSyncIsGroupLeader`: the stereo-pair half
/// that leads the pair's tight-sync group (usually the `gid` +0 half).
pub const STATUS_TIGHT_SYNC_LEADER: u64 = 1 << 13;
/// Status flag bit 14 — `TightSyncBuddyNotReachable`.
pub const STATUS_TIGHT_SYNC_BUDDY_UNREACHABLE: u64 = 1 << 14;
/// Status flag bit 17 — `ReceiverSessionIsActive`.
pub const STATUS_SESSION_ACTIVE: u64 = 1 << 17;

impl GroupHints {
    /// The key two receivers must share to be in one group: the parent
    /// group when nested, else the group itself (owntone#1413's rule).
    pub fn grouping_key(&self) -> Option<&str> {
        self.parent_group_id
            .as_deref()
            .or(self.group_id.as_deref())
    }

    /// True if this receiver advertises status flag bit 13
    /// (`TightSyncIsGroupLeader`).
    pub fn is_tight_sync_leader(&self) -> bool {
        self.status_flags.map(|f| f & STATUS_TIGHT_SYNC_LEADER != 0).unwrap_or(false)
    }

    /// True if any group key was advertised at all.
    pub fn any(&self) -> bool {
        self.group_id.is_some()
            || self.parent_group_id.is_some()
            || self.is_group_leader.is_some()
            || self.group_contains_leader.is_some()
            || self.group_name.is_some()
    }
}

/// Suffix that turns a group leader's stable id into its group row's
/// id. The leader keeps its plain id for its own (folded) row, so the
/// device and the group it leads are two different list entries.
pub const GROUP_ID_SUFFIX: &str = "#group";

/// One unit of a group: a single device, or a HomePod stereo pair
/// collapsed to its primary half (the `gid` +0 half, else the lowest MAC)
/// plus the other half (halves).
#[derive(Debug, Clone)]
pub struct PairUnit {
    pub primary: AirPlayRenderer,
    pub partners: Vec<AirPlayRenderer>,
}

impl PairUnit {
    fn single(primary: AirPlayRenderer) -> Self {
        Self { primary, partners: Vec::new() }
    }

    /// True for a stereo pair (two or more halves discovered).
    pub fn is_pair(&self) -> bool {
        !self.partners.is_empty()
    }

    /// Every half, primary first.
    pub fn halves(&self) -> Vec<AirPlayRenderer> {
        std::iter::once(self.primary.clone()).chain(self.partners.iter().cloned()).collect()
    }

    /// The pair's tight-sync leader: the half advertising status flag bit
    /// 13 (`TightSyncIsGroupLeader`), else the `gid` +0 half, else the
    /// primary. For a single device, the device.
    pub fn leader(&self) -> &AirPlayRenderer {
        let all = || std::iter::once(&self.primary).chain(self.partners.iter());
        all()
            .find(|r| r.group.is_tight_sync_leader())
            .or_else(|| all().find(|r| r.group.tight_sync_index == Some(0)))
            .unwrap_or(&self.primary)
    }

    /// The halves in session order: the leader first (the tight-sync
    /// leader is set up first), then the rest in pair order.
    pub fn ordered_halves(&self) -> Vec<AirPlayRenderer> {
        let lead = self.leader().mac_id.clone();
        let mut v = vec![self.leader().clone()];
        v.extend(self.halves().into_iter().filter(|r| r.mac_id != lead));
        v
    }

    /// What this unit streams to under `t`: every half (`both`) or the
    /// leader half alone (`leader`); a single device is itself.
    pub fn targets(&self, t: PairTargets) -> Vec<AirPlayRenderer> {
        if !self.is_pair() {
            return vec![self.primary.clone()];
        }
        match t {
            PairTargets::Both => self.ordered_halves(),
            PairTargets::Leader => vec![self.leader().clone()],
        }
    }

    /// What peers call this unit: the pair's `gpn`, else the device name.
    fn label(&self) -> String {
        if !self.partners.is_empty() {
            if let Some(n) = self.primary.group.group_name.as_deref() {
                if !n.trim().is_empty() {
                    return n.trim().to_string();
                }
            }
        }
        self.primary.friendly_name.clone()
    }
}

/// What a row stands for, and so what a click on it can stream to.
#[derive(Debug, Clone)]
pub enum RowKind {
    /// One device (a plain row, or a member row folded under a group).
    Device,
    /// A HomePod stereo pair.
    Pair(PairUnit),
    /// An Apple TV and the HomePods it claims as its audio output (see
    /// [`tv_claims`]), all speaking AirPlay 2: streams to the units (the
    /// TV's HomePods), or — with [`TvRow::Tv`] — to the TV alone.
    TvGroup { tv: AirPlayRenderer, units: Vec<PairUnit> },
    /// An Apple TV and its claimed HomePods when one of those lacks AirPlay
    /// 2: streams to the TV (the leader unit) to relay (untested).
    RelayGroup { leader: PairUnit },
}

/// One row of the AirPlay speaker list after group folding: either a
/// plain device, a group (the leader plus the members folded under it),
/// or a member hidden under a group row. Built by [`group_renderers`].
#[derive(Debug, Clone)]
pub struct AirPlayEntry {
    /// The row's device. For a group row this is the group leader, for
    /// a pair row the pair's first half.
    pub renderer: AirPlayRenderer,
    /// The other members of the group this renderer leads, sorted by
    /// name. Empty for a plain device.
    pub led_members: Vec<AirPlayRenderer>,
    /// The other half (halves) of the stereo pair this row stands for.
    /// Empty unless this row is a pair (or a group led by a pair).
    pub pair_members: Vec<AirPlayRenderer>,
    /// What the row stands for; decides the targets (see
    /// [`AirPlayEntry::targets_with`]).
    pub kind: RowKind,
    /// `Some(group row id)` when this renderer is a member that was
    /// folded under a group or pair row — hidden from the list by
    /// default.
    pub folded_under: Option<String>,
    /// Names of same-group peers when the group could NOT be collapsed
    /// (no unambiguous leader). Purely informational.
    pub ungrouped_peers: Vec<String>,
    /// True when this row's device is a stereo-pair half (its partner
    /// discovered too) playing as an Apple TV's audio output (claimed by a
    /// TV-led group). A lone claimed HomePod is not: its row is a single
    /// receiver.
    pub tv_group_member: bool,
    /// True when this row's device is one half of a stereo pair whose
    /// other half is discovered too (see
    /// [`crate::airplay::pair_experiments::is_pair_half`]). A HomePod whose
    /// `tsid` nobody else in range shares — its buddy is offline, or it
    /// advertises one on its own — is a single receiver.
    pub pair_half: bool,
}

impl AirPlayEntry {
    fn plain(renderer: AirPlayRenderer) -> Self {
        Self {
            renderer,
            led_members: Vec::new(),
            pair_members: Vec::new(),
            kind: RowKind::Device,
            folded_under: None,
            ungrouped_peers: Vec::new(),
            tv_group_member: false,
            pair_half: false,
        }
    }

    /// A device hidden under the row `under`; a click streams to it alone.
    fn folded(renderer: AirPlayRenderer, under: &str, tv_group_member: bool) -> Self {
        Self { folded_under: Some(under.to_string()), tv_group_member, ..Self::plain(renderer) }
    }

    /// The row's id in the speaker list: the device's stable id, or for
    /// a group row the leader's id plus [`GROUP_ID_SUFFIX`].
    pub fn id(&self) -> String {
        let id = self.renderer.stable_id();
        if self.is_group() {
            id + GROUP_ID_SUFFIX
        } else {
            id
        }
    }

    /// True if this row stands for a group (leader + ≥1 folded member).
    pub fn is_group(&self) -> bool {
        !self.led_members.is_empty()
    }

    /// True if this row stands for a stereo pair.
    pub fn is_pair(&self) -> bool {
        !self.pair_members.is_empty()
    }

    /// True if this row is led by an Apple TV that streams to its speakers.
    pub fn is_tv_group(&self) -> bool {
        matches!(self.kind, RowKind::TvGroup { .. })
    }

    /// True if a click on this row streams to the group leader and lets
    /// it relay to the members (an Apple TV one of whose HomePods lacks
    /// AirPlay 2).
    pub fn streams_via_leader(&self) -> bool {
        matches!(self.kind, RowKind::RelayGroup { .. })
    }

    /// Every receiver a click on this row streams to directly, session
    /// lead first, under the default settings. See [`Self::targets_with`].
    pub fn targets(&self) -> Vec<AirPlayRenderer> {
        self.targets_with(&TargetPolicy::default())
    }

    /// Every receiver a click on this row streams to directly, session
    /// lead first:
    /// * a plain or member row: its device;
    /// * a stereo pair: both halves (`both`, the tight-sync leader first)
    ///   or the leader half alone (`leader`) — a half is reported not to
    ///   forward audio to its partner (owntone#1291), so `both` is the
    ///   default;
    /// * a group led by an Apple TV: its HomePods, each pair unit like a
    ///   bare pair (`homepods`), or the TV alone (`tv`) — a TV whose
    ///   default output is HomePods is reported to play nothing from
    ///   third-party senders (owntone#1675; Rogue Amoeba,
    ///   <https://rogueamoeba.com/support/knowledgebase/?showArticle=AirfoilSatellite-AppleTVHomePods>);
    /// * an Apple TV group with a HomePod lacking AirPlay 2: the TV, which
    ///   is expected to relay (untested).
    pub fn targets_with(&self, p: &TargetPolicy) -> Vec<AirPlayRenderer> {
        match &self.kind {
            RowKind::Device => vec![self.renderer.clone()],
            RowKind::Pair(u) => u.targets(p.pair_targets),
            RowKind::TvGroup { tv, units } => match p.tv_row {
                TvRow::Tv => vec![tv.clone()],
                TvRow::Homepods => units.iter().flat_map(|u| u.targets(p.pair_targets)).collect(),
            },
            RowKind::RelayGroup { leader } => leader.targets(p.pair_targets),
        }
    }

    /// True if a session started from this row gets the pair/group
    /// settings: a pair row, an Apple-TV-led row that streams to its
    /// HomePods or to the TV, or a member row whose device is a pair half
    /// (its partner discovered too). Everything else — a relaying Apple TV
    /// row included — is a single receiver, with every pair/group rule off
    /// (see [`crate::airplay::pair_experiments`]).
    pub fn group_session(&self) -> bool {
        match &self.kind {
            RowKind::Pair(_) | RowKind::TvGroup { .. } => true,
            // Streams to the Apple TV alone (its leader is never a pair).
            RowKind::RelayGroup { .. } => false,
            RowKind::Device => self.tv_group_member || self.pair_half,
        }
    }

    /// Short description of the row for the experiment log line.
    pub fn row_kind_label(&self, p: &TargetPolicy) -> &'static str {
        match &self.kind {
            RowKind::Device if self.tv_group_member => "member row (pair half behind an Apple TV)",
            RowKind::Device if self.pair_half => "member row (pair half)",
            RowKind::Device => "single",
            RowKind::Pair(_) => "stereo pair",
            RowKind::TvGroup { .. } => match p.tv_row {
                TvRow::Homepods => "Apple TV row (HomePods)",
                TvRow::Tv => "Apple TV row (TV only)",
            },
            RowKind::RelayGroup { .. } => "Apple TV row (relayed by the TV)",
        }
    }

    /// Row label: the group's public name (`gpn`) for a group or a
    /// stereo pair, else the device's friendly name.
    pub fn display_name(&self) -> String {
        if self.is_group() || self.is_pair() {
            if let Some(n) = self.renderer.group.group_name.as_deref() {
                let n = n.trim();
                if !n.is_empty() {
                    return n.to_string();
                }
            }
        }
        self.renderer.friendly_name.clone()
    }

    /// Names of every device in the group or pair, leader / first half
    /// first.
    pub fn member_names(&self) -> Vec<String> {
        let mut v = vec![self.renderer.friendly_name.clone()];
        v.extend(self.pair_members.iter().map(|m| m.friendly_name.clone()));
        v.extend(self.led_members.iter().map(|m| m.friendly_name.clone()));
        v
    }

    /// The transport a click on this row uses under the default settings.
    pub fn transport(&self) -> Option<Transport> {
        self.transport_with(&TargetPolicy::default())
    }

    /// The transport a click on this row uses. A group streaming to its
    /// leader (relay, or an Apple TV row set to `tv`) drives it over
    /// AirPlay 2 whenever it can (see
    /// [`AirPlayRenderer::transport_as_group_leader`]); several targets
    /// are AirPlay 2 only (one lock-step session per target on a shared
    /// clock — there is no legacy equivalent); a single target uses
    /// exactly its [`AirPlayRenderer::transport`].
    pub fn transport_with(&self, p: &TargetPolicy) -> Option<Transport> {
        let targets = self.targets_with(p);
        let via_leader = matches!(self.kind, RowKind::RelayGroup { .. } | RowKind::TvGroup { .. });
        match targets.as_slice() {
            [one] if via_leader && one.mac_id == self.renderer.mac_id => {
                self.renderer.transport_as_group_leader()
            }
            [one] => one.transport(),
            many => many.iter().all(|r| r.supports_airplay2()).then_some(Transport::AirPlay2),
        }
    }
}

impl AirPlayRenderer {
    /// Stable identifier used by the UI / persistence layer. Prefixed so
    /// it can't collide with the UPnP `Renderer::stable_id()`.
    pub fn stable_id(&self) -> String {
        format!("airplay:{}", self.mac_id)
    }

    /// True if this device exposes a usable **legacy RAOP** path: a RAOP
    /// port, ALAC, no password, and either no-encryption or Apple-RSA
    /// (i.e. not a FairPlay-only `et`).
    pub fn supports_legacy_raop(&self) -> bool {
        if self.port == 0 {
            return false;
        }
        if !self.codecs.contains(&1) {
            return false;
        }
        // Password-protected (`pw=true`) receivers ARE supported — we
        // speak RTSP Digest auth (the session uses the stored password,
        // or the UI prompts for one).
        self.encryption_types.contains(&0) || self.encryption_types.contains(&1)
    }

    /// Whether to encrypt the audio with the RSA-wrapped-key path rather
    /// than stream plaintext. Reference behaviour (OwnTone/libraop):
    /// classic AirPort Express genuinely *requires* AES, while sending
    /// RSA keys to a device that only does plaintext breaks it — so we
    /// only turn it on for the receiver classes that want it:
    ///
    ///   * the device advertised `ek=1` (AirPort Express gen1, shairport —
    ///     the latter also accepts plaintext, so RSA is safe there), or
    ///   * the model is a gen2 802.11n AirPort Express (`am=AirPort4,x`;
    ///     OwnTone's `APEX2` class, which it force-encrypts).
    ///
    /// **A device advertising `et=4` (the AirPlay-2 era, incl. Sonos and
    /// the AP2 `AirPort10,x`) always takes plaintext** — matching OwnTone,
    /// which force-disables encryption for those. So `et=1` must be on
    /// offer AND `et=4` must be absent. Everything else — Apple TV,
    /// third-party `et=0,4`, and non-AirPort models — stays plaintext.
    pub fn prefers_rsa_encryption(&self) -> bool {
        if !self.encryption_types.contains(&1) || self.encryption_types.contains(&4) {
            return false;
        }
        if self.encryption_key_required {
            return true;
        }
        // OwnTone's APEX2 (gen2 802.11n) is exactly `am=AirPort4,x`; other
        // `AirPort*` models (APEX3, e.g. AirPort10,x) it streams plaintext.
        self.model
            .as_deref()
            .map(|m| m.starts_with("AirPort4"))
            .unwrap_or(false)
    }

    /// True if this device is reachable via the AirPlay-2 HomeKit path:
    /// it advertises `_airplay._tcp` audio support plus a HomeKit
    /// pairing/encryption capability we can satisfy (transient pairing).
    pub fn supports_airplay2(&self) -> bool {
        let Some(ft) = self.features else {
            return false;
        };
        if self.airplay_port.is_none() {
            return false;
        }
        if ft & FEAT_AUDIO == 0 {
            return false;
        }
        // We implement *transient* pairing, advertised by bit 48; bit 46
        // (HK pairing + access control) is the broader capability HomePods
        // and Apple TVs set. Either is sufficient for us to attempt it.
        ft & (FEAT_TRANSIENT_PAIRING | FEAT_HK_PAIRING) != 0
    }

    /// True if this device *requires* the AirPlay-2 path — i.e. it won't
    /// play via legacy RAOP. HomePods (model `AudioAccessory*`) gate all
    /// audio on HomeKit pairing even though they still advertise a RAOP
    /// service, so we route them to AP2 regardless of their `et` list.
    pub fn requires_airplay2(&self) -> bool {
        if self.is_homepod() {
            return true;
        }
        // No usable legacy path but a usable AP2 path ⇒ AP2 is required.
        self.supports_airplay2() && !self.supports_legacy_raop()
    }

    /// HomePod / HomePod mini detection by model prefix.
    pub fn is_homepod(&self) -> bool {
        self.model
            .as_deref()
            .map(|m| m.starts_with("AudioAccessory"))
            .unwrap_or(false)
    }

    /// Apple TV detection by model prefix (the openairplay subtype rule).
    pub fn is_apple_tv(&self) -> bool {
        self.model
            .as_deref()
            .map(|m| m.starts_with("AppleTV"))
            .unwrap_or(false)
    }

    /// Transport for a device that LEADS a group (an Apple TV with
    /// HomePods as its default audio output). Such a TV is reported to
    /// accept a session but play nothing on those HomePods (owntone#1675;
    /// Rogue Amoeba, for Airfoil:
    /// <https://rogueamoeba.com/support/knowledgebase/?showArticle=AirfoilSatellite-AppleTVHomePods>).
    /// Used for group rows that stream via their leader
    /// ([`AirPlayEntry::streams_via_leader`], or an Apple TV row set to
    /// `tv` — both untested; a group led by an Apple TV streams
    /// to its speakers by default): AirPlay 2 whenever the leader
    /// advertises it, else the plain per-device choice.
    pub fn transport_as_group_leader(&self) -> Option<Transport> {
        if self.supports_airplay2() {
            Some(Transport::AirPlay2)
        } else {
            self.transport()
        }
    }

    /// True if the device expects IEEE-1588 PTP timing (feature bit 41).
    /// HomePods set this; many older AP2 devices accept NTP instead.
    pub fn expects_ptp(&self) -> bool {
        self.features.map(|f| f & FEAT_PTP != 0).unwrap_or(false)
    }

    /// True if the device supports buffered audio (feature bit 40) — the
    /// TCP type-103 stream modern iOS senders use. Current Sonos firmware
    /// appears to *only* actually play this stream kind.
    pub fn supports_buffered_audio(&self) -> bool {
        self.features.map(|f| f & FEAT_BUFFERED_AUDIO != 0).unwrap_or(false)
    }

    /// The transport we'll use for this device, if any. Prefers the
    /// proven legacy RAOP path for devices that support it and don't
    /// *require* AP2 — that keeps Apple TV / Sonos / AirPort Express on
    /// the well-tested code path and reserves the AP2 path for HomePods
    /// and AP2-only receivers.
    pub fn transport(&self) -> Option<Transport> {
        if self.requires_airplay2() {
            return self.supports_airplay2().then_some(Transport::AirPlay2);
        }
        if self.supports_legacy_raop() {
            return Some(Transport::RaopLegacy);
        }
        if self.supports_airplay2() {
            return Some(Transport::AirPlay2);
        }
        None
    }

    /// True if we have *any* path to this device.
    pub fn is_supported(&self) -> bool {
        self.transport().is_some()
    }
}

// ---------------------------------------------------------------------------
// Per-service partial records. We keep RAOP and AirPlay info in separate
// maps keyed by normalised MAC, then merge on read so resolution order
// (and one service vanishing) can't corrupt the other half.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct RaopInfo {
    friendly_name: String,
    ip: Option<IpAddr>,
    port: u16,
    encryption_types: Vec<u8>,
    codecs: Vec<u8>,
    password_protected: bool,
    encryption_key_required: bool,
    model: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct AirPlayInfo {
    friendly_name: String,
    ip: Option<IpAddr>,
    port: u16,
    features: Option<u64>,
    pk: Option<String>,
    model: Option<String>,
    group: GroupHints,
}

/// Shared discovery state. Mirrors `ssdp::DiscoveryState` semantics.
#[derive(Default)]
pub struct AirPlayDiscoveryState {
    raop: Mutex<HashMap<String, RaopInfo>>,
    airplay: Mutex<HashMap<String, AirPlayInfo>>,
    /// Last TXT line logged per MAC — a record is logged at info when
    /// first resolved and again whenever its TXT changes (mdns-sd
    /// re-resolves on every TXT update), streaming or not.
    txt_logged: Mutex<HashMap<String, String>>,
}

impl AirPlayDiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot of currently-known receivers, sorted by friendly name.
    pub fn renderers(&self) -> Vec<AirPlayRenderer> {
        let raop = self.raop.lock().unwrap();
        let airplay = self.airplay.lock().unwrap();
        let mut macs: HashSet<String> = HashSet::new();
        macs.extend(raop.keys().cloned());
        macs.extend(airplay.keys().cloned());

        let mut v: Vec<AirPlayRenderer> = macs
            .into_iter()
            .filter_map(|mac| merge(&mac, raop.get(&mac), airplay.get(&mac)))
            .collect();
        v.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        v
    }

    /// The speaker list with AirPlay groups folded: one row per group
    /// (targeting the leader), members hidden under it. See
    /// [`group_renderers`].
    pub fn entries(&self) -> Vec<AirPlayEntry> {
        group_renderers(self.renderers())
    }

    /// The list row with this id ([`AirPlayEntry::id`]): a group row for
    /// a group id, else the device's own row (plain, pair, or folded).
    pub fn entry_by_id(&self, id: &str) -> Option<AirPlayEntry> {
        self.entries().into_iter().find(|e| e.id() == id)
    }

    /// Find by our public `stable_id()` (i.e. with the `airplay:` prefix).
    /// A group row id resolves to the group's leader.
    pub fn find_by_id(&self, id: &str) -> Option<AirPlayRenderer> {
        let id = id.strip_suffix(GROUP_ID_SUFFIX).unwrap_or(id);
        let mac = id.strip_prefix("airplay:")?.to_string();
        let raop = self.raop.lock().unwrap();
        let airplay = self.airplay.lock().unwrap();
        merge(&mac, raop.get(&mac), airplay.get(&mac))
    }

    fn upsert_raop(&self, mac: String, info: RaopInfo) {
        self.raop.lock().unwrap().insert(mac, info);
    }

    fn upsert_airplay(&self, mac: String, info: AirPlayInfo) {
        self.airplay.lock().unwrap().insert(mac, info);
    }

    /// Log the TXT line for `mac` if it differs from the last one.
    fn note_txt(&self, mac: &str, line: String) {
        let mut logged = self.txt_logged.lock().unwrap();
        if logged.get(mac) != Some(&line) {
            info!("{}", line);
            logged.insert(mac.to_string(), line);
        }
    }

    fn remove(&self, mac: &str, service: &str) {
        if service == RAOP_SERVICE {
            self.raop.lock().unwrap().remove(mac);
        } else {
            self.airplay.lock().unwrap().remove(mac);
        }
    }
}

/// Build a public [`AirPlayRenderer`] from whichever partial records we
/// have for one MAC. Returns None if neither half has a usable IPv4.
fn merge(mac: &str, raop: Option<&RaopInfo>, airplay: Option<&AirPlayInfo>) -> Option<AirPlayRenderer> {
    let ip = raop
        .and_then(|r| r.ip)
        .or_else(|| airplay.and_then(|a| a.ip))?;

    // Prefer the AirPlay-2 instance name (the user-set friendly name);
    // fall back to the RAOP `@`-suffix name.
    let friendly_name = airplay
        .map(|a| a.friendly_name.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| raop.map(|r| r.friendly_name.clone()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| mac.to_string());

    Some(AirPlayRenderer {
        friendly_name,
        mac_id: mac.to_string(),
        ip,
        port: raop.map(|r| r.port).unwrap_or(0),
        airplay_port: airplay.map(|a| a.port),
        encryption_types: raop.map(|r| r.encryption_types.clone()).unwrap_or_default(),
        codecs: raop.map(|r| r.codecs.clone()).unwrap_or_default(),
        password_protected: raop.map(|r| r.password_protected).unwrap_or(false),
        encryption_key_required: raop.map(|r| r.encryption_key_required).unwrap_or(false),
        features: airplay.and_then(|a| a.features),
        pk: airplay.and_then(|a| a.pk.clone()),
        model: airplay
            .and_then(|a| a.model.clone())
            .or_else(|| raop.and_then(|r| r.model.clone())),
        group: airplay.map(|a| a.group.clone()).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Group folding
// ---------------------------------------------------------------------------

/// HomePods playing as an Apple TV's audio output, keyed by MAC → the
/// grouping key of the TV that claims them. A HomePod is claimed when it
/// nests under the TV — its `pgid` is the TV's `gid` while its own `gid`
/// is not (the TV's group is its parent group, whatever it leads inside
/// it: a stereo pair's halves may say `igl=1` for their own pair group) —
/// or when its `gid` is the TV's and it does not lead that group itself
/// (`igl` ≠ 1). Both shapes appear in sources (the cyrahs/spoticonn#1
/// test fixture: the TV's gid; music-assistant/support#6430: `pgid` =
/// the TV), and a stereo pair behind a TV keeps its own `tsid` either
/// way. The pair halves are grouped into units by `tsid` afterwards.
///
/// While an iPhone plays to an Apple TV and HomePods together, the
/// HomePods are expected to advertise the same shape: receivers in a
/// sender's session take its UUID as `gid` and `pgid` with `igl=0`
/// (openairplay's Apple TV state table; the owntone#1413 multi-room
/// capture), so they share the TV's `gid`. Nothing in the TXT keys
/// would tell the two apart. So a TV claims its HomePods only when at
/// least one of them is a stereo-pair half whose partner is
/// discovered too ([`is_pair_half`]) — the home-theater pair the
/// pair/group settings are for. A TV with only lone HomePods claims none:
/// the TV and each HomePod stay plain rows, each a single receiver.
fn tv_claims(renderers: &[AirPlayRenderer]) -> HashMap<String, String> {
    let mut tvs: Vec<&AirPlayRenderer> = renderers.iter().filter(|r| r.is_apple_tv()).collect();
    tvs.sort_by(|a, b| a.mac_id.cmp(&b.mac_id));
    let mut claims: HashMap<String, String> = HashMap::new();
    for tv in tvs {
        let (Some(tv_gid), Some(tv_key)) = (tv.group.group_id.as_deref(), tv.group.grouping_key()) else {
            continue;
        };
        let claimed: Vec<&AirPlayRenderer> = renderers
            .iter()
            .filter(|r| r.is_homepod() && !r.is_apple_tv())
            .filter(|r| {
                let nested = r.group.parent_group_id.as_deref() == Some(tv_gid)
                    && r.group.group_id.as_deref() != Some(tv_gid);
                let member =
                    r.group.group_id.as_deref() == Some(tv_gid) && r.group.is_group_leader != Some(true);
                nested || member
            })
            .collect();
        if !claimed.iter().any(|r| is_pair_half(r, renderers)) {
            continue;
        }
        for r in claimed {
            claims.entry(r.mac_id.clone()).or_insert_with(|| tv_key.to_string());
        }
    }
    claims
}

/// Fold discovered receivers into list rows — but only structures that
/// persist: a HomePod stereo pair, and an Apple TV with the HomePods it
/// claims as its audio output ([`tv_claims`]: only when one of them is a
/// stereo-pair half).
///
/// Devices sharing a [`GroupHints::grouping_key`] are considered together;
/// a HomePod the TV claims joins the TV's key. Within a key, members
/// sharing a `tsid` are first collapsed into ONE unit — a HomePod stereo
/// pair — represented by its lower-indexed half (the `+0` in `gid`, else
/// the lower MAC) with the other half in `pair_members`: a pair row, plus
/// a hidden row (`folded_under`) for the partner half. When the key holds
/// exactly one Apple TV and at least one HomePod it claims, the TV and
/// those HomePods collapse into ONE group row — named after `gpn`, with
/// its own id ([`AirPlayEntry::id`]) — and every one of them also gets a
/// hidden row of its own. The hidden row of a stereo-pair half the TV
/// claims is a pair/group session ([`AirPlayEntry::tv_group_member`]); the
/// hidden row of any other claimed HomePod (claimed beside a pair) streams
/// to it as a single receiver — a HomePod in a group an iPhone built with
/// an Apple TV looks like a home-theater HomePod (see [`tv_claims`]).
/// Anything else sharing the key — the receivers of a group someone built
/// on an iPhone, which share a `gid` while it plays (a Sonos, a HomePod
/// that leads its own group, a second TV, or an Apple TV and HomePods
/// with no stereo-pair half among them) — stays a visible row of its own
/// (a pair row for a pair) that keeps its own path, noting its peers in
/// `ungrouped_peers`: no receiver is known to forward audio from another
/// sender, so a row that streamed to one of them would leave the rest
/// silent. Devices with no group key, or whose key nobody else shares
/// (every standalone AP2 receiver advertises a private `gid`), are plain
/// rows. Output is sorted by row label.
pub fn group_renderers(renderers: Vec<AirPlayRenderer>) -> Vec<AirPlayEntry> {
    let claims = tv_claims(&renderers);
    let all = renderers.clone();
    let mut by_key: HashMap<String, Vec<AirPlayRenderer>> = HashMap::new();
    let mut plain: Vec<AirPlayRenderer> = Vec::new();
    for r in renderers {
        let key = claims
            .get(&r.mac_id)
            .cloned()
            .or_else(|| r.group.grouping_key().map(str::to_string));
        match key {
            Some(k) => by_key.entry(k).or_default().push(r),
            None => plain.push(r),
        }
    }

    let mut out: Vec<AirPlayEntry> = plain.into_iter().map(AirPlayEntry::plain).collect();
    for (_, members) in by_key {
        if members.len() < 2 {
            out.extend(members.into_iter().map(AirPlayEntry::plain));
            continue;
        }
        // The one Apple TV in the key and the HomePods it claims fold;
        // everything else in the key stays its own row.
        let tv = pick_tv_leader(&members);
        let (fold, rest): (Vec<AirPlayRenderer>, Vec<AirPlayRenderer>) = match tv {
            Some(i) if members.iter().any(|m| claims.contains_key(&m.mac_id)) => {
                let tv_mac = members[i].mac_id.clone();
                members.into_iter().partition(|m| m.mac_id == tv_mac || claims.contains_key(&m.mac_id))
            }
            _ => (Vec::new(), members),
        };
        let mut peers: Vec<String> = Vec::new();
        if !fold.is_empty() {
            let (tvs, claimed): (Vec<AirPlayRenderer>, Vec<AirPlayRenderer>) =
                fold.into_iter().partition(|m| m.is_apple_tv());
            let tv = tvs.into_iter().next().expect("the fold holds its Apple TV");
            let mut units = pair_units(claimed);
            units.sort_by(|a, b| a.primary.friendly_name.cmp(&b.primary.friendly_name));
            let group_id = tv.stable_id() + GROUP_ID_SUFFIX;
            // The group row streams to the TV's HomePods themselves (see
            // `targets_with`) as long as every one of their halves speaks
            // AirPlay 2; otherwise to the TV, to relay (untested).
            let kind = if units.iter().flat_map(|u| u.halves()).all(|r| r.supports_airplay2()) {
                RowKind::TvGroup { tv: tv.clone(), units: units.clone() }
            } else {
                RowKind::RelayGroup { leader: PairUnit::single(tv.clone()) }
            };
            // Every device, the TV and pair halves included, also gets its
            // own row under the group; a click on it streams to that
            // device alone.
            let mut led: Vec<AirPlayRenderer> = units.iter().flat_map(|u| u.halves()).collect();
            led.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
            // Only a claimed pair half's own row is a pair/group session: a
            // lone claimed HomePod may be in an iPhone-built group rather
            // than a home theater, and keeps the single-receiver path.
            out.extend(std::iter::once(&tv).chain(&led).map(|m| {
                let tv_member = m.is_homepod() && claims.contains_key(&m.mac_id) && is_pair_half(m, &all);
                AirPlayEntry::folded(m.clone(), &group_id, tv_member)
            }));
            let row = AirPlayEntry { led_members: led, kind, ..AirPlayEntry::plain(tv) };
            peers.push(row.display_name());
            out.push(row);
        }
        if rest.is_empty() {
            continue;
        }
        // No fold for these: each unit is its own row (a pair row for a
        // pair), noting the key's other units as peers.
        let units = pair_units(rest);
        let names: Vec<String> = units.iter().map(|u| u.label()).collect();
        for (i, u) in units.into_iter().enumerate() {
            let unit_peers = peers
                .iter()
                .cloned()
                .chain(names.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, n)| n.clone()))
                .collect();
            let primary_id = u.primary.stable_id();
            out.extend(u.partners.iter().map(|p| AirPlayEntry::folded(p.clone(), &primary_id, false)));
            let kind = if u.is_pair() { RowKind::Pair(u.clone()) } else { RowKind::Device };
            out.push(AirPlayEntry {
                pair_members: u.partners.clone(),
                ungrouped_peers: unit_peers,
                kind,
                ..AirPlayEntry::plain(u.primary)
            });
        }
    }
    for e in &mut out {
        e.pair_half = is_pair_half(&e.renderer, &all);
    }
    out.sort_by(|a, b| {
        a.display_name()
            .cmp(&b.display_name())
            .then_with(|| a.renderer.mac_id.cmp(&b.renderer.mac_id))
    });
    out
}

/// Collapse the members of one group into units: devices sharing a
/// `tsid` (a HomePod stereo pair — both halves advertise the same `tsid`,
/// and standalone `gid = <tsid>+<0|1>+<uuid>`) become one unit whose
/// primary is the `+0` half (else the lowest MAC — deterministic, so the
/// row id is stable across scans). Everything else is a unit of one.
fn pair_units(members: Vec<AirPlayRenderer>) -> Vec<PairUnit> {
    let mut by_tsid: HashMap<String, Vec<AirPlayRenderer>> = HashMap::new();
    let mut singles: Vec<AirPlayRenderer> = Vec::new();
    for m in members {
        match m.group.tight_sync_id.clone() {
            Some(t) => by_tsid.entry(t).or_default().push(m),
            None => singles.push(m),
        }
    }
    let mut units: Vec<PairUnit> = singles.into_iter().map(PairUnit::single).collect();
    for (_, mut halves) in by_tsid {
        if halves.len() < 2 {
            units.extend(halves.into_iter().map(PairUnit::single));
            continue;
        }
        halves.sort_by(|a, b| {
            pair_half_order(a)
                .cmp(&pair_half_order(b))
                .then_with(|| a.mac_id.cmp(&b.mac_id))
        });
        let primary = halves.remove(0);
        units.push(PairUnit { primary, partners: halves });
    }
    units
}

/// Sort key for the halves of a pair: the `gid` index (`+0` before
/// `+1`), unknown last.
fn pair_half_order(r: &AirPlayRenderer) -> u8 {
    r.group.tight_sync_index.unwrap_or(u8::MAX)
}

/// Index of the one Apple TV among `members` — the home-theater case
/// (HomePods set as the TV's default output) — whichever way its `igl`
/// currently reads (it drops to `igl=0` while receiving AirPlay from
/// someone else). `None` with no Apple TV, or several. An `igl=1` speaker
/// does not lead: a group without an Apple TV is one someone built on an
/// iPhone, which no speaker in it is known to relay for another sender.
pub fn pick_tv_leader(members: &[AirPlayRenderer]) -> Option<usize> {
    let mut tvs = members.iter().enumerate().filter(|(_, m)| m.is_apple_tv());
    let first = tvs.next()?;
    tvs.next().is_none().then_some(first.0)
}

/// Spawn the long-running mDNS browser. Browses **both** `_raop._tcp` and
/// `_airplay._tcp` on a shared daemon; a consumer thread per service keeps
/// the corresponding map fresh.
///
/// `iface_hint` is informational only — `mdns-sd` listens on all
/// interfaces and picks the right one per outgoing query.
pub fn spawn_airplay_discovery(
    state: Arc<AirPlayDiscoveryState>,
    iface_hint: Option<Ipv4Addr>,
) -> Result<()> {
    let daemon = ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon init: {}", e))?;

    info!(
        "AirPlay discovery: browsing {} + {} (iface hint {:?})",
        RAOP_SERVICE, AIRPLAY_SERVICE, iface_hint,
    );

    for service in [RAOP_SERVICE, AIRPLAY_SERVICE] {
        let receiver = daemon
            .browse(service)
            .map_err(|e| anyhow::anyhow!("mdns browse {}: {}", service, e))?;
        let state = state.clone();
        // Each consumer holds its own clone of the daemon handle so the
        // background daemon thread stays alive as long as either consumer
        // is running.
        let daemon = daemon.clone();
        thread::Builder::new()
            .name(format!("stream-to-speaker-airplay-mdns:{}", service))
            .spawn(move || {
                let _daemon_keepalive = daemon;
                loop {
                    match receiver.recv_timeout(Duration::from_secs(60)) {
                        Ok(ServiceEvent::ServiceResolved(info)) => {
                            ingest_resolution(&state, service, &info);
                        }
                        Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                            if let Some(mac) = mac_from_event(service, &fullname) {
                                debug!("AirPlay removed ({}): {}", service, fullname);
                                state.remove(&mac, service);
                            }
                        }
                        Ok(_) => { /* SearchStarted / ServiceFound / etc. */ }
                        Err(flume::RecvTimeoutError::Timeout) => {}
                        Err(flume::RecvTimeoutError::Disconnected) => {
                            warn!("AirPlay mDNS daemon disconnected ({}); discovery stops", service);
                            return;
                        }
                    }
                }
            })?;
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Resolution ingestion
// -----------------------------------------------------------------------------

/// TXT record as plain key → value pairs (keys as advertised). Lookups
/// are case-insensitive, as mdns-sd's own accessors are: receivers spell
/// keys inconsistently (`deviceid`/`deviceID`, `features`/`Features`).
type TxtMap = HashMap<String, String>;

fn txt_map(txt: &TxtProperties) -> TxtMap {
    txt.iter()
        .map(|p| (p.key().to_string(), p.val_str().to_string()))
        .collect()
}

fn ingest_resolution(state: &AirPlayDiscoveryState, service: &str, info: &ServiceInfo) {
    let ip = first_v4(info);
    let txt = txt_map(info.get_properties());

    if service == RAOP_SERVICE {
        // Service name: "<MAC>@<FriendlyName>._raop._tcp.local."
        let Some((mac, friendly)) = split_raop_name(info.get_fullname()) else {
            debug!("AirPlay RAOP: can't parse name {}", info.get_fullname());
            return;
        };
        let r = parse_raop_txt(friendly, ip, info.get_port(), &txt);
        debug!(
            "AirPlay RAOP resolved: {} @ {:?}:{} et={:?} cn={:?}",
            r.friendly_name, r.ip, r.port, r.encryption_types, r.codecs
        );
        state.upsert_raop(mac, r);
    } else {
        // _airplay._tcp: MAC is in the `deviceid` TXT key; the service
        // instance name is the friendly name.
        let Some(mac) = read_txt_string(&txt, "deviceid").map(|s| normalise_mac(&s)) else {
            debug!("AirPlay v2: no deviceid in {}", info.get_fullname());
            return;
        };
        let a = parse_airplay_txt(airplay_instance_name(info.get_fullname()), ip, info.get_port(), &txt);
        state.note_txt(&mac, txt_log_line(&a.friendly_name, ip, &txt));
        debug!(
            "AirPlay v2 resolved: {} @ {:?}:{} ft={:?} model={:?} gid={:?} pgid={:?} igl={:?} gcgl={:?} gpn={:?}",
            a.friendly_name,
            a.ip,
            a.port,
            a.features,
            a.model,
            a.group.group_id,
            a.group.parent_group_id,
            a.group.is_group_leader,
            a.group.group_contains_leader,
            a.group.group_name,
        );
        state.upsert_airplay(mac, a);
    }
}

fn parse_raop_txt(friendly_name: String, ip: Option<IpAddr>, port: u16, txt: &TxtMap) -> RaopInfo {
    RaopInfo {
        friendly_name,
        ip,
        port,
        encryption_types: read_txt_int_list(txt, "et"),
        codecs: read_txt_int_list(txt, "cn"),
        password_protected: read_txt_bool(txt, "pw").unwrap_or(false),
        encryption_key_required: read_txt_bool(txt, "ek").unwrap_or(false),
        model: read_txt_string(txt, "am"),
    }
}

fn parse_airplay_txt(friendly_name: String, ip: Option<IpAddr>, port: u16, txt: &TxtMap) -> AirPlayInfo {
    AirPlayInfo {
        friendly_name,
        ip,
        port,
        features: read_features(txt),
        pk: read_txt_string(txt, "pk"),
        model: read_txt_string(txt, "model"),
        group: parse_group_hints(txt),
    }
}

/// Read the group keys (see [`GroupHints`]).
fn parse_group_hints(txt: &TxtMap) -> GroupHints {
    GroupHints {
        group_id: read_txt_string(txt, "gid").and_then(|s| normalise_group_id(&s)),
        parent_group_id: read_txt_string(txt, "pgid").and_then(|s| normalise_group_id(&s)),
        // Apple spells it `igl`; Sonos' AP2 stack spells it `isGroupLeader`.
        is_group_leader: read_txt_bool(txt, "igl").or_else(|| read_txt_bool(txt, "isGroupLeader")),
        group_contains_leader: read_txt_bool(txt, "gcgl"),
        parent_group_contains_leader: read_txt_bool(txt, "pgcgl"),
        group_name: read_txt_string(txt, "gpn")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        tight_sync_id: read_txt_string(txt, "tsid")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty()),
        tight_sync_index: read_txt_string(txt, "gid").and_then(|s| pair_index_from_gid(&s)),
        raw_gid: read_txt_string(txt, "gid"),
        tsm: read_txt_string(txt, "tsm"),
        status_flags: read_txt_string(txt, "flags").and_then(|s| parse_flags(&s)),
        raw_features: read_txt_string(txt, "features").or_else(|| read_txt_string(txt, "ft")),
    }
}

/// Parse the status `flags` word: `0x9a404` (hex, the usual spelling) or
/// plain decimal.
fn parse_flags(raw: &str) -> Option<u64> {
    let s = raw.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// The TXT log line for one `_airplay._tcp` record: the raw group keys,
/// the status flags with the pair-relevant bits spelled out, and the raw
/// features — no interpretation (in particular no L/R guess).
fn txt_log_line(name: &str, ip: Option<IpAddr>, txt: &TxtMap) -> String {
    let get = |k: &str| read_txt_string(txt, k).unwrap_or_default();
    let flags_raw = get("flags");
    let bits = parse_flags(&flags_raw)
        .map(|f| {
            [11u32, 12, 13, 14, 17]
                .iter()
                .filter(|b| f & (1u64 << **b) != 0)
                .map(|b| format!("b{b}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let features = read_txt_string(txt, "features").or_else(|| read_txt_string(txt, "ft")).unwrap_or_default();
    format!(
        "AP2 TXT {} {}: gid={} pgid={} tsid={} tsm={} igl={} gcgl={} pgcgl={} gpn={} psgid={} psgtp={} psgsz={} flags={} [{}] features={} osvers={} model={}",
        name,
        ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
        get("gid"),
        get("pgid"),
        get("tsid"),
        get("tsm"),
        read_txt_string(txt, "igl").or_else(|| read_txt_string(txt, "isGroupLeader")).unwrap_or_default(),
        get("gcgl"),
        get("pgcgl"),
        get("gpn"),
        get("psgid"),
        get("psgtp"),
        get("psgsz"),
        flags_raw,
        bits,
        features,
        get("osvers"),
        get("model"),
    )
}

/// The half index of a stereo pair's `gid` (`<tsid>+<index>+<uuid>`);
/// `None` when the `gid` has no such suffix.
fn pair_index_from_gid(raw: &str) -> Option<u8> {
    let mut parts = raw.trim().split('+');
    parts.next()?;
    parts.next()?.trim().parse().ok()
}

/// Normalise a `gid`/`pgid` so records from different devices compare:
/// lower-case, trimmed, and cut at the first `+` (a HomePod stereo pair
/// advertises `<tsid>+0+<uuid>` on one half and `<tsid>+1+<uuid>` on the
/// other). Empty and all-zero ids mean "no group".
fn normalise_group_id(raw: &str) -> Option<String> {
    let s = raw.trim().split('+').next().unwrap_or("").trim().to_ascii_lowercase();
    if s.is_empty() || s.chars().all(|c| c == '0' || c == '-') {
        return None;
    }
    Some(s)
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn first_v4(info: &ServiceInfo) -> Option<IpAddr> {
    info.get_addresses().iter().find_map(|a| match a {
        IpAddr::V4(v4) => Some(IpAddr::V4(*v4)),
        _ => None,
    })
}

/// Normalise a MAC to upper-case hex with no separators so the `deviceid`
/// form (`B8:E9:37:35:AC:11`) and the RAOP-name form (`B8E93735AC11`)
/// correlate.
fn normalise_mac(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Split `<MAC>@<FriendlyName>._raop._tcp.local.` → (normalised MAC, name).
fn split_raop_name(fullname: &str) -> Option<(String, String)> {
    let trimmed = fullname
        .strip_suffix(&format!(".{}", RAOP_SERVICE))
        .or_else(|| fullname.strip_suffix(RAOP_SERVICE))
        .unwrap_or(fullname);
    let (mac, friendly) = trimmed.split_once('@')?;
    let mac = normalise_mac(mac);
    let friendly = friendly.trim().to_string();
    if mac.is_empty() || friendly.is_empty() {
        return None;
    }
    Some((mac, friendly))
}

/// Extract the instance (friendly) name from an `_airplay._tcp` fullname.
fn airplay_instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(&format!(".{}", AIRPLAY_SERVICE))
        .or_else(|| fullname.strip_suffix(AIRPLAY_SERVICE))
        .unwrap_or(fullname)
        .trim_end_matches('.')
        .to_string()
}

/// On removal we only get the fullname; recover the MAC so we can evict
/// the right map entry. For RAOP it's in the name; for `_airplay._tcp` the
/// name is just the friendly name (no MAC), so we can't reliably evict by
/// MAC — return None and let the stale entry age out / be overwritten.
fn mac_from_event(service: &str, fullname: &str) -> Option<String> {
    if service == RAOP_SERVICE {
        Some(split_raop_name(fullname)?.0)
    } else {
        None
    }
}

fn read_txt_string(txt: &TxtMap, key: &str) -> Option<String> {
    txt.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

/// `Some(true)` for `true`/`1`/`yes`, `Some(false)` for any other value,
/// `None` when the key is absent (absence is meaningful for the group
/// hints, which a record may lack).
fn read_txt_bool(txt: &TxtMap, key: &str) -> Option<bool> {
    read_txt_string(txt, key).map(|s| {
        let s = s.trim().to_ascii_lowercase();
        s == "true" || s == "1" || s == "yes"
    })
}

/// Parse a TXT field whose value is a comma-separated integer list
/// (`cn=0,1,3` → `[0, 1, 3]`). Returns an empty vec on absence.
fn read_txt_int_list(txt: &TxtMap, key: &str) -> Vec<u8> {
    match read_txt_string(txt, key) {
        Some(s) => s
            .split(',')
            .filter_map(|p| p.trim().parse::<u8>().ok())
            .collect(),
        None => Vec::new(),
    }
}

/// Parse the 64-bit `features` / `ft` flag word. AirPlay advertises it
/// either as a single value (`0x445F8A00`) or — very commonly — as two
/// comma-separated 32-bit halves, low first: `0x445F8A00,0x1C340`
/// → `(high << 32) | low`. Decimal is also accepted.
fn read_features(txt: &TxtMap) -> Option<u64> {
    let raw = read_txt_string(txt, "features").or_else(|| read_txt_string(txt, "ft"))?;
    let parts: Vec<&str> = raw.split(',').map(|p| p.trim()).collect();
    let parse = |s: &str| -> Option<u64> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).ok()
        } else {
            s.parse::<u64>().ok().or_else(|| u64::from_str_radix(s, 16).ok())
        }
    };
    match parts.as_slice() {
        [single] => parse(single),
        [low, high, ..] => {
            let low = parse(low)?;
            let high = parse(high)?;
            Some((high << 32) | (low & 0xFFFF_FFFF))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer(model: Option<&str>, et: Vec<u8>, ft: Option<u64>, raop_port: u16, ap_port: Option<u16>) -> AirPlayRenderer {
        AirPlayRenderer {
            friendly_name: "Test".into(),
            mac_id: "AABBCCDDEEFF".into(),
            ip: "192.168.1.5".parse().unwrap(),
            port: raop_port,
            airplay_port: ap_port,
            encryption_types: et,
            codecs: vec![1],
            password_protected: false,
            encryption_key_required: false,
            features: ft,
            pk: None,
            model: model.map(|s| s.to_string()),
            group: GroupHints::default(),
        }
    }

    #[test]
    fn normalise_mac_strips_separators_and_uppercases() {
        assert_eq!(normalise_mac("b8:e9:37:35:ac:11"), "B8E93735AC11");
        assert_eq!(normalise_mac("B8E93735AC11"), "B8E93735AC11");
    }

    #[test]
    fn features_single_and_split_hex() {
        // single
        let f = parse_features_str("0x40000200");
        assert_eq!(f, Some(0x40000200));
        // split low,high
        let f = parse_features_str("0x445F8A00,0x1C340");
        assert_eq!(f, Some((0x1C340u64 << 32) | 0x445F8A00));
    }

    // Tiny shim so the test can exercise the same parse logic without a
    // TxtProperties (which we can't easily synthesise here).
    fn parse_features_str(raw: &str) -> Option<u64> {
        let parts: Vec<&str> = raw.split(',').map(|p| p.trim()).collect();
        let parse = |s: &str| -> Option<u64> {
            if let Some(hex) = s.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).ok()
            } else {
                s.parse().ok()
            }
        };
        match parts.as_slice() {
            [single] => parse(single),
            [low, high, ..] => Some((parse(high)? << 32) | (parse(low)? & 0xFFFF_FFFF)),
            _ => None,
        }
    }


    // ------------------------------------------------------------------
    // Group folding — realistic TXT sets. Sources: openairplay
    // service_discovery.md (Apple TV state table + _raop example), the
    // HomePod stereo-pair / multi-room captures on owntone#1413, the
    // Apple-TV-with-HomePod-default-output fixture in cyrahs/spoticonn#1, and
    // OwnTone airplay.c's TXT examples (Sonos, Marantz, Apple TV 4).
    // ------------------------------------------------------------------

    fn txt(pairs: &[(&str, &str)]) -> TxtMap {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn txt_lookups_ignore_key_case() {
        // mdns-sd matched keys case-insensitively; a receiver that
        // advertises mixed-case keys must still be resolved and grouped.
        let t = txt(&[("deviceID", "AA:BB:CC:DD:EE:FF"), ("Features", "0x445F8A00,0x1C340"),
                      ("gid", "ABCDEF01-2222-3333-4444-555555555555"), ("isGroupLeader", "1"),
                      ("GPN", "Kitchen")]);
        assert_eq!(read_txt_string(&t, "deviceid").as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert!(read_features(&t).is_some());
        let g = parse_group_hints(&t);
        assert_eq!(g.is_group_leader, Some(true));
        assert_eq!(g.group_name.as_deref(), Some("Kitchen"));
    }

    /// Build a merged renderer from an `_airplay._tcp` TXT set (plus an
    /// optional `_raop._tcp` one), the way discovery does.
    fn from_txt(
        name: &str,
        mac: &str,
        ip: &str,
        airplay: Option<&[(&str, &str)]>,
        raop: Option<&[(&str, &str)]>,
    ) -> AirPlayRenderer {
        let ip: IpAddr = ip.parse().unwrap();
        let a = airplay.map(|t| parse_airplay_txt(name.to_string(), Some(ip), 7000, &txt(t)));
        let r = raop.map(|t| parse_raop_txt(name.to_string(), Some(ip), 7000, &txt(t)));
        merge(&normalise_mac(mac), r.as_ref(), a.as_ref()).expect("has ip")
    }

    const TV_GID: &str = "FA6CE6B6-16B6-557C-B2D2-451F3C68D932";
    const TV_FEATURES: &str = "0x4A7FDFD5,0x3C155FDE"; // AppleTV6,2, pyatv capture
    const HOMEPOD_FEATURES: &str = "0x4A7FCA00,0xBC354BD0"; // owntone#1413 capture

    fn apple_tv_raop() -> Vec<(&'static str, &'static str)> {
        vec![
            ("txtvers", "1"), ("ch", "2"), ("cn", "0,1,2,3"), ("da", "true"), ("et", "0,3,5"),
            ("md", "0,1,2"), ("pw", "false"), ("sv", "false"), ("sr", "44100"), ("ss", "16"),
            ("tp", "UDP"), ("vn", "65537"), ("vs", "550.10"), ("am", "AppleTV6,2"), ("sf", "0x4"),
        ]
    }

    /// Apple TV, idle, leading its default-output group (spec: `igl=1
    /// gcgl=1`; cyrahs/spoticonn#1 fixture: `gpn` shared with the
    /// HomePods).
    fn apple_tv_leader() -> AirPlayRenderer {
        from_txt(
            "Living Room",
            "06:03:16:5E:17:B1",
            "192.0.2.10",
            Some(&[
                ("acl", "0"), ("deviceid", "06:03:16:5E:17:B1"), ("features", TV_FEATURES),
                ("flags", "0x244"), ("gid", TV_GID), ("igl", "1"), ("gcgl", "1"),
                ("gpn", "Living Room"), ("model", "AppleTV6,2"), ("protovers", "1.1"),
                ("pi", "de7562c4-7bd2-4005-a8e4-d584bf63161a"), ("pk", "aa"), ("srcvers", "550.10"),
                ("osvers", "14.7"), ("vv", "2"),
            ]),
            Some(&apple_tv_raop()),
        )
    }

    /// A HomePod set as that Apple TV's default audio output: shares the
    /// TV's `gid` + `gpn`, `igl=0 gcgl=1`.
    fn homepod_member(name: &str, mac: &str, ip: &str) -> AirPlayRenderer {
        from_txt(
            name,
            mac,
            ip,
            Some(&[
                ("acl", "0"), ("deviceid", mac), ("features", HOMEPOD_FEATURES), ("flags", "0xb8c04"),
                ("gid", TV_GID), ("igl", "0"), ("gcgl", "1"), ("gpn", "Living Room"),
                ("model", "AudioAccessory5,1"), ("protovers", "1.1"), ("pk", "bb"),
                ("srcvers", "710.68.3"), ("osvers", "17.4"), ("vv", "2"),
            ]),
            Some(&[
                ("cn", "0,1,2,3"), ("et", "0,3,5"), ("am", "AudioAccessory5,1"), ("tp", "UDP"),
                ("pw", "false"), ("sf", "0x8c04"), ("vs", "710.68.3"),
            ]),
        )
    }

    /// One half of a HomePod stereo pair set as that Apple TV's default
    /// audio output: [`homepod_member`] plus the pair's shared `tsid`.
    fn homepod_pair_member(name: &str, mac: &str, ip: &str) -> AirPlayRenderer {
        let mut r = homepod_member(name, mac, ip);
        r.group.tight_sync_id = Some("22222222-3333-4444-5555-666666666666".into());
        r
    }

    /// The device's own row (plain, pair or folded) — never a group row.
    fn find<'a>(entries: &'a [AirPlayEntry], name: &str) -> &'a AirPlayEntry {
        entries
            .iter()
            .find(|e| e.renderer.friendly_name == name && !e.is_group())
            .unwrap_or_else(|| panic!("no entry named {name}"))
    }

    /// The group row led by the device named `leader`.
    fn find_group<'a>(entries: &'a [AirPlayEntry], leader: &str) -> &'a AirPlayEntry {
        entries
            .iter()
            .find(|e| e.renderer.friendly_name == leader && e.is_group())
            .unwrap_or_else(|| panic!("no group led by {leader}"))
    }

    fn names(rs: &[AirPlayRenderer]) -> Vec<String> {
        rs.iter().map(|r| r.friendly_name.clone()).collect()
    }

    #[test]
    fn group_hints_parse_and_normalise() {
        let g = parse_group_hints(&txt(&[
            ("gid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+0+6276FFFA-04E1-439E-8139-2C906B34E587"),
            ("igl", "1"), ("gcgl", "1"), ("gpn", "Büro 2"),
            ("tsid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C"),
        ]));
        assert_eq!(g.group_id.as_deref(), Some("eafa36aa-9785-54b2-a537-d9ee2a55cf1c"));
        assert_eq!(g.is_group_leader, Some(true));
        assert_eq!(g.group_contains_leader, Some(true));
        assert_eq!(g.group_name.as_deref(), Some("Büro 2"));
        assert_eq!(g.tight_sync_id.as_deref(), Some("eafa36aa-9785-54b2-a537-d9ee2a55cf1c"));
        assert_eq!(g.parent_group_id, None);
        assert_eq!(g.grouping_key(), Some("eafa36aa-9785-54b2-a537-d9ee2a55cf1c"));

        // Sonos spells the leader key `isGroupLeader`; pgid wins as key.
        let g = parse_group_hints(&txt(&[
            ("isGroupLeader", "0"), ("gcgl", "0"), ("gid", "A918B6A2-BB3F-4A50-A422-0BB043C9F3BF"),
            ("pgid", "0BB043C9-F3BF-4A50-A422-A918B6A2BB3F"), ("pgcgl", "0"),
        ]));
        assert_eq!(g.is_group_leader, Some(false));
        assert_eq!(g.grouping_key(), Some("0bb043c9-f3bf-4a50-a422-a918b6a2bb3f"));
        assert_eq!(g.parent_group_contains_leader, Some(false));

        // Absent keys stay None (absence ≠ false); zero/empty gid = no group.
        let g = parse_group_hints(&txt(&[("gid", "00000000-0000-0000-0000-000000000000")]));
        assert_eq!(g.group_id, None);
        assert_eq!(g.is_group_leader, None);
        assert!(!g.any());
        let g = parse_group_hints(&txt(&[("gid", "  ")]));
        assert_eq!(g.group_id, None);
    }

    #[test]
    fn apple_tv_leader_with_a_homepod_pair_folds_into_one_row() {
        let tv = apple_tv_leader();
        let left = homepod_pair_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        let right = homepod_pair_member("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12");
        // Each device on its own keeps its own transport.
        assert_eq!(tv.transport(), Some(Transport::RaopLegacy));
        assert_eq!(left.transport(), Some(Transport::AirPlay2));

        let entries = group_renderers(vec![left.clone(), tv.clone(), right.clone()]);
        assert_eq!(entries.len(), 4);
        let group = find_group(&entries, "Living Room");
        assert_eq!(group.display_name(), "Living Room");
        assert_eq!(
            group.member_names(),
            vec!["Living Room", "Living Room L", "Living Room R"]
        );
        assert_eq!(group.renderer.stable_id(), tv.stable_id());
        let group_id = format!("{}{}", tv.stable_id(), GROUP_ID_SUFFIX);
        assert_eq!(group.id(), group_id);
        // The group streams to its HomePods, not to the TV (reported to
        // play nothing from third-party senders while they are its audio
        // output: owntone#1675, Rogue Amoeba's Airfoil Satellite KB).
        assert!(!group.streams_via_leader());
        assert_eq!(names(&group.targets()), vec!["Living Room L", "Living Room R"]);
        assert_eq!(group.transport(), Some(Transport::AirPlay2));
        // Every device, the TV included, has its own hidden row.
        for name in ["Living Room", "Living Room L", "Living Room R"] {
            let m = find(&entries, name);
            assert_eq!(m.folded_under.as_deref(), Some(group_id.as_str()));
            assert!(!m.is_group());
            assert!(m.ungrouped_peers.is_empty());
            assert_eq!(m.id(), m.renderer.stable_id());
            assert_eq!(names(&m.targets()), vec![name]);
        }
        assert_eq!(find(&entries, "Living Room").transport(), Some(Transport::RaopLegacy));
        assert_eq!(find(&entries, "Living Room L").transport(), Some(Transport::AirPlay2));
        // Visible rows (what the list shows by default) = the group only.
        let visible: Vec<_> = entries.iter().filter(|e| e.folded_under.is_none()).collect();
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn apple_tv_group_with_a_pair_and_a_single_homepod_streams_to_both_units() {
        // Halves behind a TV by gid, assumed to advertise igl=0 (the
        // grouped HomePod mini in music-assistant/support#6430 does, with
        // pgid = the Apple TV).
        let nest = |mut r: AirPlayRenderer| {
            r.group.group_id = Some(TV_GID.to_ascii_lowercase());
            r.group.is_group_leader = Some(false);
            r
        };
        let tv = apple_tv_leader();
        let kitchen = homepod_member("Kitchen", "E0:2B:96:96:FB:77", "192.0.2.50");
        let entries = group_renderers(vec![nest(links()), tv.clone(), kitchen, nest(rechts())]);
        let group = find_group(&entries, "Living Room");
        // Default (targets=both, tv-row=homepods): one session per HomePod
        // — the single one and BOTH halves of the pair (a half does not
        // forward to its partner); never the TV.
        assert_eq!(names(&group.targets()), vec!["Kitchen", "Links", "Rechts"]);
        let leader_only = TargetPolicy { pair_targets: PairTargets::Leader, ..TargetPolicy::default() };
        assert_eq!(names(&group.targets_with(&leader_only)), vec!["Kitchen", "Links"]);
        let tv_only = TargetPolicy { tv_row: TvRow::Tv, ..TargetPolicy::default() };
        assert_eq!(names(&group.targets_with(&tv_only)), vec!["Living Room"]);
        assert_eq!(group.transport_with(&tv_only), Some(Transport::AirPlay2));
        assert!(group.group_session());
        assert!(!group.streams_via_leader());
        assert_eq!(group.transport(), Some(Transport::AirPlay2));
        // Row ids are unique: the group row and the TV's own row differ.
        let mut ids: Vec<String> = entries.iter().map(|e| e.id()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate row ids");
    }

    #[test]
    fn apple_tv_group_with_a_non_airplay2_member_streams_via_the_tv() {
        let tv = apple_tv_leader();
        let pod = homepod_pair_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        let mut old = homepod_pair_member("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12");
        old.features = None;
        assert!(!old.supports_airplay2());
        let entries = group_renderers(vec![pod, tv.clone(), old]);
        let group = find_group(&entries, "Living Room");
        assert!(group.streams_via_leader());
        assert_eq!(names(&group.targets()), vec!["Living Room"]);
        assert_eq!(group.transport(), tv.transport_as_group_leader());
    }

    #[test]
    fn apple_tv_group_streaming_to_one_homepod_uses_that_homepods_transport() {
        let tv = apple_tv_leader();
        let pod = homepod_pair_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        let partner = homepod_pair_member("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12");
        let entries = group_renderers(vec![pod.clone(), tv, partner]);
        let group = find_group(&entries, "Living Room");
        let leader_only = TargetPolicy { pair_targets: PairTargets::Leader, ..TargetPolicy::default() };
        assert_eq!(names(&group.targets_with(&leader_only)), vec!["Living Room L"]);
        assert_eq!(group.transport_with(&leader_only), pod.transport());
    }

    #[test]
    fn state_resolves_group_ids_and_drops_the_row_when_the_group_dissolves() {
        let state = AirPlayDiscoveryState::default();
        let add = |name: &str, mac: &str, ip: &str, ap: &[(&str, &str)], raop: Option<&[(&str, &str)]>| {
            let ip: IpAddr = ip.parse().unwrap();
            let mac = normalise_mac(mac);
            state.upsert_airplay(mac.clone(), parse_airplay_txt(name.to_string(), Some(ip), 7000, &txt(ap)));
            if let Some(r) = raop {
                state.upsert_raop(mac, parse_raop_txt(name.to_string(), Some(ip), 7000, &txt(r)));
            }
        };
        let tv_txt = [
            ("deviceid", "06:03:16:5E:17:B1"), ("features", TV_FEATURES), ("gid", TV_GID), ("igl", "1"),
            ("gcgl", "1"), ("gpn", "Living Room"), ("model", "AppleTV6,2"), ("pk", "aa"),
        ];
        let pod_txt = |mac: &'static str| {
            [
                ("deviceid", mac), ("features", HOMEPOD_FEATURES), ("gid", TV_GID), ("igl", "0"),
                ("gcgl", "1"), ("gpn", "Living Room"), ("model", "AudioAccessory5,1"), ("pk", "bb"),
                ("tsid", "22222222-3333-4444-5555-666666666666"),
            ]
        };
        add("Living Room", "06:03:16:5E:17:B1", "192.0.2.10", &tv_txt, Some(&apple_tv_raop()));
        add("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11", &pod_txt("6A:9C:DD:77:21:C7"), None);
        add("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12", &pod_txt("6A:9C:DD:77:21:C8"), None);

        let tv_id = format!("airplay:{}", normalise_mac("06:03:16:5E:17:B1"));
        let group_id = format!("{tv_id}{GROUP_ID_SUFFIX}");
        let group = state.entry_by_id(&group_id).expect("group row");
        assert!(group.is_group());
        assert_eq!(names(&group.targets()), vec!["Living Room L", "Living Room R"]);
        let tv_row = state.entry_by_id(&tv_id).expect("TV row");
        assert!(!tv_row.is_group());
        assert_eq!(tv_row.folded_under.as_deref(), Some(group_id.as_str()));
        assert_eq!(state.find_by_id(&group_id).map(|r| r.mac_id), state.find_by_id(&tv_id).map(|r| r.mac_id));

        // One half leaves: the other is no longer a pair half, the group
        // can't fold, so its id resolves to no row (callers must not fall
        // back to the TV) and the TV's own row is visible again.
        state.remove(&normalise_mac("6A:9C:DD:77:21:C8"), AIRPLAY_SERVICE);
        assert!(state.entry_by_id(&group_id).is_none());
        assert!(state.entry_by_id(&tv_id).expect("TV row").folded_under.is_none());
    }

    #[test]
    fn apple_tv_receiving_from_another_sender_still_leads_its_homepods() {
        // Spec "Apple TV receiving AirPlay audio": igl=0 gcgl=0, gid and
        // pgid = the sender's session group. The HomePods (a stereo pair,
        // sharing a tsid) follow suit.
        const SESSION: &str = "19F5D4B2-8A06-4792-923E-8AFA83913238";
        let tv = from_txt(
            "Living Room",
            "06:03:16:5E:17:B1",
            "192.0.2.10",
            Some(&[
                ("deviceid", "06:03:16:5E:17:B1"), ("features", TV_FEATURES), ("flags", "0x30e44"),
                ("gid", SESSION), ("igl", "0"), ("gcgl", "0"), ("pgid", SESSION), ("pgcgl", "0"),
                ("model", "AppleTV6,2"), ("pk", "aa"), ("srcvers", "550.10"),
            ]),
            Some(&apple_tv_raop()),
        );
        let pod = |name: &str, mac: &'static str, ip: &str| {
            from_txt(
                name,
                mac,
                ip,
                Some(&[
                    ("deviceid", mac), ("features", HOMEPOD_FEATURES),
                    ("gid", SESSION), ("igl", "0"), ("gcgl", "0"), ("pgid", SESSION), ("pgcgl", "0"),
                    ("model", "AudioAccessory5,1"), ("pk", "bb"),
                    ("tsid", "22222222-3333-4444-5555-666666666666"),
                ]),
                None,
            )
        };
        let entries = group_renderers(vec![
            pod("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11"),
            tv.clone(),
            pod("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12"),
        ]);
        let group = find_group(&entries, "Living Room");
        assert_eq!(group.renderer.mac_id, tv.mac_id, "the Apple TV leads regardless of its momentary igl");
        // No gpn → falls back to the leader's name.
        assert_eq!(group.display_name(), "Living Room");
    }

    /// Based on the owntone#1413 pair shape (pk shortened, other keys
    /// omitted): both halves advertise igl=1 gcgl=1,
    /// gid = "<tsid>+<0|1>+<uuid>", the same gpn and tsid.
    fn stereo_half(name: &str, mac: &str, ip: &str, gid: &str, flags: &str) -> AirPlayRenderer {
        from_txt(
            name,
            mac,
            ip,
            Some(&[
                ("deviceid", mac), ("features", HOMEPOD_FEATURES), ("flags", flags),
                ("gid", gid), ("igl", "1"), ("gcgl", "1"), ("gpn", "Büro 2"), ("tsm", "0"),
                ("tsid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C"), ("model", "AudioAccessory1,1"),
                ("pk", "cc"), ("srcvers", "530.6"), ("acl", "0"),
            ]),
            Some(&[("cn", "0,1,2,3"), ("et", "0,3,5"), ("am", "AudioAccessory1,1"), ("tp", "UDP")]),
        )
    }

    const LINKS_GID: &str = "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+0+6276FFFA-04E1-439E-8139-2C906B34E587";
    const RECHTS_GID: &str = "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+1+4671EEC3-7E13-4DC7-BEC3-C9805D3AB964";

    fn links() -> AirPlayRenderer {
        stereo_half("Links", "D4:A3:3D:7A:28:D8", "192.168.178.46", LINKS_GID, "0x9a404")
    }

    fn rechts() -> AirPlayRenderer {
        stereo_half("Rechts", "50:BC:96:07:E8:6D", "192.168.178.47", RECHTS_GID, "0x98404")
    }

    #[test]
    fn stereo_pair_gid_index_parses() {
        assert_eq!(links().group.tight_sync_index, Some(0));
        assert_eq!(rechts().group.tight_sync_index, Some(1));
        assert_eq!(pair_index_from_gid(TV_GID), None);
        assert_eq!(pair_index_from_gid("abc+x+def"), None);
        assert_eq!(apple_tv_leader().group.tight_sync_index, None);
    }

    #[test]
    fn lone_homepod_stereo_pair_folds_into_one_pair_row() {
        let (links, rechts) = (links(), rechts());
        assert_eq!(links.group.grouping_key(), rechts.group.grouping_key());
        // Input order must not matter: the +0 half is the row.
        for order in [vec![links.clone(), rechts.clone()], vec![rechts.clone(), links.clone()]] {
            let entries = group_renderers(order);
            assert_eq!(entries.len(), 2);
            let pair = find(&entries, "Links");
            assert!(pair.is_pair());
            assert!(!pair.is_group());
            assert!(pair.folded_under.is_none());
            assert!(pair.ungrouped_peers.is_empty());
            assert_eq!(pair.display_name(), "Büro 2");
            assert_eq!(pair.member_names(), vec!["Links", "Rechts"]);
            assert_eq!(pair.renderer.stable_id(), links.stable_id());
            assert_eq!(pair.pair_members.len(), 1);
            assert_eq!(pair.pair_members[0].stable_id(), rechts.stable_id());
            assert_eq!(pair.transport(), Some(Transport::AirPlay2));
            // Default: one session per half, the tight-sync leader (flags
            // bit 13, here also the +0 half) first; `leader` = that half.
            assert_eq!(names(&pair.targets()), vec!["Links", "Rechts"]);
            let leader_only = TargetPolicy { pair_targets: PairTargets::Leader, ..TargetPolicy::default() };
            assert_eq!(names(&pair.targets_with(&leader_only)), vec!["Links"]);
            assert_eq!(pair.transport_with(&leader_only), Some(Transport::AirPlay2));
            assert!(pair.group_session());
            assert_eq!(pair.id(), links.stable_id());
            // The partner half is a hidden row under the pair.
            let other = find(&entries, "Rechts");
            assert_eq!(other.folded_under.as_deref(), Some(links.stable_id().as_str()));
            assert!(!other.is_pair());
            assert_eq!(other.display_name(), "Rechts");
            let visible: Vec<_> = entries.iter().filter(|e| e.folded_under.is_none()).collect();
            assert_eq!(visible.len(), 1);
        }
    }

    #[test]
    fn stereo_pair_without_gid_index_still_pairs_by_lowest_mac() {
        let mut a = links();
        let mut b = rechts();
        a.group.tight_sync_index = None;
        b.group.tight_sync_index = None;
        // MACs: 50:BC… (Rechts) < D4:A3… (Links).
        let entries = group_renderers(vec![a, b]);
        let pair = entries.iter().find(|e| e.is_pair()).expect("a pair row");
        assert_eq!(pair.renderer.friendly_name, "Rechts");
        assert_eq!(pair.pair_members[0].friendly_name, "Links");
    }

    #[test]
    fn stereo_pair_under_an_apple_tv_folds_under_the_tv_as_before() {
        // The pair is the TV's default output: both halves carry the TV's
        // gid as pgid (nested), keep their own tsid pair gid.
        let nest = |mut r: AirPlayRenderer| {
            r.group.parent_group_id = Some(TV_GID.to_ascii_lowercase());
            r.group.parent_group_contains_leader = Some(true);
            r
        };
        let tv = apple_tv_leader();
        let entries = group_renderers(vec![nest(links()), tv.clone(), nest(rechts())]);
        assert_eq!(entries.len(), 4);
        let group = find_group(&entries, "Living Room");
        assert!(!group.is_pair(), "the TV is not itself a pair");
        assert_eq!(group.member_names(), vec!["Living Room", "Links", "Rechts"]);
        assert_eq!(group.transport(), Some(Transport::AirPlay2));
        // Both halves by default — not the TV.
        assert_eq!(names(&group.targets()), vec!["Links", "Rechts"]);
        assert!(group.is_tv_group());
        for name in ["Living Room", "Links", "Rechts"] {
            let m = find(&entries, name);
            assert_eq!(m.folded_under.as_deref(), Some(group.id().as_str()));
            assert!(!m.is_pair());
        }
    }

    #[test]
    fn stereo_pair_beside_another_leader_claimant_stays_a_pair_row() {
        // A user-built multi-room group: the pair plus a lone HomePod that
        // also says igl=1 → two claimants, no fold; the pair stays ONE row.
        let mut solo = homepod_member("Küche", "E0:2B:96:96:FB:77", "192.168.178.50");
        solo.group.group_id = links().group.group_id.clone();
        solo.group.is_group_leader = Some(true);
        let entries = group_renderers(vec![links(), solo.clone(), rechts()]);
        assert_eq!(entries.len(), 3);
        let pair = find(&entries, "Links");
        assert!(pair.is_pair());
        assert_eq!(pair.ungrouped_peers, vec!["Küche".to_string()]);
        let k = find(&entries, "Küche");
        assert!(!k.is_pair() && !k.is_group());
        assert!(k.folded_under.is_none());
        assert_eq!(k.ungrouped_peers, vec!["Büro 2".to_string()], "peers name the pair by gpn");
        assert_eq!(find(&entries, "Rechts").folded_under.as_deref(), Some(links().stable_id().as_str()));
    }

    #[test]
    fn stereo_pair_with_a_speaker_joined_to_it_stays_a_pair_row_beside_that_speaker() {
        // The pair (igl=1) with a Sonos (isGroupLeader=0) sharing its gid —
        // a group built on an iPhone. The pair is the one persistent
        // structure: a pair row driving both halves. The Sonos is not
        // folded under it (a HomePod is reported not to forward another
        // sender's audio, owntone#1291): it stays visible on its own path,
        // naming its peer.
        let mut sonos = homepod_member("Basement", "34:7E:5C:31:D9:96", "192.0.2.41");
        sonos.model = Some("Bookshelf".into());
        sonos.group.group_id = links().group.group_id.clone();
        sonos.group.is_group_leader = Some(false);
        let entries = group_renderers(vec![sonos.clone(), links(), rechts()]);
        assert!(entries.iter().all(|e| !e.is_group()), "no group row");
        let pair = find(&entries, "Links");
        assert!(pair.is_pair());
        assert!(pair.folded_under.is_none());
        assert_eq!(pair.display_name(), "Büro 2");
        assert_eq!(names(&pair.targets()), vec!["Links", "Rechts"]);
        assert!(pair.group_session());
        assert_eq!(pair.ungrouped_peers, vec!["Basement".to_string()]);
        let b = find(&entries, "Basement");
        assert!(b.folded_under.is_none(), "the Sonos stays visible");
        assert!(!b.group_session());
        assert_eq!(names(&b.targets()), vec!["Basement"]);
        assert_eq!(b.transport(), sonos.transport(), "its own path, not AirPlay 2 via the pair");
        assert_eq!(b.ungrouped_peers, vec!["Büro 2".to_string()]);
        assert_eq!(find(&entries, "Rechts").folded_under.as_deref(), Some(links().stable_id().as_str()));
    }

    #[test]
    fn member_with_gid_but_no_leader_in_range_is_a_plain_row() {
        let pod = homepod_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        assert_eq!(pod.group.group_contains_leader, Some(true));
        let entries = group_renderers(vec![pod.clone()]);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(!e.is_group());
        assert!(e.folded_under.is_none());
        assert!(e.ungrouped_peers.is_empty());
        assert_eq!(e.display_name(), "Living Room L");
        assert_eq!(e.transport(), pod.transport());
    }

    #[test]
    fn standalone_receivers_with_private_gids_are_not_grouped() {
        // Every standalone AP2 receiver advertises its own gid (Sonos:
        // gid = pi, gcgl=0). Distinct gids → distinct plain rows, each
        // with the device's own transport.
        let sonos = from_txt(
            "Kitchen",
            "11:22:33:44:55:66",
            "192.0.2.20",
            Some(&[
                ("pk", "e5"), ("gcgl", "0"), ("gid", "0fd9c3b0-1111-2222-3333-444455556666"),
                ("pi", "0fd9c3b0-1111-2222-3333-444455556666"), ("srcvers", "366.0"),
                ("protovers", "1.1"), ("manufacturer", "Sonos"), ("model", "Bookshelf"),
                ("flags", "0x4"), ("features", "0x445F8A00,0x1C340"),
                ("deviceid", "11:22:33:44:55:66"), ("acl", "0"),
            ]),
            Some(&[("et", "0,4"), ("cn", "0,1"), ("sf", "0x4"), ("tp", "UDP"), ("am", "Bookshelf")]),
        );
        let tv = apple_tv_leader(); // idle TV, gid nobody shares
        let marantz = from_txt(
            "Marantz",
            "00:06:12:12:12:12",
            "192.0.2.30",
            Some(&[
                ("srcvers", "190.9.p6"), ("model", "NR1607"), ("flags", "0x4"),
                ("features", "0x444F8A00"), ("deviceid", "00:06:12:12:12:12"),
            ]),
            Some(&[("et", "0,1"), ("cn", "0,1"), ("tp", "UDP")]),
        );
        assert!(!marantz.group.any());
        let entries = group_renderers(vec![sonos.clone(), tv.clone(), marantz.clone()]);
        assert_eq!(entries.len(), 3);
        for (r, name) in [(&sonos, "Kitchen"), (&tv, "Living Room"), (&marantz, "Marantz")] {
            let e = find(&entries, name);
            assert!(!e.is_group());
            assert!(e.folded_under.is_none());
            assert!(e.ungrouped_peers.is_empty());
            assert_eq!(e.transport(), r.transport(), "{name} keeps today's transport");
        }
        // A lone Apple TV still takes the proven legacy path.
        assert_eq!(find(&entries, "Living Room").transport(), Some(Transport::RaopLegacy));
    }

    /// A group built on an iPhone (owntone#1413 multi-room shape): one
    /// igl=1 HomePod and a Sonos carrying `isGroupLeader=0`, one gid.
    fn iphone_group_homepod_and_sonos(g: &str) -> (AirPlayRenderer, AirPlayRenderer) {
        let homepod = from_txt(
            "Büro",
            "E0:2B:96:96:FB:77",
            "192.0.2.40",
            Some(&[
                ("deviceid", "E0:2B:96:96:FB:77"), ("features", HOMEPOD_FEATURES), ("gid", g),
                ("igl", "1"), ("gcgl", "1"), ("gpn", "Büro"), ("model", "AudioAccessory5,1"), ("pk", "dd"),
            ]),
            None,
        );
        let sonos = from_txt(
            "Basement",
            "34:7E:5C:31:D9:96",
            "192.0.2.41",
            Some(&[
                ("deviceid", "34:7E:5C:31:D9:96"), ("features", "0x445F8A00,0x1C340"), ("gid", g),
                ("isGroupLeader", "0"), ("gcgl", "0"), ("model", "Bookshelf"), ("pk", "ee"),
            ]),
            Some(&[("et", "0,4"), ("cn", "0,1")]),
        );
        (homepod, sonos)
    }

    #[test]
    fn a_group_built_on_an_iphone_is_not_folded() {
        // No Apple TV: the igl=1 HomePod does not lead a row that would
        // stream to it alone and hide the Sonos. Both stay visible, each
        // on the path it has as a single receiver, noting the other.
        const G: &str = "A918B6A2-BB3F-4A50-A422-0BB043C9F3BF";
        let (homepod, sonos) = iphone_group_homepod_and_sonos(G);
        let entries = group_renderers(vec![sonos.clone(), homepod.clone()]);
        assert_eq!(entries.len(), 2);
        for (r, peer) in [(&homepod, "Basement"), (&sonos, "Büro")] {
            let e = find(&entries, &r.friendly_name);
            assert!(!e.is_group() && e.folded_under.is_none());
            assert!(!e.group_session());
            assert_eq!(names(&e.targets()), vec![r.friendly_name.clone()]);
            assert_eq!(e.transport(), r.transport());
            assert_eq!(e.ungrouped_peers, vec![peer.to_string()]);
        }
        assert_eq!(find(&entries, "Basement").transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn an_iphone_group_with_an_apple_tv_and_a_sonos_is_not_a_tv_row() {
        // The iPhone plays to its Apple TV and a Sonos: both carry the
        // session's gid (the TV as gid = pgid). The TV claims no HomePod,
        // so nothing folds: the Sonos is not handed the pair recipe over
        // AirPlay 2 only, and neither row is hidden.
        const SESSION: &str = "19F5D4B2-8A06-4792-923E-8AFA83913238";
        let tv = from_txt(
            "Living Room",
            "06:03:16:5E:17:B1",
            "192.0.2.10",
            Some(&[
                ("deviceid", "06:03:16:5E:17:B1"), ("features", TV_FEATURES), ("flags", "0x30e44"),
                ("gid", SESSION), ("igl", "0"), ("gcgl", "0"), ("pgid", SESSION), ("pgcgl", "0"),
                ("model", "AppleTV6,2"), ("pk", "aa"), ("srcvers", "550.10"),
            ]),
            Some(&apple_tv_raop()),
        );
        let (_, sonos) = iphone_group_homepod_and_sonos(SESSION);
        assert!(sonos.supports_airplay2(), "every unit speaks AirPlay 2 — the old TvGroup condition");
        let entries = group_renderers(vec![sonos.clone(), tv.clone()]);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| !e.is_group() && !e.is_tv_group() && e.folded_under.is_none()));
        let b = find(&entries, "Basement");
        assert!(!b.group_session());
        assert_eq!(b.transport(), sonos.transport());
        assert_eq!(b.ungrouped_peers, vec!["Living Room".to_string()]);
        assert_eq!(find(&entries, "Living Room").transport(), tv.transport());
    }

    #[test]
    fn a_speaker_sharing_a_tv_rows_gid_stays_its_own_row() {
        // The TV's HomePod pair folds under the TV row; a Sonos that shares
        // the TV's gid (the iPhone added it to the TV's session) does not.
        let tv = apple_tv_leader();
        let left = homepod_pair_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        let right = homepod_pair_member("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12");
        let (_, mut sonos) = iphone_group_homepod_and_sonos(TV_GID);
        sonos.group.group_id = tv.group.group_id.clone();
        let entries = group_renderers(vec![left, sonos.clone(), tv.clone(), right]);
        let group = find_group(&entries, "Living Room");
        assert!(group.is_tv_group());
        assert_eq!(group.member_names(), vec!["Living Room", "Living Room L", "Living Room R"]);
        assert_eq!(names(&group.targets()), vec!["Living Room L", "Living Room R"]);
        let b = find(&entries, "Basement");
        assert!(b.folded_under.is_none(), "visible");
        assert!(!b.group_session() && !b.tv_group_member);
        assert_eq!(b.transport(), sonos.transport());
        assert_eq!(b.ungrouped_peers, vec!["Living Room".to_string()]);
    }

    /// Based on the nested capture in owntone#1413 (ma-ku, 2022-02-02;
    /// `pk` shortened, `psi`/`pi`/`btaddr` omitted): a
    /// HomePod pair ("Büro 2") inside a user-built multi-room group with a
    /// HomePod mini and a Sonos. Every member has igl=0, gid = pgid = the
    /// group; the pair halves keep their tsid; only Links has flags bit 13.
    fn owntone_1413_nested() -> Vec<AirPlayRenderer> {
        const G: &str = "A918B6A2-BB3F-4A50-A422-0BB043C9F3BF";
        const TSID: &str = "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C";
        let pair_half = |name: &str, mac: &str, ip: &str, flags: &str| {
            from_txt(
                name,
                mac,
                ip,
                Some(&[
                    ("vv", "2"), ("osvers", "15.4"), ("srcvers", "610.14.1"), ("pk", "40e4"),
                    ("protovers", "1.1"), ("model", "AudioAccessory1,1"), ("tsm", "0"), ("tsid", TSID),
                    ("pgcgl", "0"), ("pgid", G), ("gpn", "Büro 2"), ("gcgl", "0"), ("igl", "0"),
                    ("gid", G), ("flags", flags), ("features", HOMEPOD_FEATURES),
                    ("fex", "AMp/StBLNbw"), ("deviceid", mac), ("acl", "0"),
                ]),
                None,
            )
        };
        let links = pair_half("Links", "D4:A3:3D:7A:28:D8", "192.168.178.46", "0xbac04");
        let rechts = pair_half("Rechts", "50:BC:96:07:E8:6D", "192.168.178.56", "0xb8c04");
        let mini = from_txt(
            "B__rok__che",
            "E0:2B:96:96:FB:77",
            "192.168.178.49",
            Some(&[
                ("osvers", "15.4"), ("model", "AudioAccessory5,1"), ("pgcgl", "0"), ("pgid", G),
                ("gcgl", "0"), ("igl", "0"), ("gid", G), ("flags", "0xb8c04"),
                ("features", "0x4A7FCA00,0xBC356BD0"), ("deviceid", "E0:2B:96:96:FB:77"), ("pk", "ec12"),
            ]),
            None,
        );
        let sonos = from_txt(
            "Basement",
            "34:7E:5C:31:D9:96",
            "192.168.178.91",
            Some(&[
                ("pk", "ffdf"), ("isGroupLeader", "0"), ("gcgl", "0"), ("gid", G), ("srcvers", "366.0"),
                ("manufacturer", "Sonos"), ("model", "Bookshelf"), ("flags", "0x804"),
                ("features", "0x445F8A00,0x1C340"), ("deviceid", "34:7E:5C:31:D9:96"),
            ]),
            None,
        );
        vec![links, rechts, mini, sonos]
    }

    #[test]
    fn owntone_1413_pair_in_a_multiroom_group_is_one_pair_row_led_by_its_bit13_half() {
        let all = owntone_1413_nested();
        let links = &all[0];
        assert!(links.group.is_tight_sync_leader(), "0xbac04 has bit 13");
        assert!(!all[1].group.is_tight_sync_leader(), "0xb8c04 has not");
        assert_eq!(links.group.raw_gid.as_deref(), Some("A918B6A2-BB3F-4A50-A422-0BB043C9F3BF"));
        assert_eq!(links.group.tsm.as_deref(), Some("0"));
        assert_eq!(links.group.status_flags, Some(0xbac04));
        assert_eq!(links.group.parent_group_contains_leader, Some(false));
        assert_eq!(links.group.raw_features.as_deref(), Some(HOMEPOD_FEATURES));
        // No +N+ in the gid here: no half index.
        assert_eq!(links.group.tight_sync_index, None);

        let entries = group_renderers(all.clone());
        // No leader claimant (everyone igl=0, no Apple TV): three rows —
        // the pair, the mini, the Sonos — plus the pair's hidden half.
        let visible: Vec<_> = entries.iter().filter(|e| e.folded_under.is_none()).collect();
        assert_eq!(visible.len(), 3);
        let pair = entries.iter().find(|e| e.is_pair()).expect("pair row");
        assert_eq!(pair.display_name(), "Büro 2");
        // Primary = lowest MAC (no gid index) …
        assert_eq!(pair.renderer.friendly_name, "Rechts");
        // … but the tight-sync leader (bit 13) is set up first / alone.
        assert_eq!(names(&pair.targets()), vec!["Links", "Rechts"]);
        let leader_only = TargetPolicy { pair_targets: PairTargets::Leader, ..TargetPolicy::default() };
        assert_eq!(names(&pair.targets_with(&leader_only)), vec!["Links"]);
        assert!(pair.group_session());
        assert_eq!(pair.ungrouped_peers.len(), 2);
        // The mini and the Sonos stay single receivers.
        let mini = find(&entries, "B__rok__che");
        assert!(!mini.group_session());
        assert_eq!(names(&mini.targets()), vec!["B__rok__che"]);
        assert!(!find(&entries, "Basement").group_session());
        // The hidden half's own row is a pair half → pair/group settings.
        let hidden = find(&entries, "Links");
        assert_eq!(hidden.folded_under.as_deref(), Some(pair.id().as_str()));
        assert!(hidden.group_session());
    }

    /// A HomePod pair as an Apple TV's default output, halves carrying the
    /// TV's gid in `gid` while `pgid` names something else (so the old
    /// "pgid wins" grouping key alone would have split them from the TV).
    fn tv_pair_half(name: &str, mac: &str, ip: &str, flags: &str, pgid: Option<&str>, gid: &str) -> AirPlayRenderer {
        let mut txt: Vec<(&str, &str)> = vec![
            ("deviceid", mac), ("features", HOMEPOD_FEATURES), ("flags", flags), ("gid", gid),
            ("igl", "0"), ("gcgl", "1"), ("gpn", "Living Room"), ("model", "AudioAccessory5,1"),
            ("pk", "bb"), ("tsid", "11111111-2222-3333-4444-555555555555"), ("tsm", "0"),
        ];
        if let Some(p) = pgid {
            txt.push(("pgid", p));
        }
        from_txt(name, mac, ip, Some(&txt), None)
    }

    #[test]
    fn tv_led_group_claims_homepods_by_gid_or_pgid_and_pairs_them_by_tsid() {
        const OTHER: &str = "99999999-8888-7777-6666-555555555555";
        let tv = apple_tv_leader();
        // Shape 1: gid = TV gid, pgid = something else.
        // Bit 13 set on B (0x1a2c04) only.
        let a = tv_pair_half("Pair A", "6A:9C:DD:77:21:C7", "192.0.2.11", "0x3a0c04", Some(OTHER), TV_GID);
        let b = tv_pair_half("Pair B", "6A:9C:DD:77:21:C8", "192.0.2.12", "0x1a2c04", Some(OTHER), TV_GID);
        assert_ne!(a.group.grouping_key(), tv.group.grouping_key(), "the old key alone splits them");
        let entries = group_renderers(vec![a.clone(), tv.clone(), b.clone()]);
        let group = find_group(&entries, "Living Room");
        assert!(group.is_tv_group());
        assert_eq!(group.member_names(), vec!["Living Room", "Pair A", "Pair B"]);
        // Both halves, the bit-13 half (B) first even though A is the
        // pair's primary (lower MAC; no gid index behind a TV).
        assert!(b.group.is_tight_sync_leader() && !a.group.is_tight_sync_leader());
        assert_eq!(names(&group.targets()), vec!["Pair B", "Pair A"]);
        let leader_only = TargetPolicy { pair_targets: PairTargets::Leader, ..TargetPolicy::default() };
        assert_eq!(names(&group.targets_with(&leader_only)), vec!["Pair B"]);
        let tv_only = TargetPolicy { tv_row: TvRow::Tv, ..TargetPolicy::default() };
        assert_eq!(names(&group.targets_with(&tv_only)), vec!["Living Room"]);
        assert_eq!(group.row_kind_label(&tv_only), "Apple TV row (TV only)");
        assert!(group.group_session());
        // Member rows: the halves are pair/group sessions, the TV is not.
        assert!(find(&entries, "Pair A").group_session());
        assert!(find(&entries, "Pair A").tv_group_member);
        assert!(!find(&entries, "Living Room").group_session());

        // Shape 2: pgid = TV gid (music-assistant/support#6430), own gid.
        let c = tv_pair_half("Pair A", "6A:9C:DD:77:21:C7", "192.0.2.11", "0x3a0c04", Some(TV_GID), OTHER);
        let d = tv_pair_half("Pair B", "6A:9C:DD:77:21:C8", "192.0.2.12", "0x1a2c04", Some(TV_GID), OTHER);
        let entries = group_renderers(vec![c, tv.clone(), d]);
        let group = find_group(&entries, "Living Room");
        assert_eq!(names(&group.targets()), vec!["Pair B", "Pair A"]);

        // A HomePod that leads its own group (igl=1) is never claimed,
        // even with the TV's gid (and the TV's pair claimed beside it).
        let mut leader_pod = homepod_member("Leader", "E0:2B:96:96:FB:78", "192.0.2.51");
        leader_pod.group.is_group_leader = Some(true);
        leader_pod.group.parent_group_id = Some("unrelated".into());
        let claims = tv_claims(&[tv.clone(), a.clone(), b.clone(), leader_pod.clone()]);
        assert!(claims.contains_key(&a.mac_id) && claims.contains_key(&b.mac_id));
        assert!(!claims.contains_key(&leader_pod.mac_id));

        // A HomePod whose gid/pgid match nothing of the TV's is not
        // claimed.
        let mut stray = homepod_member("Stray", "E0:2B:96:96:FB:77", "192.0.2.50");
        stray.group.group_id = Some(OTHER.to_ascii_lowercase());
        stray.group.parent_group_id = None;
        let entries = group_renderers(vec![stray.clone(), a, tv.clone(), b]);
        let s = find(&entries, "Stray");
        assert!(s.folded_under.is_none(), "not folded under the TV row");
        assert!(!s.group_session(), "a lone HomePod stays single");
    }

    #[test]
    fn a_homepod_leading_the_tvs_group_is_not_claimed_even_with_the_tv_as_parent() {
        // The flat shape: a HomePod leads its own session (igl=1, gid = G)
        // and the Apple TV joined it, both advertising gid = pgid = G.
        // `pgid` = the TV's gid is not nesting when the HomePod's own gid
        // is the TV's too, and it leads, so it is not claimed.
        let tv = apple_tv_leader();
        let mut pod = homepod_member("Kitchen", "E0:2B:96:96:FB:77", "192.0.2.50");
        pod.group.is_group_leader = Some(true);
        pod.group.parent_group_id = tv.group.group_id.clone();
        assert_eq!(pod.group.group_id, tv.group.group_id);
        assert!(tv_claims(&[tv.clone(), pod.clone()]).is_empty());
        // Not even beside a pair the TV does claim.
        let left = homepod_pair_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11");
        let right = homepod_pair_member("Living Room R", "6A:9C:DD:77:21:C8", "192.0.2.12");
        let claims = tv_claims(&[tv.clone(), pod.clone(), left.clone(), right]);
        assert!(claims.contains_key(&left.mac_id) && !claims.contains_key(&pod.mac_id));
        let entries = group_renderers(vec![pod.clone(), tv.clone()]);
        assert!(entries.iter().all(|e| !e.is_group() && !e.is_tv_group()), "nothing folds");
        let kitchen = find(&entries, "Kitchen");
        assert!(kitchen.folded_under.is_none(), "visible");
        assert!(!kitchen.group_session() && !kitchen.tv_group_member);
        assert_eq!(names(&kitchen.targets()), vec!["Kitchen"]);
        assert!(!find(&entries, "Living Room").group_session());
    }

    #[test]
    fn a_lone_homepod_beside_a_tv_is_not_claimed_and_both_rows_stay_visible() {
        // An iPhone playing to an Apple TV and a lone HomePod: both are
        // expected to advertise igl=0, gid = pgid = the iPhone's session
        // (the shape of the owntone#1413 multi-room capture and the
        // openairplay Apple TV state table) — the same TXT shape as a home
        // theater. With no stereo-pair half among them the TV claims
        // nothing: under the default settings (members not shown
        // individually) both rows stay visible, each a single receiver on
        // its own transport.
        const SESSION: &str = "19F5D4B2-8A06-4792-923E-8AFA83913238";
        let session_txt = |mac: &'static str, features: &'static str, model: &'static str| {
            vec![
                ("deviceid", mac), ("features", features), ("gid", SESSION), ("igl", "0"), ("gcgl", "0"),
                ("pgid", SESSION), ("pgcgl", "0"), ("model", model), ("pk", "aa"),
            ]
        };
        let tv = from_txt(
            "Living Room",
            "06:03:16:5E:17:B1",
            "192.0.2.10",
            Some(&session_txt("06:03:16:5E:17:B1", TV_FEATURES, "AppleTV6,2")),
            Some(&apple_tv_raop()),
        );
        let pod = from_txt(
            "Kitchen",
            "E0:2B:96:96:FB:77",
            "192.0.2.50",
            Some(&session_txt("E0:2B:96:96:FB:77", HOMEPOD_FEATURES, "AudioAccessory5,1")),
            None,
        );
        // The home-theater shape (the TV's gid, igl=0) with one HomePod.
        let home_theater = vec![
            apple_tv_leader(),
            homepod_member("Living Room L", "6A:9C:DD:77:21:C7", "192.0.2.11"),
        ];
        let policy = TargetPolicy::default();
        for all in [vec![pod.clone(), tv.clone()], home_theater] {
            assert!(tv_claims(&all).is_empty());
            let entries = group_renderers(all.clone());
            assert_eq!(entries.len(), 2);
            for r in &all {
                let e = find(&entries, &r.friendly_name);
                assert!(e.folded_under.is_none(), "{} is visible", r.friendly_name);
                assert!(!e.is_group() && !e.is_tv_group() && !e.tv_group_member);
                assert!(!e.group_session(), "{} is a single receiver", r.friendly_name);
                assert_eq!(e.row_kind_label(&policy), "single");
                assert_eq!(names(&e.targets_with(&policy)), vec![r.friendly_name.clone()]);
                assert_eq!(e.transport_with(&policy), r.transport());
            }
        }
        // The TV's row streams to the TV over its own transport.
        let entries = group_renderers(vec![pod, tv.clone()]);
        assert_eq!(find(&entries, "Living Room").transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn single_receivers_keep_single_targets_and_are_not_group_sessions() {
        // A lone HomePod, a Sonos, a lone Apple TV: one target each, no
        // pair/group settings, whatever the policy says.
        let pod = renderer(Some("AudioAccessory5,1"), vec![0], Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING | FEAT_PTP), 7000, Some(7000));
        let mut sonos = pod.clone();
        sonos.mac_id = "112233445566".into();
        sonos.friendly_name = "Sonos".into();
        sonos.model = Some("One".into());
        let mut tv = apple_tv_leader();
        tv.group = GroupHints::default();
        let all_policies = [
            TargetPolicy::default(),
            TargetPolicy { pair_targets: PairTargets::Leader, tv_row: TvRow::Tv },
        ];
        for e in group_renderers(vec![pod.clone(), sonos.clone(), tv.clone()]) {
            assert!(!e.group_session(), "{} must stay single", e.renderer.friendly_name);
            assert_eq!(e.row_kind_label(&TargetPolicy::default()), "single");
            for p in &all_policies {
                assert_eq!(names(&e.targets_with(p)), vec![e.renderer.friendly_name.clone()]);
                assert_eq!(e.transport_with(p), e.renderer.transport());
            }
        }
    }

    #[test]
    fn txt_log_line_is_raw_and_spells_out_pair_bits() {
        let t = txt(&[
            ("gid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+0+6276FFFA-04E1-439E-8139-2C906B34E587"),
            ("tsid", "EAFA36AA-9785-54B2-A537-D9EE2A55CF1C"), ("tsm", "0"), ("igl", "1"), ("gcgl", "1"),
            ("gpn", "Büro 2"), ("flags", "0x9a404"), ("features", "0x4A7FCA00,0xBC354BD0"),
            ("osvers", "15.4"), ("model", "AudioAccessory1,1"), ("fex", "AMp/StBLNbw"),
        ]);
        let line = txt_log_line("Links", Some("192.0.2.46".parse().unwrap()), &t);
        assert!(line.starts_with("AP2 TXT Links 192.0.2.46: gid=EAFA36AA-9785-54B2-A537-D9EE2A55CF1C+0+"), "{line}");
        // 0x9a404: bits 2, 10, 13, 15, 16, 19 → of the listed ones only b13.
        assert!(line.contains("flags=0x9a404 [b13]"), "{line}");
        assert!(line.contains("features=0x4A7FCA00,0xBC354BD0"), "{line}");
        assert!(!line.contains("fex"), "fex is not a left/right marker and is not logged as one");
        assert_eq!(parse_flags("0x3a2c04"), Some(0x3a2c04));
        assert_eq!(parse_flags("2048"), Some(2048));
        assert_eq!(parse_flags("zz"), None);
    }

    #[test]
    fn only_a_lone_apple_tv_leads() {
        let a = renderer(Some("AudioAccessory5,1"), vec![0], Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING), 0, Some(7000));
        let mut b = a.clone();
        b.mac_id = "AABBCCDDEE00".into();
        b.friendly_name = "Other".into();
        let mut members = vec![a, b];
        for m in &mut members {
            m.group.group_id = Some("g".into());
        }
        // igl=1 does not make a speaker lead.
        members[0].group.is_group_leader = Some(true);
        assert_eq!(pick_tv_leader(&members), None);
        members[1].model = Some("AppleTV14,1".into());
        assert_eq!(pick_tv_leader(&members), Some(1), "the Apple TV leads, whatever its igl");
        members[0].model = Some("AppleTV6,2".into());
        assert_eq!(pick_tv_leader(&members), None, "two Apple TVs: no leader");
    }

    #[test]
    fn airport_express_is_legacy() {
        let r = renderer(Some("AirPort10,115"), vec![0, 1], None, 5000, None);
        assert!(r.supports_legacy_raop());
        assert!(!r.requires_airplay2());
        assert_eq!(r.transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn homepod_routes_to_airplay2_even_with_legacy_et() {
        // HomePod advertises RAOP with et that includes 0/1, but gates on
        // pairing — must route to AP2.
        let r = renderer(
            Some("AudioAccessory5,1"),
            vec![0, 3, 5],
            Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING | FEAT_PTP),
            7000,
            Some(7000),
        );
        assert!(r.is_homepod());
        assert!(r.requires_airplay2());
        assert!(r.supports_airplay2());
        assert_eq!(r.transport(), Some(Transport::AirPlay2));
        assert!(r.expects_ptp());
    }

    #[test]
    fn real_sonos_features_classify_as_ap2_with_ptp() {
        // The exact `features` word a Sonos advertises on _airplay._tcp.
        // It must decode to AirPlay 2 + PTP so routing prefers AP2 — its
        // _raop._tcp service is vestigial and times out at OPTIONS.
        let ft = parse_features_str("0x445F8A00,0x1C340");
        assert!(ft.is_some(), "split-hex features should parse");
        let r = renderer(Some("One"), vec![0, 1], ft, 5000, Some(7000));
        assert!(r.supports_airplay2(), "Sonos must support AirPlay 2");
        assert!(r.expects_ptp(), "Sonos advertises PTP (feature bit 41)");
        assert!(r.supports_buffered_audio(), "Sonos advertises buffered audio (bit 40)");
        // It also advertises a legacy RAOP service we'd otherwise prefer,
        // which is why blind RAOP routing stalled at OPTIONS.
        assert!(r.supports_legacy_raop());
    }

    #[test]
    fn appletv_with_legacy_et_prefers_legacy() {
        // Apple TV supports HK pairing but also legacy RAOP — keep it on
        // the proven path.
        let r = renderer(
            Some("AppleTV6,2"),
            vec![0, 1],
            Some(FEAT_AUDIO | FEAT_HK_PAIRING | FEAT_TRANSIENT_PAIRING),
            7000,
            Some(7000),
        );
        assert!(!r.requires_airplay2());
        assert_eq!(r.transport(), Some(Transport::RaopLegacy));
    }

    #[test]
    fn fairplay_only_without_ap2_is_unsupported() {
        let r = renderer(Some("Speaker1,1"), vec![3, 5], None, 5000, None);
        assert!(!r.supports_legacy_raop());
        assert!(!r.supports_airplay2());
        assert_eq!(r.transport(), None);
        assert!(!r.is_supported());
    }

    #[test]
    fn rsa_preference_targets_airport_express_not_sonos() {
        // Sonos-class (et=0,4, no ek, model is a friendly name) → plaintext.
        let sonos = renderer(Some("Picture frame"), vec![0, 4], None, 5000, None);
        assert!(!sonos.prefers_rsa_encryption(), "Sonos must stay on plaintext");

        // Classic AirPort Express gen2 (am=AirPort4,x, et=0,1) → RSA.
        let apex2 = renderer(Some("AirPort4,107"), vec![0, 1], None, 5000, None);
        assert!(apex2.prefers_rsa_encryption());

        // AP2-era AirPort Express (am=AirPort10,x, et=0,4) → plaintext.
        let apex_ap2 = renderer(Some("AirPort10,115"), vec![0, 4], None, 5000, None);
        assert!(!apex_ap2.prefers_rsa_encryption());

        // ek=1 with et=1 available → RSA (classic AirPort Express gen1).
        let mut ek = renderer(None, vec![0, 1], None, 5000, None);
        ek.encryption_key_required = true;
        assert!(ek.prefers_rsa_encryption());

        // ek=1 but no et=1 on offer → we can't RSA, so no.
        let mut ek_no_rsa = renderer(Some("Weird"), vec![0], None, 5000, None);
        ek_no_rsa.encryption_key_required = true;
        assert!(!ek_no_rsa.prefers_rsa_encryption());

        // Third-party et=0,1 plaintext speaker (no ek, non-AirPort model)
        // → plaintext (must NOT force RSA, which would break it).
        let thirdparty = renderer(Some("Denon"), vec![0, 1], None, 5000, None);
        assert!(!thirdparty.prefers_rsa_encryption());

        // et=0,1,4 with ek=1 → still plaintext (et=4 present ⇒ AP2-era,
        // takes plaintext regardless of ek — matches OwnTone APEX3).
        let mut et4_ek = renderer(Some("AirPort4,999"), vec![0, 1, 4], None, 5000, None);
        et4_ek.encryption_key_required = true;
        assert!(!et4_ek.prefers_rsa_encryption());
    }
}
