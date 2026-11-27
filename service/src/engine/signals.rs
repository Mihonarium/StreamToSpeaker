//! What a speaker tells the engine about itself, read from each protocol:
//! a UPnP speaker's transport events, a Sonos system's group changes, an
//! AirPlay receiver's announcement and how its sessions end. The readers
//! here are pure; the app keeps what it heard in a [`SignalLog`] until the
//! engine takes it.

use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::gena::TransportChange;
use crate::model::state::Secret;
use crate::sonos::ZoneGroup;

/// What a speaker said, or what a request to it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeakerSignal {
    /// It answered a request. Also the first request of a connection the
    /// core asked for, while that connection is still getting ready: that
    /// is what tells a speaker that takes its time from one that is silent.
    Responding,
    /// The speaker reports that it is playing this PC’s stream.
    PlayingOurStream,
    /// A request to it failed, or the stream to it broke.
    NotResponding,
    /// It confirmed it stopped playing this PC's sound (or the request to
    /// make it silent failed).
    Silenced,
    /// It asks for its code or password again.
    AsksSecret(Secret),
    /// It plays something else (an AirPlay speaker: another device sends to
    /// it); `who` names another PC with this app where the speaker says it.
    PlayingElse { who: Option<String> },
    /// It no longer plays something else.
    Free,
    /// A pause from its own controls (or its app).
    PausedAtSpeaker,
    /// A stop from its own controls, or a stop that can't be told from a
    /// broken stream.
    StoppedAtSpeaker,
    /// Play pressed on the speaker.
    PlayPressed,
    /// It ended this PC's stream; `reachable` when it could still be
    /// reached afterwards.
    EndedStream { reachable: bool },
    /// It announced itself on the network.
    Announced,
    /// It left the network by its own announcement.
    Unannounced,
    /// A room joined the Sonos group it leads while it played.
    JoinedGroup { room: String },
    /// The Sonos room was taken into the group that room `with` leads.
    GroupedInto { with: String },
    /// It left that group.
    LeftGroup,
}

/// What a Sonos topology says of the room that plays this PC's sound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SonosRoom {
    /// It leads a group of these rooms, its own first; `joined`: the rooms
    /// that weren't in it before.
    Leads { members: Vec<String>, joined: Vec<String> },
    /// It plays in the group that room `with` leads.
    TakenInto { with: String },
}

/// A signal and who gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    /// The app's id for the speaker.
    pub speaker: String,
    pub signal: SpeakerSignal,
    pub at: Instant,
}

/// What the app heard from speakers since the engine last looked.
#[derive(Default)]
pub struct SignalLog(Mutex<Vec<Heard>>);

impl SignalLog {
    /// The longest backlog kept: older signals are dropped first.
    const KEPT: usize = 64;

    pub fn push(&self, speaker: &str, signal: SpeakerSignal) {
        let mut log = self.0.lock().unwrap();
        if log.len() >= Self::KEPT {
            log.remove(0);
        }
        log.push(Heard { speaker: speaker.to_string(), signal, at: Instant::now() });
    }

