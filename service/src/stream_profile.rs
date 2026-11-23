//! The ways the app serves its audio to a UPnP renderer.
//!
//! The audio is always 16-bit 44.1 kHz stereo PCM. A profile is one way of
//! naming and framing it, and fixes together the URL path the renderer is
//! given, the `Content-Type` it is served with, the body (a WAV file, or
//! raw big-endian samples) and the `protocolInfo` of the DIDL-Lite
//! metadata. So HEAD, GET and the metadata agree by construction, and the
//! server needs nothing but the path to know what to send.
//!
//! A renderer is offered the original stream first (or the profile it
//! played last time); only when it refuses is it asked which types it
//! plays (the `Sink` list of its ConnectionManager). That list only
//! selects among these built-in profiles: nothing a device says ends up
//! in a header, a URL or the metadata.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamProfile {
    /// What every UPnP speaker got before profiles existed, and what a
    /// Sonos always gets: WAV served as `audio/vnd.wave;codec=1` at
    /// `/stream.raw`, announced as `audio/wav` in the metadata, a HEAD
    /// answered with `Content-Length: 0`. Kept byte for byte.
    Legacy,
    /// WAV, as `audio/wav` everywhere.
    Wav,
    /// WAV, as `audio/x-wav` everywhere.
    XWav,
    /// WAV, as `audio/wave` everywhere.
    Wave,
    /// Raw samples, big-endian, without a header, as
    /// `audio/L16;rate=44100;channels=2` (RFC 2586).
    L16,
}

/// Every profile, in the order the app prefers them when a renderer
/// lists several: WAV spellings before L16.
pub const ALL: [StreamProfile; 5] =
    [StreamProfile::Legacy, StreamProfile::Wav, StreamProfile::XWav, StreamProfile::Wave, StreamProfile::L16];

/// After the opening offers and those a renderer lists, the order the
/// profiles it doesn't list are offered in: one of each body first.
const UNLISTED_ORDER: [StreamProfile; 4] =
    [StreamProfile::Wav, StreamProfile::L16, StreamProfile::XWav, StreamProfile::Wave];

/// Profiles tried in one connect, at most.
pub const MAX_ATTEMPTS: usize = 3;

/// The DLNA part of a WAV `protocolInfo`, the one Sonos and others have
/// been getting.
const WAV_DLNA: &str = "DLNA.ORG_PN=WAV;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=03700000000000000000000000000000";

const L16_MIME: &str = "audio/L16;rate=44100;channels=2";

impl StreamProfile {
    /// The path of this profile's stream on the app's HTTP server.
    pub fn path(self) -> &'static str {
        match self {
            StreamProfile::Legacy => "/stream.raw",
            StreamProfile::Wav => "/wav/stream.wav",
            StreamProfile::XWav => "/x-wav/stream.wav",
            StreamProfile::Wave => "/wave/stream.wav",
            StreamProfile::L16 => "/l16/stream.raw",
        }
    }

    /// The profile served at `path`, if any.
    pub fn from_path(path: &str) -> Option<StreamProfile> {
        ALL.into_iter().find(|p| p.path() == path)
    }

    /// The `Content-Type` the stream is served with.
    pub fn content_type(self) -> &'static str {
        match self {
            StreamProfile::Legacy => "audio/vnd.wave;codec=1",
            other => other.listed_as(),
        }
    }

    /// The `protocolInfo` of the stream's `<res>` in the metadata.
    pub fn protocol_info(self) -> String {
        match self {
            // Legacy's metadata has always said audio/wav.
            StreamProfile::Legacy => format!("http-get:*:audio/wav:{WAV_DLNA}"),
            StreamProfile::L16 => format!("http-get:*:{L16_MIME}:DLNA.ORG_PN=LPCM"),
            wav => format!("http-get:*:{}:{WAV_DLNA}", wav.listed_as()),
        }
    }

    /// Whether the body is a WAV file (a 44-byte RIFF header, then
    /// little-endian samples); otherwise raw big-endian samples.
    pub fn is_wav(self) -> bool {
        self != StreamProfile::L16
    }

    /// The MIME type a renderer would list for this profile.
    fn listed_as(self) -> &'static str {
        match self {
            StreamProfile::Legacy => "audio/vnd.wave",
            StreamProfile::Wav => "audio/wav",
            StreamProfile::XWav => "audio/x-wav",
            StreamProfile::Wave => "audio/wave",
            StreamProfile::L16 => L16_MIME,
        }
    }

    /// Whether `format` (a list entry's content format) names this
    /// profile's type. Parameters beyond the type count only for L16,
    /// whose rate and channels must be this stream's when given.
    fn matches(self, format: &str) -> bool {
        let mut parts = format.split(';').map(str::trim);
        let base = parts.next().unwrap_or("");
        let ours = self.listed_as().split(';').next().unwrap_or("");
        if !base.eq_ignore_ascii_case(ours) {
            return false;
        }
        if self != StreamProfile::L16 {
            return true;
        }
        parts.all(|param| {
            let (k, v) = param.split_once('=').unwrap_or((param, ""));
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if k.eq_ignore_ascii_case("rate") {
                v == "44100"
            } else if k.eq_ignore_ascii_case("channels") {
                v == "2"
            } else {
                true
            }
        })
    }
}

