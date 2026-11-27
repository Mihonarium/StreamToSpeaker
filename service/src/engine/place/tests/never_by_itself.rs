//! The app never moves the sound to a speaker by itself, and never pauses
//! or starts a player by itself: every event, in every moment. The only way
//! a speaker becomes the place is a person's switch (a command, or the
//! app's device chosen in Windows' list), and the only pause or play the
//! app asks of a player is one pressed on the place's speaker itself.

use super::rig::*;

/// The moments to walk through.
fn moments() -> Vec<(&'static str, Rig)> {
    let mut list: Vec<(&'static str, Rig)> = Vec::new();
    list.push(("This PC, nothing playing", Rig::new()));
    list.push(("This PC, playing", {
        let mut r = Rig::new();
        r.play(SPOTIFY);
        r.wait(4_000);
        r
    }));
    list.push(("Kitchen, nothing playing", Rig::on_kitchen()));
    list.push(("Kitchen, playing", Rig::playing_on_kitchen()));
    list.push(("Kitchen, resting", {
        let mut r = Rig::on_kitchen();
        r.wait_long(31 * 60_000);
        r
    }));
    list.push(("Kitchen, the tries", {
        let mut r = Rig::playing_on_kitchen();
        r.speaker(K, SpeakerSignal::NotResponding);
        r.wait(3_000);
        r
    }));
    list.push(("Kitchen, connecting as sound starts", {
        let mut r = Rig::on_kitchen();
        r.wait_long(31 * 60_000);
        r.play(SPOTIFY);
        r
    }));
    list.push(("the app moved the sound off Kitchen", {
        let mut r = Rig::playing_on_kitchen();
        r.speaker(K, SpeakerSignal::NotResponding);
        r.wait(10_250);
        r
    }));
    list.push(("the app moved the sound off Kitchen, available again", {
        let mut r = Rig::playing_on_kitchen();
        r.speaker(K, SpeakerSignal::PlayingElse { who: None });
        r.speaker(K, SpeakerSignal::Free);
        r
    }));
    list.push(("Kitchen away, nothing playing", {
        let mut r = Rig::on_kitchen();
        r.speaker(K, SpeakerSignal::NotResponding);
        r
    }));
    list.push(("headphones, This PC before them", {
        let mut r = Rig::new();
        r.plug_headphones(true);
        r
    }));
    list.push(("headphones, Kitchen playing before them", {
        let mut r = Rig::playing_on_kitchen();
        r.plug_headphones(true);
        r.wait(4_000);
        r
    }));
    list.push(("headphones went, Kitchen before them", {
        let mut r = Rig::playing_on_kitchen();
        r.plug_headphones(true);
        r.wait(4_000);
        r.unplug_headphones();
        r
    }));
    list.push(("the app started while sound played on this PC", {
        let kept = Kept { place: Some(id(K)), named: Some(id(K)), first_run_done: true, ..Kept::default() };
        let mut r = Rig::starting(kept, world(), StartKind::Normal, false);
        r.play(SPOTIFY);
        r.wait(3_500);
        r
    }));
    list.push(("a confirmation by the clock waits", {
        let mut r = Rig::new();
        r.w.screen_reader = true;
        r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
        assert!(r.sit().popups.confirmation.is_some(), "set-up: it asks by the clock");
        r
    }));
    list.push(("first run, step 2", {
        let mut r = Rig::with(Kept::default(), world());
        r.cmd(Command::FirstRunAction(FirstRunAction::Chime(id(K)))).unwrap();
        r
    }));
    list.push(("the Settings option switched to this PC", {
        let mut r = Rig::on_kitchen();
        r.w.settings.switch_back_after_quiet = true;
        r.wait_long(31 * 60_000);
        r
    }));
    list
}

/// Events that come without a person's act.
fn events(r: &Rig) -> Vec<(String, Event)> {
    let mut list = Vec::new();
    for s in [K, B, OFFICE] {
        for signal in [
            SpeakerSignal::Responding,
            SpeakerSignal::PlayingOurStream,
            SpeakerSignal::NotResponding,
            SpeakerSignal::Silenced,
            SpeakerSignal::AsksSecret(Secret::Code),
            SpeakerSignal::PlayingElse { who: None },
            SpeakerSignal::Free,
            SpeakerSignal::PausedAtSpeaker,
            SpeakerSignal::StoppedAtSpeaker,
            SpeakerSignal::PlayPressed,
            SpeakerSignal::EndedStream { reachable: true },
            SpeakerSignal::EndedStream { reachable: false },
            SpeakerSignal::Announced,
            SpeakerSignal::Unannounced,
            SpeakerSignal::JoinedGroup { room: "Den".into() },
            SpeakerSignal::GroupedInto { with: "Den".into() },
            SpeakerSignal::LeftGroup,
        ] {
            list.push((format!("{s}: {signal:?}"), Event::Speaker { id: id(s), signal }));
        }
    }
    for j in &r.jobs {
        for ended in [Ended::Ready, Ended::NotResponding, Ended::Busy { who: None }, Ended::NeedsSecret(Secret::Password)] {
            if j.purpose == Purpose::Switch || j.purpose == Purpose::TryAgain {
                // The end of a person's switch: it may take effect.
                continue;
            }
            list.push((format!("{:?} ends {ended:?}", j.purpose), Event::Connected { gen: j.gen, ended }));
        }
    }
    let t = r.now.mono_ms;
    list.push((
        "headphones arrive".into(),
        Event::Output(OutputEvent::Added { id: out("hp2"), kind: OutputKind::Headphones, name: "HP".into(), at_ms: t }),
    ));
    list.push(("Windows' default changes".into(), Event::Output(OutputEvent::DefaultChanged { at_ms: t })));
    list.push(("a change not taken".into(), Event::Output(OutputEvent::NotTaken { at_ms: t })));
    list.push(("the speakers go".into(), Event::Output(OutputEvent::Removed { id: out("speakers"), at_ms: t })));
    list.push(("the headphones go".into(), Event::Output(OutputEvent::Removed { id: out("headphones"), at_ms: t })));
    list.push(("a player starts".into(), Event::Player(PlayerEvent::Status { id: PlayerId::new(SPOTIFY), status: PlayerStatus::Playing })));
    list.push(("a player is touched".into(), Event::Player(PlayerEvent::Other { id: PlayerId::new(SPOTIFY) })));
    list.push(("the network changes".into(), Event::Network));
    for p in [PowerEvent::Suspending, PowerEvent::Resumed, PowerEvent::SigningOut, PowerEvent::ShuttingDown] {
        list.push((format!("{p:?}"), Event::Power(p)));
    }
    for k in [StartKind::Normal, StartKind::AfterUpdate, StartKind::AfterCrash, StartKind::AfterGivingUp] {
        list.push((format!("started {k:?}"), Event::Started(k)));
    }
    list.push((
        "a sound".into(),
        Event::Sound(vec![Level { app: AppKey::new(GAME), output: OutputRef::Ours, peak: 0.2, system: false }]),
    ));
    list
}

/// The pause and the play pressed on a speaker itself: the only events on
/// which the app asks anything of a player.
fn speakers_own_pause(e: &Event) -> bool {
    matches!(
        e,
        Event::Speaker {
            signal: SpeakerSignal::PausedAtSpeaker
                | SpeakerSignal::StoppedAtSpeaker
                | SpeakerSignal::EndedStream { reachable: true },
            ..
        }
    )
}

fn speakers_own_play(e: &Event) -> bool {
    matches!(e, Event::Speaker { signal: SpeakerSignal::PlayPressed | SpeakerSignal::PlayingOurStream, .. })
}

#[test]
fn the_app_never_chooses_a_speaker_by_itself() {
    let mut walked = 0;
    for (moment, _) in moments() {
        let count = {
            let (_, r) = moments().into_iter().find(|(m, _)| *m == moment).unwrap();
            events(&r).len()
        };
        for i in 0..count {
            for later in [0u64, 1_000, 25_000, 31 * 60_000, 4 * 3_600_000] {
                let (_, mut r) = moments().into_iter().find(|(m, _)| *m == moment).unwrap();
                let (what, e) = events(&r).swap_remove(i);
                r.take();
                let before = r.sit().spot;
                let place_before = r.core.kept().place;
                let switching = r.sit().picking.is_some();
                let (own_pause, own_play) = (speakers_own_pause(&e), speakers_own_play(&e));
                r.event(e);
                if later > 0 {
                    r.wait_long(later);
                }
                let after = r.sit().spot;
                let dos = r.take();
                walked += 1;
                let became_speaker = matches!(&after, Spot::Speaker(_)) && after != before;
                // A switch a person made may take effect; nothing else makes
                // a speaker the place.
                if became_speaker && !switching {
                    panic!("{moment}, then {what} (+{later} ms): {before:?} became {after:?}\n  asked: {dos:?}");
                }
                for d in &dos {
                    match d {
                        Do::Connect(j) => assert!(
                            !matches!(j.purpose, Purpose::Switch | Purpose::TryAgain | Purpose::Way),
                            "{moment}, then {what} (+{later} ms): a switch nobody made: {j:?}\n  asked: {dos:?}"
                        ),
                        Do::Pause(_) => assert!(
                            own_pause,
                            "{moment}, then {what} (+{later} ms): the app paused a player by itself\n  asked: {dos:?}"
                        ),
                        Do::Play(_) => assert!(
                            own_play && trials::T3_PLAY_ON_SPEAKER_STARTS_PLAYER,
                            "{moment}, then {what} (+{later} ms): the app started a player by itself\n  asked: {dos:?}"
                        ),
                        _ => {}
                    }
                }
                // The core never asks to be woken for a moment already past
                // (the engine would spin).
                assert!(r.core.next_due().is_none_or(|t| t > r.now.mono_ms), "{moment}, then {what}: due in the past");
                let place_after = r.core.kept().place;
                // The app connects by itself only the place's speaker (the
                // place may have changed since, when the try failed).
                let connects_elsewhere = dos.iter().any(|d| {
                    matches!(d, Do::Connect(j) if j.purpose != Purpose::Chime
                        && Some(&j.speaker) != place_after.as_ref()
                        && Some(&j.speaker) != place_before.as_ref())
                });
                assert!(!connects_elsewhere, "{moment}, then {what}: a connection to a speaker that isn't the place: {dos:?}");
            }
        }
    }
    assert!(walked > 1_000, "walked {walked}");
}