    /// Everything heard so far, oldest first; the log is empty after.
    pub fn take(&self) -> Vec<Heard> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// What a UPnP speaker's transport event says, for the speaker that was
/// given `ours` (the start of this PC's stream address toward it):
/// paused, stopped, playing this PC's stream, or something else. None
/// when it says nothing of that (a bare re-buffering, an empty event).
pub fn upnp_transport(change: &TransportChange, ours: &str) -> Option<SpeakerSignal> {
    let state = change.state.as_deref().unwrap_or_default().trim().to_ascii_uppercase();
    let uri = change.uri.as_deref().unwrap_or_default();
    let someone_else = !uri.is_empty() && !uri.starts_with(ours);
    let who = || change.title.as_deref().and_then(crate::upnp::other_pc_in_title);
    match state.as_str() {
        "PAUSED_PLAYBACK" if !someone_else => Some(SpeakerSignal::PausedAtSpeaker),
        "STOPPED" | "NO_MEDIA_PRESENT" if !someone_else => Some(SpeakerSignal::StoppedAtSpeaker),
        "PLAYING" | "TRANSITIONING" if someone_else => Some(SpeakerSignal::PlayingElse { who: who() }),
        "PLAYING" => Some(SpeakerSignal::PlayingOurStream),
        // Paused or stopped on another source: someone else's, not
        // playing; nothing to say about this PC's stream.
        "PAUSED_PLAYBACK" | "STOPPED" | "NO_MEDIA_PRESENT" => Some(SpeakerSignal::Free),
        // Only a new address: it was told to play something else.
        "" if someone_else => Some(SpeakerSignal::PlayingElse { who: who() }),
        _ => None,
    }
}

/// What a Sonos topology says about the room `uuid` that plays this PC's
/// sound, whose group had the rooms `before`: taken into another room's
/// group, or leading its own (the rooms that joined since). None when the
/// topology doesn't name the room.
pub fn sonos_group(groups: &[ZoneGroup], uuid: &str, before: &[String]) -> Option<SonosRoom> {
    let uuid = uuid.trim_start_matches("uuid:");
    let group = groups.iter().find(|g| g.members.iter().any(|m| m.uuid == uuid))?;
    if group.coordinator_uuid != uuid {
        let leader = group.members.iter().find(|m| m.uuid == group.coordinator_uuid)?;
        return Some(SonosRoom::TakenInto { with: leader.zone_name.clone() });
    }
    let members = group.visible_member_names();
    let joined = members.iter().filter(|m| !before.contains(m)).cloned().collect();
    Some(SonosRoom::Leads { members, joined })
}

/// Whether something answers a TCP connection at `addr` within `within`:
/// the least a speaker can be asked (nothing is sent on the connection).
pub fn answers_on(addr: SocketAddr, within: Duration) -> bool {
    TcpStream::connect_timeout(&addr, within).is_ok()
}

/// How long [`answers_on`] waits when a session ended, to tell a speaker
/// that ended it from one that went away.
pub const STILL_THERE_WAIT: Duration = Duration::from_millis(1500);

/// How often the app asks a speaker it switched off whether it can play
/// again: every [`ASK_EVERY_AT_FIRST`] for [`ASK_OFTEN_FOR`], then every
/// [`ASK_EVERY_LATER`] until [`ASK_UNTIL`] after the switch; its
/// announcements are listened to throughout.
pub const ASK_EVERY_AT_FIRST: Duration = Duration::from_secs(15);
/// See [`ASK_EVERY_AT_FIRST`].
pub const ASK_OFTEN_FOR: Duration = Duration::from_secs(2 * 60);
/// See [`ASK_EVERY_AT_FIRST`].
pub const ASK_EVERY_LATER: Duration = Duration::from_secs(60);
/// See [`ASK_EVERY_AT_FIRST`].
pub const ASK_UNTIL: Duration = Duration::from_secs(30 * 60);

/// When to ask a speaker the app switched off at `switched_off` next,
/// after it was last asked at `asked` (None: not yet); None once the
/// asking has ended.
pub fn next_ask(switched_off: Instant, asked: Option<Instant>) -> Option<Instant> {
    let end = switched_off + ASK_UNTIL;
    let next = match asked {
        None => switched_off + ASK_EVERY_AT_FIRST,
        Some(at) if at < switched_off + ASK_OFTEN_FOR => at + ASK_EVERY_AT_FIRST,
        Some(at) => at + ASK_EVERY_LATER,
    };
    (next <= end).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::ZoneMember;

    fn change(state: Option<&str>, uri: Option<&str>, title: Option<&str>) -> TransportChange {
        TransportChange { state: state.map(Into::into), uri: uri.map(Into::into), title: title.map(Into::into) }
    }

    const OURS: &str = "http://192.168.1.10:8080/";

    /// A UPnP speaker's transport events read as paused, stopped, playing
    /// this PC's stream, or another source; another PC with this app is
    /// named from the title it gives.
    #[test]
    fn upnp_events_tell_paused_stopped_and_another_source() {
        let ours = Some("http://192.168.1.10:8080/stream.flac");
        let read = |state, uri, title| upnp_transport(&change(state, uri, title), OURS);
        assert_eq!(read(Some("PAUSED_PLAYBACK"), ours, None), Some(SpeakerSignal::PausedAtSpeaker));
        assert_eq!(read(Some("PAUSED_PLAYBACK"), None, None), Some(SpeakerSignal::PausedAtSpeaker));
        assert_eq!(read(Some("STOPPED"), ours, None), Some(SpeakerSignal::StoppedAtSpeaker));
        assert_eq!(read(Some("NO_MEDIA_PRESENT"), None, None), Some(SpeakerSignal::StoppedAtSpeaker));
        assert_eq!(read(Some("PLAYING"), ours, None), Some(SpeakerSignal::PlayingOurStream));
        assert_eq!(read(Some("playing"), None, None), Some(SpeakerSignal::PlayingOurStream));
        let spotify = Some("x-sonos-spotify:spotify%3atrack%3a1");
        assert_eq!(read(Some("PLAYING"), spotify, None), Some(SpeakerSignal::PlayingElse { who: None }));
        assert_eq!(read(Some("TRANSITIONING"), spotify, None), Some(SpeakerSignal::PlayingElse { who: None }));
        assert_eq!(read(None, spotify, None), Some(SpeakerSignal::PlayingElse { who: None }));
        let laptop = Some("http://192.168.1.44:8080/stream.flac");
        assert_eq!(
            read(Some("PLAYING"), laptop, Some("LAPTOP-7Q2K via Stream To Speaker")),
            Some(SpeakerSignal::PlayingElse { who: Some("LAPTOP-7Q2K".into()) })
        );
        // Paused on someone else's music: not playing something else.
        assert_eq!(read(Some("PAUSED_PLAYBACK"), spotify, None), Some(SpeakerSignal::Free));
        // Re-buffering on this PC's stream says nothing here.
        assert_eq!(read(Some("TRANSITIONING"), ours, None), None);
        assert_eq!(read(None, None, None), None);

        // From the NOTIFY bodies a Sonos sends.
        let notify = |inner: &str| {
            let escaped = inner.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
            format!(
                "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property><LastChange>{escaped}</LastChange>\
                 </e:property></e:propertyset>"
            )
        };
        let paused = notify(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="PAUSED_PLAYBACK"/></InstanceID></Event>"#,
        );
        let parsed = crate::gena::parse_transport_notify(&paused).unwrap();
        assert_eq!(upnp_transport(&parsed, OURS), Some(SpeakerSignal::PausedAtSpeaker));
        let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/"><item><dc:title>LAPTOP-7Q2K via Stream To Speaker</dc:title></item></DIDL-Lite>"#
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let other_pc = notify(&format!(
            r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"><InstanceID val="0"><TransportState val="PLAYING"/><AVTransportURI val="http://192.168.1.44:8080/stream.flac"/><AVTransportURIMetaData val="{didl}"/></InstanceID></Event>"#
        ));
        let parsed = crate::gena::parse_transport_notify(&other_pc).unwrap();
        assert_eq!(upnp_transport(&parsed, OURS), Some(SpeakerSignal::PlayingElse { who: Some("LAPTOP-7Q2K".into()) }));
    }

    fn member(uuid: &str, name: &str) -> ZoneMember {
        ZoneMember { uuid: uuid.into(), zone_name: name.into(), location: String::new(), invisible: false }
    }

    /// A Sonos room that plays this PC's sound and is taken into another
    /// room's group is reported with that group's leader; a room that
    /// joins its own group changes the group's rooms.
    #[test]
    fn a_sonos_room_grouped_elsewhere_is_reported() {
        let kitchen = "RINCON_KITCHEN";
        let before = vec!["Kitchen".to_string()];
        let alone = vec![
            ZoneGroup { coordinator_uuid: kitchen.into(), members: vec![member(kitchen, "Kitchen")] },
            ZoneGroup { coordinator_uuid: "RINCON_LIVING".into(), members: vec![member("RINCON_LIVING", "Living Room")] },
        ];
        let leads = |members: &[&str], joined: &[&str]| {
            Some(SonosRoom::Leads {
                members: members.iter().map(|m| m.to_string()).collect(),
                joined: joined.iter().map(|m| m.to_string()).collect(),
            })
        };
        assert_eq!(sonos_group(&alone, kitchen, &before), leads(&["Kitchen"], &[]));
        assert_eq!(sonos_group(&alone, &format!("uuid:{kitchen}"), &before), leads(&["Kitchen"], &[]));
        let taken = vec![ZoneGroup {
            coordinator_uuid: "RINCON_LIVING".into(),
            members: vec![member("RINCON_LIVING", "Living Room"), member(kitchen, "Kitchen")],
        }];
        assert_eq!(sonos_group(&taken, kitchen, &before), Some(SonosRoom::TakenInto { with: "Living Room".into() }));
        let joined = vec![ZoneGroup {
            coordinator_uuid: kitchen.into(),
            members: vec![member(kitchen, "Kitchen"), member("RINCON_BED", "Bedroom")],
        }];
        assert_eq!(sonos_group(&joined, kitchen, &before), leads(&["Kitchen", "Bedroom"], &["Bedroom"]));
        // A topology that doesn't name the room says nothing.
        assert_eq!(sonos_group(&alone[1..], kitchen, &before), None);
    }

    /// The asking after a switch off: every 15 s for two minutes, then
    /// every minute, until 30 minutes have passed.
    #[test]
    fn a_speaker_switched_off_is_asked_on_a_falling_schedule() {
        let t0 = Instant::now();
        let s = |n| Duration::from_secs(n);
        assert_eq!(next_ask(t0, None), Some(t0 + s(15)));
        assert_eq!(next_ask(t0, Some(t0 + s(15))), Some(t0 + s(30)));
        assert_eq!(next_ask(t0, Some(t0 + s(105))), Some(t0 + s(120)));
        assert_eq!(next_ask(t0, Some(t0 + s(120))), Some(t0 + s(180)));
        assert_eq!(next_ask(t0, Some(t0 + s(1740))), Some(t0 + s(1800)));
        assert_eq!(next_ask(t0, Some(t0 + s(1790))), None);
        // Counted out: 8 asks in the first two minutes, then 28 more.
        let mut asked = None;
        let mut n = 0;
        while let Some(at) = next_ask(t0, asked) {
            asked = Some(at);
            n += 1;
        }
        assert_eq!(n, 8 + 28);
    }

    #[test]
    fn the_log_keeps_signals_in_order_until_taken() {
        let log = SignalLog::default();
        log.push("a", SpeakerSignal::PausedAtSpeaker);
        log.push("b", SpeakerSignal::Free);
        let heard: Vec<_> = log.take().into_iter().map(|h| (h.speaker, h.signal)).collect();
        assert_eq!(heard, [("a".to_string(), SpeakerSignal::PausedAtSpeaker), ("b".to_string(), SpeakerSignal::Free)]);
        assert!(log.take().is_empty());
        for i in 0..100 {
            log.push(&i.to_string(), SpeakerSignal::Free);
        }
        let kept = log.take();
        assert_eq!(kept.len(), SignalLog::KEPT);
        assert_eq!(kept[0].speaker, (100 - SignalLog::KEPT).to_string());
    }

    #[test]
    fn a_closed_port_does_not_answer() {
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(answers_on(open.local_addr().unwrap(), Duration::from_secs(1)));
        let addr = open.local_addr().unwrap();
        drop(open);
        assert!(!answers_on(addr, Duration::from_secs(1)));
    }
}