impl fmt::Display for StreamProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body = if self.is_wav() { "WAV" } else { "L16" };
        write!(f, "{body} as {} at {}", self.content_type(), self.path())
    }
}

/// One entry of a protocol list ([CM], the UPnP ConnectionManager service
/// specification, 2.5.2:
/// `<protocol>:<network>:<contentFormat>:<additionalInfo>`), the parts
/// that choose a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolInfo {
    pub protocol: String,
    pub content_format: String,
}

/// Entries read from one list, at most: real lists have tens.
const MAX_LIST_ENTRIES: usize = 1024;

/// A protocol list as a ConnectionManager gives it: comma-separated
/// entries, a comma inside one escaped as `\,`.
pub fn parse_protocol_list(csv: &str) -> Vec<ProtocolInfo> {
    let mut entries = Vec::new();
    let mut entry = String::new();
    let mut chars = csv.chars();
    let finish = |entry: &mut String, entries: &mut Vec<ProtocolInfo>| {
        let mut fields = entry.trim().splitn(4, ':');
        if let (Some(protocol), Some(_network), Some(format), Some(_info)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        {
            if entries.len() < MAX_LIST_ENTRIES {
                entries.push(ProtocolInfo { protocol: protocol.trim().to_string(), content_format: format.trim().to_string() });
            }
        }
        entry.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '\\' => entry.extend(chars.next()),
            ',' => finish(&mut entry, &mut entries),
            _ => entry.push(c),
        }
        if entries.len() >= MAX_LIST_ENTRIES {
            return entries;
        }
    }
    finish(&mut entry, &mut entries);
    entries
}

/// What the renderer said it accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sinks {
    /// No answer: no ConnectionManager, or the call failed or took too
    /// long.
    Unknown,
    /// Its `Sink` list.
    Listed(Vec<ProtocolInfo>),
}

/// Why a profile is offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// A Sonos (or IKEA SYMFONISK): the original stream, and only that.
    Sonos,
    /// It played this profile last time.
    Remembered,
    /// The original stream, the one every UPnP speaker is offered first.
    Original,
    /// It lists the profile's type.
    Listed,
    /// It lists types this app makes, but not this one, and refused those
    /// offered so far.
    NotListed,
    /// It gave no list.
    NoList,
    /// Its list is empty.
    EmptyList,
    /// Its list says only that anything goes (`http-get:*:*:*`).
    Wildcard,
    /// Its list names nothing the app produces.
    NothingWeMake,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Why::Sonos => "it is a Sonos",
            Why::Remembered => "it played that last time",
            Why::Original => "it is the stream every UPnP speaker is offered first",
            Why::Listed => "it lists that type",
            Why::NotListed => "it lists no other type this app makes",
            Why::NoList => "it gave no list of the types it plays",
            Why::EmptyList => "its list of the types it plays is empty",
            Why::Wildcard => "its list of types says only that anything goes",
            Why::NothingWeMake => "it lists none of the types this app makes",
        })
    }
}

