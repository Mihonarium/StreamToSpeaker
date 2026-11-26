//! What the app knows, as the model's facts.

use std::collections::HashMap;
use std::net::IpAddr;
use crate::app::App;
use crate::model::facts::*;
use crate::model::state::SpeakerId;
/// The addresses of every room of a Sonos group that `renderers` lists
/// (its coordinator and its members).
fn grouped_rooms(renderers: &[crate::ssdp::Renderer]) -> std::collections::HashSet<IpAddr> {
    renderers
        .iter()
        .filter(|r| r.is_group())
        .flat_map(|r| std::iter::once(r.ip).chain(r.host_addrs.iter().copied()).chain(r.group_member_addrs.iter().copied()))
        .collect()
}

/// Every speaker the app knows, one entry per way of reaching it: a speaker
/// reached two ways is two entries that name each other as twins.
pub(crate) fn speaker_list(app: &App) -> Vec<SpeakerFacts> {
    let (settings, last) = {
        let uc = app.user_config.lock().unwrap();
        (uc.speaker_settings.clone(), uc.last_speaker_id.clone())
    };
    let row = |id: String, name: String, kind: SpeakerKind, address: String, presence: Presence| {
        let s = settings.get(&id);
        // *Show as*: the person's name for it, everywhere.
        let name = s.and_then(|s| s.show_as.clone()).filter(|n| !n.trim().is_empty()).unwrap_or(name);
        SpeakerFacts {
            used_here: s.is_some() || last.as_deref() == Some(id.as_str()),
            hidden: s.is_some_and(|s| s.hidden),
            id: SpeakerId::new(&id),
            name,
            kind,
            address,
            presence,
            twin: None,
            last_used: s.and_then(|s| s.last_played),
        }
    };
    let mut out = Vec::new();
    let mut upnp_at: HashMap<IpAddr, usize> = HashMap::new();
    let mut upnp_id_at: HashMap<IpAddr, SpeakerId> = HashMap::new();
    let mut grouped = std::collections::HashSet::new();
    if let Some(d) = app.discovery.as_ref() {
        let renderers = d.renderers();
        grouped = grouped_rooms(&renderers);
        for r in renderers {
            *upnp_at.entry(r.ip).or_default() += 1;
            upnp_id_at.insert(r.ip, SpeakerId::new(&r.stable_id()));
            let kind = if r.is_group() {
                SpeakerKind::SonosGroup
            } else if r.is_sonos() {
                SpeakerKind::Sonos
            } else {
                SpeakerKind::Other { maker: None }
            };
            out.push(row(r.stable_id(), r.display_name(), kind, r.ip.to_string(), Presence::Found));
        }
    }
    let playing = app.session.lock().unwrap().as_ref().map(|s| s.stable_id());
    if let Some(d) = app.airplay_discovery.as_ref() {
        for r in d.renderers() {
            // A room of a Sonos group is reached as the group, by Sonos
            // only: by AirPlay this PC couldn't know which rooms sound.
            if grouped.contains(&r.ip) && playing.as_deref() != Some(r.stable_id().as_str()) {
                continue;
            }
            let usable = r.transport().is_some();
            // One UPnP entry at the same address is the same speaker; it
            // plays with less delay. More than one, or a receiver claiming
            // another's identity: unsure, so separate rows.
            let twin = (usable && r.pair.is_none() && !r.is_newcomer() && upnp_at.get(&r.ip) == Some(&1))
                .then(|| upnp_id_at.get(&r.ip).cloned())
                .flatten();
            let (kind, presence) = match &r.pair {
                Some(pair) => (
                    SpeakerKind::HomePodPair,
                    if pair.is_complete() { Presence::Found } else { Presence::WaitingForPair },
                ),
                None if r.is_homepod() => (SpeakerKind::HomePod, Presence::Found),
                None if r.model.as_deref().is_some_and(|m| m.contains("AppleTV")) => {
                    (SpeakerKind::AppleTv, Presence::Found)
                }
                None => (SpeakerKind::AirPlay, Presence::Found),
            };
            let presence = if usable { presence } else { Presence::CantPlay };
            let mut entry = row(r.stable_id(), r.friendly_name.clone(), kind, r.ip.to_string(), presence);
            if let Some(t) = twin {
                let airplay = entry.id.clone();
                if let Some(upnp) = out.iter_mut().find(|s| s.id == t) {
                    upnp.twin = Some(airplay);
                    entry.twin = Some(t);
                }
            }
            out.push(entry);
        }
    }
    for s in app.sendspin_speakers(None) {
        if s.id == crate::sendspin_app::MA_OUTPUT_ID {
            continue;
        }
        let name = s.friendly_name.strip_suffix(" (Sendspin)").unwrap_or(&s.friendly_name).to_string();
        out.push(row(s.id, name, SpeakerKind::Sendspin, s.ip, Presence::Found));
    }
    for m in app.manual_speakers() {
        let kind = match m.kind {
            crate::manual_speakers::ManualKind::AirPlay => SpeakerKind::AirPlay,
            crate::manual_speakers::ManualKind::Upnp => SpeakerKind::Other { maker: None },
        };
        out.push(row(m.id(), m.display_name(), kind, m.address_label(), Presence::Found));
    }
    out
}

/// The adjustments offered for this way of playing on a speaker.
pub(crate) fn delay_range(d: &crate::delay::DelayStatus) -> Option<(DelayRange, Applies)> {
    use crate::delay::{Applies as A, Unchangeable as U};
    let applies = match d.applies {
        Ok(A::Live) => Applies::Live,
        Ok(A::Reconnect) => Applies::Reconnect,
        Err(U::KeptBySpeakerOnSteady) => Applies::Fixed,
        Err(U::NoSpeaker | U::MusicAssistant) => return None,
    };
    let range = d
        .limits
        .map(|l| DelayRange { max_removed_ms: l.remove_ms, max_added_ms: l.add_ms })
        .unwrap_or(DelayRange { max_removed_ms: 0, max_added_ms: 0 });
    Some((range, applies))
}
