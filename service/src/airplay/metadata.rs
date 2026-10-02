//! Sending "now playing" metadata to an AirPlay receiver.
//!
//! Both AirPlay generations take the same three `SET_PARAMETER` bodies
//! (OwnTone `raop.c` / `airplay.c`, shairport-sync `rtsp.c`):
//!
//! * text — `application/x-dmap-tagged` (title / artist / album / length),
//! * artwork — `image/jpeg` or `image/png`,
//! * progress — `text/parameters` `progress: start/current/end`, three RTP
//!   timestamps on the stream's clock: where the track began, what is
//!   being heard now, where it will end.
//!
//! A receiver says which it wants: RAOP in the `md` TXT key (`0` text,
//! `1` artwork, `2` progress), AirPlay 2 in feature bits 15/16/17.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::airplay::ap2_rtsp::Ap2Rtsp;
use crate::airplay::dmap::{now_playing_body, TrackText};
use crate::airplay::rtsp::RtspClient;
use crate::now_playing::{NowPlaying, Plan};
use crate::WIRE_SAMPLE_RATE;

/// AirPlay 2 feature bit 15 — `MetadataFeatures_0`: send artwork.
pub const FEAT_METADATA_ARTWORK: u64 = 1 << 15;
/// Bit 16 — `MetadataFeatures_1`: send progress.
pub const FEAT_METADATA_PROGRESS: u64 = 1 << 16;
/// Bit 17 — `MetadataFeatures_2`: send now-playing text (DAAP).
pub const FEAT_METADATA_TEXT: u64 = 1 << 17;

/// Which metadata parts a receiver accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetadataCaps {
    pub text: bool,
    pub artwork: bool,
    pub progress: bool,
}

impl MetadataCaps {
    /// From the RAOP `md` TXT list. A receiver that doesn't publish `md`
    /// still gets the text (what this sender always sent to RAOP
    /// receivers, and harmless — every receiver class ignores bodies it
    /// doesn't use); artwork and progress only when asked for.
    pub fn from_raop_md(md: Option<&[u8]>) -> Self {
        match md {
            None => Self { text: true, artwork: false, progress: false },
            Some(list) => Self {
                text: list.contains(&0),
                artwork: list.contains(&1),
                progress: list.contains(&2),
            },
        }
    }

    /// From the AirPlay 2 `features` word (OwnTone parity: nothing unless
    /// the bits are set).
    pub fn from_features(features: Option<u64>) -> Self {
        let f = features.unwrap_or(0);
        Self {
            text: f & FEAT_METADATA_TEXT != 0,
            artwork: f & FEAT_METADATA_ARTWORK != 0,
            progress: f & FEAT_METADATA_PROGRESS != 0,
        }
    }

    pub fn any(&self) -> bool {
        self.text || self.artwork || self.progress
    }
}

/// A fresh id for each session's metadata handle (never reused, unlike
/// an allocation address).
pub fn next_session_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
enum Conn {
    Raop(Arc<Mutex<RtspClient>>),
    Ap2(Arc<Mutex<Ap2Rtsp>>),
}

/// Detached metadata sender for one session. Holds only the shared RTSP
/// connection and the RTP clock, so the background forwarder never holds
/// the (GUI-polled) session mutex during a network send.
#[derive(Clone)]
pub struct MetadataHandle {
    conn: Conn,
    /// RTP write head of the audio sender.
    current_rtptime: Arc<AtomicU32>,
    /// How far (in samples) the audible point trails the write head: the
    /// receiver latency the sync packets / anchor establish.
    playout_lag: u32,
    caps: MetadataCaps,
    /// Set after a failed send: metadata stops for this session. Shared by
    /// every clone. (The forwarder also remembers the device, so a
    /// reconnect doesn't retry — see `App::run_now_playing_forwarder`.)
    disabled: Arc<AtomicBool>,
    session_id: u64,
    /// The speaker's stable id.
    device_id: String,
}