/// The offers that come before any question about the renderer's types:
/// a Sonos's only one, or the profile it played last time and the
/// original stream.
fn opening(sonos: bool, remembered: Option<StreamProfile>) -> Vec<(StreamProfile, Why)> {
    match (sonos, remembered) {
        (true, _) => vec![(StreamProfile::Legacy, Why::Sonos)],
        (false, Some(StreamProfile::Legacy)) => vec![(StreamProfile::Legacy, Why::Remembered)],
        (false, Some(p)) => vec![(p, Why::Remembered), (StreamProfile::Legacy, Why::Original)],
        (false, None) => vec![(StreamProfile::Legacy, Why::Original)],
    }
}

/// Every offer to a renderer, in order, and why each. A Sonos gets the
/// original stream and nothing else. Any other renderer gets the profile
/// it played last time, if there is one, and the original stream, which
/// is what every renderer that works today gets; when those are refused,
/// the profiles it lists (WAV spellings before L16), then the rest,
/// [`MAX_ATTEMPTS`] in all. Only the offers after the opening ones depend
/// on `sinks` (see [`Offers`]).
pub fn plan(sonos: bool, remembered: Option<StreamProfile>, sinks: &Sinks) -> Vec<(StreamProfile, Why)> {
    let mut offers = opening(sonos, remembered);
    if sonos {
        return offers;
    }
    let http: Vec<&ProtocolInfo> = match sinks {
        Sinks::Unknown => Vec::new(),
        Sinks::Listed(list) => list.iter().filter(|p| p.protocol.eq_ignore_ascii_case("http-get")).collect(),
    };
    let listed: Vec<StreamProfile> =
        ALL.into_iter().filter(|p| http.iter().any(|entry| p.matches(&entry.content_format))).collect();
    let unlisted = match sinks {
        _ if !listed.is_empty() => Why::NotListed,
        Sinks::Unknown => Why::NoList,
        Sinks::Listed(list) if list.is_empty() => Why::EmptyList,
        Sinks::Listed(_)
            if !http.is_empty() && http.iter().all(|e| e.content_format == "*" || e.content_format.ends_with("/*")) =>
        {
            Why::Wildcard
        }
        Sinks::Listed(_) => Why::NothingWeMake,
    };
    let rest = listed.into_iter().map(|p| (p, Why::Listed)).chain(UNLISTED_ORDER.map(|p| (p, unlisted)));
    for (p, why) in rest {
        if offers.len() == MAX_ATTEMPTS {
            break;
        }
        if !offers.iter().any(|(q, _)| *q == p) {
            offers.push((p, why));
        }
    }
    offers
}

/// What to remember for a renderer once `played` has played: another
/// profile than the original only when the original stream was refused
/// for its content in this connect (`original_refused`); the remembered
/// one kept when it played again; otherwise nothing, so the next connect
/// starts from the original stream. The original itself is never stored:
/// it is offered first anyway.
pub fn to_remember(remembered: Option<StreamProfile>, played: StreamProfile, original_refused: bool) -> Option<StreamProfile> {
    match played {
        StreamProfile::Legacy => None,
        p if original_refused || remembered == Some(p) => Some(p),
        _ => None,
    }
}

/// The offers of one connect, worked out as they come due. The renderer
/// is asked which types it plays only when the next offer depends on its
/// list, that is after the opening offers were refused; so a renderer
/// that takes the original stream (or the profile it played last time)
/// is asked nothing, and a Sonos never is.
pub struct Offers {
    sonos: bool,
    remembered: Option<StreamProfile>,
    opening: usize,
    asked: bool,
    plan: Vec<(StreamProfile, Why)>,
}

impl Offers {
    pub fn new(sonos: bool, remembered: Option<StreamProfile>) -> Self {
        let plan = opening(sonos, remembered);
        Offers { sonos, remembered, opening: plan.len(), asked: false, plan }
    }

    /// Whether there is an offer number `i`, without asking anything.
    pub fn has(&self, i: usize) -> bool {
        if self.sonos { i < self.opening } else { i < MAX_ATTEMPTS }
    }

