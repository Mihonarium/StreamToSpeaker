//! What the app knows, as the model's facts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::core::{Core, World};
use crate::app::App;
use crate::model::facts::*;
use crate::model::state::{Place, SpeakerId};
use crate::model::text::{Key, Lang};
use crate::model::PcOutputId;
use crate::platform::Platform;

/// A speaker reached two ways (a Sonos speaker by its own protocol and by
/// AirPlay) is listed once per way. `true` lists it once, by the way with
/// less delay.
pub(crate) const MERGE_TWINS: bool = false;

/// How long the panel says it is looking before it says nothing was found.
const LOOKING_FOR: Duration = Duration::from_millis(crate::model::rules::CANT_FIND_AFTER_MS);

/// Every speaker the app knows, one entry per way of reaching it: a speaker
/// reached two ways is two entries that name each other as twins (one,
/// with [`MERGE_TWINS`]).
pub(crate) fn speaker_list(app: &App) -> Vec<SpeakerFacts> {
    let (settings, last) = {
        let uc = app.user_config.lock().unwrap();
        (uc.speaker_settings.clone(), uc.last_speaker_id.clone())
    };
    let row = |id: String, name: String, kind: SpeakerKind, address: String, presence: Presence| {
        let s = settings.get(&id);
        let status = app.delay_status_of(&id);
        SpeakerFacts {
            delay: Some(SpeakerDelay { own_ms: status.own_delay_ms, measured: false, change_ms: status.change_ms }),
            used_here: s.is_some() || last.as_deref() == Some(id.as_str()),
            hidden: s.is_some_and(|s| s.hidden),
            id: SpeakerId::new(&id),
            name,
            kind,
            address,
            presence,
            busy: false,
            failure: None,
            question: None,
            twin: None,
            last_used: s.and_then(|s| s.last_played),
        }
    };
    let mut out = Vec::new();
    let mut upnp_at: HashMap<IpAddr, usize> = HashMap::new();
    let mut upnp_id_at: HashMap<IpAddr, SpeakerId> = HashMap::new();
    if let Some(d) = app.discovery.as_ref() {
        for r in d.renderers() {
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
    if let Some(d) = app.airplay_discovery.as_ref() {
        for r in d.renderers() {
            let usable = r.transport().is_some();
            // One UPnP entry at the same address is the same speaker; it
            // plays with less delay. More than one, or a receiver claiming
            // another's identity: unsure, so separate rows.
            let twin = (usable && r.pair.is_none() && !r.is_newcomer() && upnp_at.get(&r.ip) == Some(&1))
                .then(|| upnp_id_at.get(&r.ip).cloned())
                .flatten();
            if twin.is_some() && MERGE_TWINS {
                continue;
            }
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

/// The model's facts for now.
/// How long a refusal from a device in the current speaker's group stays
/// on the notice line.
const REFUSAL_SHOWS_FOR: Duration = Duration::from_secs(10 * 60);

/// The notice line's facts.
fn notices(app: &App) -> NoticeFacts {
    let version = crate::display_version().to_string();
    let updated_to = {
        let uc = app.user_config.lock().unwrap();
        uc.last_version_seen.as_ref().filter(|v| **v != version).map(|_| version.clone())
    };
    let adapter_off = match app.discovery_net_status().scope {
        crate::discovery_net::Resolved::Paused { name } => Some(name),
        _ => None,
    };
    // A device in the playing speaker's group (a Sonos group member) that
    // asked for the sound and was refused.
    let group: Vec<std::net::IpAddr> = match app.session.lock().unwrap().as_ref() {
        Some(crate::app::ActiveSession::Upnp(r)) => r.renderer.stream_peers(),
        _ => Vec::new(),
    };
    let refused_in_group = app
        .recently_refused()
        .into_iter()
        .find(|r| {
            group.contains(&r.address) && r.at.elapsed().map(|e| e < REFUSAL_SHOWS_FOR).unwrap_or(false)
        })
        .map(|r| RefusedDevice { name: r.name, key: r.address.to_string() });
    NoticeFacts {
        restart_pending: false,
        adapter_off,
        refused_in_group,
        lab_on: app.lab_on(),
        update_ready: app.update_banner().map(|r| r.version.to_string()),
        updated_to,
        donation_due: app.should_show_donation_prompt(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn facts(
    app: &App,
    platform: &Platform,
    core: &Core,
    w: &World,
    now: Instant,
    started: Instant,
    mut speakers: Vec<SpeakerFacts>,
    first_run: Option<FirstRun>,
) -> Facts {
    let lang = Lang::English;
    let state = core.state(w, now);
    for s in speakers.iter_mut() {
        s.failure = core.failures.get(&s.id).copied();
        s.question = core.questions.get(&s.id).cloned();
    }
    // Every speaker the state or Windows' device names is in the list,
    // found or not.
    let named = match &core.named {
        Some(Place::Speaker(id)) => Some(id.clone()),
        _ => None,
    };
    for id in state.speaker().cloned().into_iter().chain(named) {
        if !speakers.iter().any(|s| s.id == id) {
            speakers.push(SpeakerFacts {
                name: app.speaker_name(id.as_str()).unwrap_or_else(|| id.as_str().to_string()),
                kind: SpeakerKind::Other { maker: None },
                address: String::new(),
                delay: None,
                presence: Presence::NotFound,
                used_here: true,
                hidden: false,
                busy: false,
                failure: None,
                question: None,
                twin: None,
                last_used: None,
                id,
            });
        }
    }

    let pc_id = core
        .pc_output
        .clone()
        .or_else(|| w.default_output.clone().filter(|d| Some(d) != w.our_output.as_ref()));
    let output = pc_id.as_ref().and_then(|id| platform.outputs.describe(id)).map(|o| o.to_model()).unwrap_or_else(|| {
        let this_pc = lang.get(Key::ThisPc, &[]);
        PcOutput {
            id: PcOutputId::new(""),
            friendly: this_pc.clone(),
            name: this_pc,
            device: String::new(),
            kind: OutputKind::Speakers,
        }
    });
    // Windows' outputs that are switched on, never this app's own.
    let outputs: Vec<PcOutput> = platform
        .outputs
        .list()
        .iter()
        .filter(|o| Some(&o.id) != w.our_output.as_ref())
        .map(|o| o.to_model())
        .collect();
    let outputs_open = app.user_config.lock().unwrap().pc_outputs_open;
    let pc_level = pc_id.as_ref().and_then(|id| platform.outputs.volume(id)).map(|(value, muted)| Level {
        value,
        muted,
        limit: None,
    });

    let place_level = match core.bound() {
        Some(Place::Speaker(id)) => app.vsync.current_level().map(|l| Level {
            value: l.min(100) as u8,
            muted: app.vsync.is_muted(),
            limit: app.volume_limit(id.as_str()),
        }),
        None => app.vsync.current_level().map(|l| Level { value: l.min(100) as u8, muted: app.vsync.is_muted(), limit: None }),
        Some(Place::MusicAssistant) => Some(Level::at(app.ma_level())),
        Some(Place::ThisPc) => None,
    };

    let delay = state.speaker().and_then(|id| app.delay_status().filter(|d| d.id == id.as_str()).and_then(|d| delay_facts(id, &d)));
    // The bound speaker's delay is what the delay engine says now.
    if let Some(d) = app.delay_status() {
        if let Some(s) = speakers.iter_mut().find(|s| s.id.as_str() == d.id) {
            s.delay = Some(SpeakerDelay { own_ms: d.own_delay_ms, measured: false, change_ms: d.change_ms });
        }
    }

    let hint_retired = app
        .user_config
        .lock()
        .unwrap()
        .speaker_settings
        .values()
        .any(|s| s.delay_change_ms != 0 || s.steady_delay_change_ms != 0 || s.delay_choice.is_some());

    let search = if w.found.is_empty() && (app.discovery.is_some() || app.airplay_discovery.is_some()) {
        SearchFacts {
            phase: if now.saturating_duration_since(started) < LOOKING_FOR {
                SearchPhase::Looking
            } else {
                SearchPhase::NoneFound
            },
            network: None,
            causes: Vec::new(),
        }
    } else {
        SearchFacts::default()
    };

    let remembered = match &core.named {
        Some(Place::Speaker(id)) => Remembered::Speaker {
            name: speakers
                .iter()
                .find(|s| &s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| id.as_str().to_string()),
        },
        Some(Place::MusicAssistant) => Remembered::MusicAssistant,
        _ => Remembered::Nothing,
    };

    Facts {
        lang,
        state: state.clone(),
        pc: PcFacts { output, outputs, outputs_open },
        speakers,
        music_assistant: app
            .is_sendspin_source_enabled()
            .then_some(MaFacts { connected: w.ma_connected, last_seen: None }),
        search,
        volume: VolumeFacts { pc: pc_level, place: place_level },
        delay,
        delay_steps: DelaySteps::default(),
        delay_hint_retired: hint_retired,
        mode: Mode::Everything,
        remembered,
        seen: core.seen,
        sending_sound: w.sending,
        notices: notices(app),
        notification: core.notification.clone(),
        first_run,
    }
}

/// The delay engine's status as the model's delay facts.
fn delay_facts(id: &SpeakerId, d: &crate::delay::DelayStatus) -> Option<DelayFacts> {
    use crate::delay::{Applies as A, Progress as P, Unchangeable as U};
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
    let short_ms = if matches!(d.kind, crate::delay::Kind::HomePod { .. }) { 200 } else { 300 };
    let choice = d.choice.map(|c| ChoiceFacts {
        current: match c {
            crate::delay::DelayChoice::Short => DelayChoice::Short,
            crate::delay::DelayChoice::Steady => DelayChoice::Steady,
        },
        steady_ms: 2000,
        short_ms,
    });
    let progress = match d.progress {
        P::Done => Progress::Settled,
        P::CatchingUp { ms_to_go } => Progress::Live { to_go_ms: ms_to_go },
        P::WillReconnect => Progress::ReconnectPending,
        P::Reconnecting { .. } => Progress::Reconnecting,
    };
    let note = d.notice.map(|n| match n {
        crate::delay::Notice::NotAccepted { tried_ms, back_to_ms } => DelayNote::NotAccepted { tried_ms, back_to_ms },
        crate::delay::Notice::DroppedOut { from_ms, to_ms } => DelayNote::DroppedOut { with_ms: from_ms, back_to_ms: to_ms },
        crate::delay::Notice::ChoiceSwitched { other_ms, .. } => DelayNote::ChoiceSwitched { other_kept_ms: other_ms },
    });
    Some(DelayFacts { speaker: id.clone(), range, applies, choice, progress, note })
}