impl MetadataHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn raop(
        rtsp: Arc<Mutex<RtspClient>>,
        current_rtptime: Arc<AtomicU32>,
        playout_lag: u32,
        caps: MetadataCaps,
        disabled: Arc<AtomicBool>,
        session_id: u64,
        device_id: String,
    ) -> Self {
        Self { conn: Conn::Raop(rtsp), current_rtptime, playout_lag, caps, disabled, session_id, device_id }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ap2(
        rtsp: Arc<Mutex<Ap2Rtsp>>,
        current_rtptime: Arc<AtomicU32>,
        playout_lag: u32,
        caps: MetadataCaps,
        disabled: Arc<AtomicBool>,
        session_id: u64,
        device_id: String,
    ) -> Self {
        Self { conn: Conn::Ap2(rtsp), current_rtptime, playout_lag, caps, disabled, session_id, device_id }
    }

    pub fn caps(&self) -> MetadataCaps {
        self.caps
    }

    /// Identifies the session this handle belongs to (stable across
    /// clones, different for every session).
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// The speaker's stable id.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    pub fn disable(&self) {
        self.disabled.store(true, Ordering::Release);
    }

    /// Send the `plan`ned parts of `np`, progress first, then text, then
    /// artwork (OwnTone's order). Stops at the first failure.
    ///
    /// Text and artwork carry the item's `RTP-Info` rtptime, which
    /// receivers use to tie them together: a new item (text in the plan)
    /// gets a fresh one, which is returned; artwork arriving later for the
    /// same item must pass that value back as `item_rtptime`.
    pub fn send(
        &self,
        np: &NowPlaying,
        plan: Plan,
        now: Instant,
        item_rtptime: Option<u32>,
    ) -> Result<u32> {
        let playing_rtp = self
            .current_rtptime
            .load(Ordering::Acquire)
            .wrapping_sub(self.playout_lag);
        let duration_ms = np.timeline.map(|t| t.duration_ms);
        let rt = item_rtptimes(playing_rtp, np.position_ms_at(now), duration_ms);
        let item = match item_rtptime {
            Some(id) if !plan.text => id,
            _ => rt.start,
        };
        if plan.progress {
            if let Some(line) = rt.progress_line() {
                self.set_parameter("text/parameters", line.as_bytes(), rt.start)?;
            }
        }
        if plan.text {
            let body = now_playing_body(&TrackText {
                title: &np.title,
                artist: &np.artist,
                album: &np.album,
                duration_ms: duration_ms.map(|d| d.min(u32::MAX as u64) as u32),
            });
            self.set_parameter("application/x-dmap-tagged", &body, item)?;
        }
        if plan.artwork {
            if let Some(art) = &np.artwork {
                self.set_parameter(art.mime, &art.bytes, item)?;
            }
        }
        Ok(item)
    }

    fn set_parameter(&self, content_type: &str, body: &[u8], rtptime: u32) -> Result<()> {
        match &self.conn {
            Conn::Raop(c) => c.lock().unwrap().set_metadata(content_type, body, rtptime),
            Conn::Ap2(c) => c.lock().unwrap().set_metadata(content_type, body, rtptime),
        }
    }
}

/// RTP timestamps describing the current item on the stream's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemRtpTimes {
    /// Where the track began (also the `RTP-Info` id of its text/art).
    pub start: u32,
    /// What the receiver is playing now.
    pub current: u32,
    /// Where the track ends, when its length is known.
    pub end: Option<u32>,
}

impl ItemRtpTimes {
    /// `progress: start/current/end\r\n` (shairport-sync / OwnTone form),
    /// only for tracks with a known length.
    pub fn progress_line(&self) -> Option<String> {
        self.end
            .map(|end| format!("progress: {}/{}/{}\r\n", self.start, self.current, end))
    }
}

/// Map a track position onto the RTP clock, given the RTP time being
/// heard now. All arithmetic wraps like RTP timestamps do.
pub fn item_rtptimes(playing_rtp: u32, position_ms: Option<u64>, duration_ms: Option<u64>) -> ItemRtpTimes {
    let samples = |ms: u64| (ms.saturating_mul(WIRE_SAMPLE_RATE as u64) / 1000) as u32;
    let start = playing_rtp.wrapping_sub(position_ms.map(samples).unwrap_or(0));
    let end = duration_ms
        .filter(|&d| d > 0)
        .map(|d| start.wrapping_add(samples(d)));
    ItemRtpTimes { start, current: playing_rtp, end }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raop_md_parsing() {
        assert_eq!(
            MetadataCaps::from_raop_md(Some(&[0, 1, 2])),
            MetadataCaps { text: true, artwork: true, progress: true }
        );
        assert_eq!(
            MetadataCaps::from_raop_md(Some(&[0, 2])),
            MetadataCaps { text: true, artwork: false, progress: true }
        );
        // No md key: text only (legacy behaviour).
        assert_eq!(
            MetadataCaps::from_raop_md(None),
            MetadataCaps { text: true, artwork: false, progress: false }
        );
        assert!(!MetadataCaps::from_raop_md(Some(&[])).any());
    }

    #[test]
    fn features_bits() {
        // The `features` word a current Sonos advertises: bits 15–17 set.
        let sonos = 0x1C340_445F8A00u64;
        assert_eq!(
            MetadataCaps::from_features(Some(sonos)),
            MetadataCaps { text: true, artwork: true, progress: true }
        );
        assert!(!MetadataCaps::from_features(None).any());
        assert_eq!(
            MetadataCaps::from_features(Some(FEAT_METADATA_TEXT)),
            MetadataCaps { text: true, ..Default::default() }
        );
    }

    #[test]
    fn rtptimes_place_track_around_now() {
        // 10 s into a 60 s track, hearing rtp 1_000_000 now.
        let rt = item_rtptimes(1_000_000, Some(10_000), Some(60_000));
        assert_eq!(rt.start, 1_000_000 - 441_000);
        assert_eq!(rt.current, 1_000_000);
        assert_eq!(rt.end, Some(1_000_000 - 441_000 + 2_646_000));
        assert_eq!(
            rt.progress_line().unwrap(),
            "progress: 559000/1000000/3205000\r\n"
        );
    }

    #[test]
    fn rtptimes_wrap_and_unknowns() {
        let rt = item_rtptimes(100, Some(1_000), Some(2_000));
        assert_eq!(rt.start, 100u32.wrapping_sub(44_100));
        assert_eq!(rt.end, Some(rt.start.wrapping_add(88_200)));
        // No position: the item starts now; no length: no progress line.
        let rt = item_rtptimes(5, None, None);
        assert_eq!(rt.start, 5);
        assert_eq!(rt.end, None);
        assert!(rt.progress_line().is_none());
        assert!(item_rtptimes(5, Some(0), Some(0)).progress_line().is_none());
    }
}