    /// Offer number `i` (from 0) and why, or None past the last. `ask`
    /// gets the renderer's list; it is called at most once, the first
    /// time an offer depends on it.
    pub fn get(&mut self, i: usize, ask: impl FnOnce() -> Sinks) -> Option<(StreamProfile, Why)> {
        if !self.sonos && !self.asked && i >= self.opening && i < MAX_ATTEMPTS {
            self.asked = true;
            self.plan = plan(self.sonos, self.remembered, &ask());
        }
        self.plan.get(i).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StreamProfile::*;

    #[test]
    fn each_profile_has_its_own_path_and_is_found_by_it() {
        for p in ALL {
            assert_eq!(StreamProfile::from_path(p.path()), Some(p), "{p:?}");
            assert!(p.path().starts_with('/'));
        }
        assert_eq!(Legacy.path(), "/stream.raw");
        let mut paths: Vec<&str> = ALL.iter().map(|p| p.path()).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), ALL.len());
        for other in ["/", "/stream", "/stream.raw/", "/wav/", "/STREAM.RAW", "/healthz"] {
            assert_eq!(StreamProfile::from_path(other), None, "{other}");
        }
    }

    #[test]
    fn header_body_and_metadata_name_the_same_type() {
        let table = [
            (Legacy, "audio/vnd.wave;codec=1", "http-get:*:audio/wav:", true),
            (Wav, "audio/wav", "http-get:*:audio/wav:", true),
            (XWav, "audio/x-wav", "http-get:*:audio/x-wav:", true),
            (Wave, "audio/wave", "http-get:*:audio/wave:", true),
            (L16, "audio/L16;rate=44100;channels=2", "http-get:*:audio/L16;rate=44100;channels=2:", false),
        ];
        for (p, content_type, info_start, wav) in table {
            assert_eq!(p.content_type(), content_type, "{p:?}");
            assert!(p.protocol_info().starts_with(info_start), "{p:?}: {}", p.protocol_info());
            assert_eq!(p.is_wav(), wav, "{p:?}");
        }
        // The WAV spellings carry the DLNA part legacy always had; L16 the
        // one DLNA defines for linear PCM.
        assert_eq!(
            Legacy.protocol_info(),
            "http-get:*:audio/wav:DLNA.ORG_PN=WAV;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=03700000000000000000000000000000"
        );
        assert_eq!(Wave.protocol_info(), Legacy.protocol_info().replace("audio/wav", "audio/wave"));
        assert_eq!(L16.protocol_info(), "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM");
    }

    #[test]
    fn protocol_lists_are_split_on_unescaped_commas() {
        let list = parse_protocol_list(
            " http-get:*:audio/wav:* ,http-get:*:audio/x-wav:DLNA.ORG_PN=WAV,rtsp-rtp-udp:*:audio/L16:*,\
             http-get:*:audio/weird\\,name:*,, garbage ,http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM",
        );
        let formats: Vec<(&str, &str)> =
            list.iter().map(|p| (p.protocol.as_str(), p.content_format.as_str())).collect();
        assert_eq!(
            formats,
            [
                ("http-get", "audio/wav"),
                ("http-get", "audio/x-wav"),
                ("rtsp-rtp-udp", "audio/L16"),
                ("http-get", "audio/weird,name"),
                ("http-get", "audio/L16;rate=44100;channels=2"),
            ]
        );
        assert!(parse_protocol_list("").is_empty());
        let huge = "http-get:*:audio/wav:*,".repeat(5000);
        assert_eq!(parse_protocol_list(&huge).len(), MAX_LIST_ENTRIES);
    }

    fn listed(csv: &str) -> Sinks {
        Sinks::Listed(parse_protocol_list(csv))
    }

    fn offers(sonos: bool, remembered: Option<StreamProfile>, sinks: &Sinks) -> Vec<(StreamProfile, Why)> {
        plan(sonos, remembered, sinks)
    }

    /// What #117's KEF LSX II LT lists.
    fn kef() -> Sinks {
        listed("http-get:*:audio/wav:*,http-get:*:audio/wave:*,http-get:*:audio/x-wav:*,http-get:*:audio/mpeg:*")
    }

    #[test]
    fn a_sonos_gets_the_original_stream_and_nothing_else() {
        for sinks in [Sinks::Unknown, listed("http-get:*:audio/wav:*")] {
            for remembered in [None, Some(Wav)] {
                assert_eq!(offers(true, remembered, &sinks), [(Legacy, Why::Sonos)]);
            }
        }
    }

    #[test]
    fn a_renderer_gets_the_original_stream_first_then_the_types_it_lists_then_the_rest() {
        assert_eq!(offers(false, None, &kef()), [(Legacy, Why::Original), (Wav, Why::Listed), (XWav, Why::Listed)]);
        assert_eq!(offers(false, None, &listed("http-get:*:AUDIO/WAVE:*")), [(Legacy, Why::Original), (Wave, Why::Listed), (Wav, Why::NotListed)]);
        assert_eq!(offers(false, None, &listed("http-get:*:audio/x-wav;codec=1:*")), [(Legacy, Why::Original), (XWav, Why::Listed), (Wav, Why::NotListed)]);
        // L16 counts with this app's rate and channels, or none named.
        assert_eq!(offers(false, None, &listed("http-get:*:audio/L16:*")), [(Legacy, Why::Original), (L16, Why::Listed), (Wav, Why::NotListed)]);
        assert_eq!(
            offers(false, None, &listed("http-get:*:audio/L16;channels=2;rate=44100:*")),
            [(Legacy, Why::Original), (L16, Why::Listed), (Wav, Why::NotListed)]
        );
        // WAV spellings come before L16.
        assert_eq!(
            offers(false, None, &listed("http-get:*:audio/L16:*,http-get:*:audio/wave:*")),
            [(Legacy, Why::Original), (Wave, Why::Listed), (L16, Why::Listed)]
        );
        // Listing the original stream's own type changes nothing.
        assert_eq!(
            offers(false, None, &listed("http-get:*:audio/vnd.wave;codec=1:*,http-get:*:audio/wav:*")),
            [(Legacy, Why::Original), (Wav, Why::Listed), (L16, Why::NotListed)]
        );
    }

    #[test]
    fn without_a_usable_list_the_rest_come_in_a_fixed_order() {
        for (sinks, why) in [
            (Sinks::Unknown, Why::NoList),
            (listed(""), Why::EmptyList),
            (listed("http-get:*:*:*"), Why::Wildcard),
            (listed("http-get:*:audio/*:*"), Why::Wildcard),
            (listed("http-get:*:audio/mpeg:*,http-get:*:audio/flac:*"), Why::NothingWeMake),
            // Other rates, other channel counts, other transports.
            (listed("http-get:*:audio/L16;rate=48000;channels=2:*"), Why::NothingWeMake),
            (listed("http-get:*:audio/L16;rate=44100;channels=1:*"), Why::NothingWeMake),
            (listed("rtsp-rtp-udp:*:audio/wav:*"), Why::NothingWeMake),
        ] {
            assert_eq!(offers(false, None, &sinks), [(Legacy, Why::Original), (Wav, why), (L16, why)], "{sinks:?}");
        }
    }

    #[test]
    fn a_remembered_profile_comes_first_then_the_original_stream() {
        let kef = kef();
        assert_eq!(offers(false, Some(Wave), &kef), [(Wave, Why::Remembered), (Legacy, Why::Original), (Wav, Why::Listed)]);
        assert_eq!(offers(false, Some(XWav), &Sinks::Unknown), [(XWav, Why::Remembered), (Legacy, Why::Original), (Wav, Why::NoList)]);
        assert_eq!(offers(false, Some(Legacy), &kef), [(Legacy, Why::Remembered), (Wav, Why::Listed), (XWav, Why::Listed)]);
    }

    #[test]
    fn a_plan_never_offers_more_than_three_or_one_twice() {
        let everything = listed(
            "http-get:*:audio/vnd.wave:*,http-get:*:audio/wav:*,http-get:*:audio/x-wav:*,http-get:*:audio/wave:*,http-get:*:audio/L16:*",
        );
        for sonos in [false, true] {
            for remembered in [None, Some(Legacy), Some(Wav), Some(XWav), Some(Wave), Some(L16)] {
                for sinks in [Sinks::Unknown, listed(""), everything.clone()] {
                    let a: Vec<StreamProfile> = plan(sonos, remembered, &sinks).into_iter().map(|(p, _)| p).collect();
                    assert!(!a.is_empty() && a.len() <= MAX_ATTEMPTS, "{a:?}");
                    let mut d = a.clone();
                    d.sort_by_key(|p| p.path());
                    d.dedup();
                    assert_eq!(d.len(), a.len(), "{a:?}");
                }
            }
        }
    }

    #[test]
    fn the_renderer_is_asked_for_its_list_only_when_an_offer_depends_on_it() {
        let never = || -> Sinks { panic!("asked for the list too early") };
        let asked = std::cell::Cell::new(0);
        let ask = || {
            asked.set(asked.get() + 1);
            kef()
        };
        // Nothing remembered: the original stream needs no list; the next does.
        let mut o = Offers::new(false, None);
        assert_eq!(o.get(0, never), Some((Legacy, Why::Original)));
        assert_eq!(o.get(1, ask), Some((Wav, Why::Listed)));
        assert_eq!(o.get(2, ask), Some((XWav, Why::Listed)));
        assert_eq!(o.get(3, ask), None);
        assert_eq!(asked.get(), 1);
        // Remembered: it and the original stream come before any question.
        let mut o = Offers::new(false, Some(Wave));
        assert_eq!(o.get(0, never), Some((Wave, Why::Remembered)));
        assert_eq!(o.get(1, never), Some((Legacy, Why::Original)));
        assert_eq!(o.get(2, ask), Some((Wav, Why::Listed)));
        assert_eq!(asked.get(), 2);
        // A Sonos is never asked.
        let mut o = Offers::new(true, None);
        assert_eq!(o.get(0, never), Some((Legacy, Why::Sonos)));
        assert_eq!(o.get(1, never), None);
    }

    #[test]
    fn only_a_profile_won_against_a_refused_original_is_remembered() {
        assert_eq!(to_remember(None, Wav, true), Some(Wav));
        assert_eq!(to_remember(None, Wav, false), None);
        assert_eq!(to_remember(Some(Wav), Wav, false), Some(Wav));
        assert_eq!(to_remember(Some(Wav), XWav, false), None);
        assert_eq!(to_remember(Some(Wav), XWav, true), Some(XWav));
        assert_eq!(to_remember(Some(Wav), Legacy, false), None);
        assert_eq!(to_remember(None, Legacy, false), None);
        assert_eq!(to_remember(Some(Legacy), Legacy, false), None);
    }

    #[test]
    fn profiles_and_reasons_read_well_in_the_log() {
        assert_eq!(Legacy.to_string(), "WAV as audio/vnd.wave;codec=1 at /stream.raw");
        assert_eq!(Wav.to_string(), "WAV as audio/wav at /wav/stream.wav");
        assert_eq!(L16.to_string(), "L16 as audio/L16;rate=44100;channels=2 at /l16/stream.raw");
        assert_eq!(Why::Original.to_string(), "it is the stream every UPnP speaker is offered first");
        assert_eq!(Why::Listed.to_string(), "it lists that type");
        assert_eq!(Why::NotListed.to_string(), "it lists no other type this app makes");
        assert_eq!(Why::NoList.to_string(), "it gave no list of the types it plays");
    }

    #[test]
    fn profiles_are_stored_under_stable_names() {
        let names: Vec<String> = ALL.iter().map(|p| serde_json::to_string(p).unwrap()).collect();
        assert_eq!(names, ["\"legacy\"", "\"wav\"", "\"x-wav\"", "\"wave\"", "\"l16\""]);
        for p in ALL {
            assert_eq!(serde_json::from_str::<StreamProfile>(&serde_json::to_string(&p).unwrap()).unwrap(), p);
        }
    }
}
