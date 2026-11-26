//! The app's life: start, sleep, wake, sign-out, shutdown, quit, updates,
//! crashes, and what is kept.

use super::rig::*;

fn kept_on_kitchen() -> Kept {
    Kept { place: Some(id(K)), named: Some(id(K)), first_run_done: true, ..Kept::default() }
}

#[test]
fn at_a_start_the_app_looks_at_what_plays_on_this_pc() {
    run(&[
        Row {
            name: "a call plays: the sound stays where it plays, as every kind of sound does",
            given: || {
                let mut r = Rig::starting(kept_on_kitchen(), world(), StartKind::Normal, false);
                r.sound("Teams");
                r
            },
            when: |r| r.wait(3_500),
            then: |r, dos| {
                assert!(!to_ours(dos) && asks_no_player(dos));
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))));
                assert_eq!(cause(r), Some(Cause::PlayingAtStart { speaker: id(K) }));
                assert_eq!(r.core.kept().place, None, "This PC is the place until a person switches");
            },
        },
        Row {
            name: "with the notes switch off the note is in the panel only",
            given: || {
                let mut r = Rig::starting(kept_on_kitchen(), world(), StartKind::Normal, false);
                r.w.settings.notes = false;
                r.play(SPOTIFY);
                r
            },
            when: |r| r.wait(3_500),
            then: |r, _| {
                assert_eq!(popup(r), None);
                assert_eq!(cause(r), Some(Cause::PlayingAtStart { speaker: id(K) }));
            },
        },
        Row {
            name: "nothing plays: Windows moves to the app's device, nothing connects",
            given: || Rig::starting(kept_on_kitchen(), world(), StartKind::Normal, false),
            when: |r| r.wait(3_500),
            then: |r, dos| {
                assert!(to_ours(dos));
                assert!(dos.iter().all(|d| !matches!(d, Do::Connect(_))));
                assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
                assert!(r.sit().note.is_none());
            },
        },
    ]);
}

#[test]
fn after_a_crash_or_update_the_app_goes_on_at_the_speaker_after_a_look() {
    fn after(kind: StartKind) -> Rig {
        let mut w = world();
        w.default_output = Some(OutputRef::Ours);
        let mut r = Rig::starting(kept_on_kitchen(), w, kind, false);
        r.w.players.push(PlayerWorld {
            id: PlayerId::new(SPOTIFY),
            app: AppKey::new(SPOTIFY),
            name: Some(SPOTIFY.into()),
            status: PlayerStatus::Playing,
        });
        r.sources.push(Source { app: AppKey::new(SPOTIFY), player: Some(PlayerId::new(SPOTIFY)), output: None });
        r
    }
    run(&[
        Row {
            name: "after an update the sound goes on at the speaker, looking first",
            given: || after(StartKind::AfterUpdate),
            when: |r| r.wait(500),
            then: |r, dos| {
                assert!(!to_speakers(dos), "Windows stays on the app's device");
                assert!(asks_no_player(dos));
                let j = r.job(Purpose::SoundStarted);
                assert!(j.look_first);
            },
        },
        Row {
            name: "the look finds it busy: this PC, where what plays carries on",
            given: || {
                let mut r = after(StartKind::AfterCrash);
                r.wait(500);
                r
            },
            when: |r| r.end(Purpose::SoundStarted, Ended::Busy { who: None }),
            then: |r, dos| {
                assert!(to_speakers(dos) && asks_no_player(dos));
                assert_eq!(r.status(SPOTIFY), PlayerStatus::Playing);
                let want = Note { cause: Cause::PlayingElse { speaker: id(K), who: None }, at: None, available_again: false, ahead: None };
                assert_eq!(note(r), Some(want));
            },
        },
        Row {
            name: "the watcher gave up: the start begins on this PC",
            given: || {
                let mut w = world();
                w.default_output = Some(OutputRef::Pc(out("speakers")));
                Rig::starting(kept_on_kitchen(), w, StartKind::AfterGivingUp, true)
            },
            when: |_| {},
            then: |r, _| {
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
                assert_eq!(r.core.kept().place, None);
            },
        },
    ]);
}

#[test]
fn quit_signout_shutdown_put_windows_on_the_pcs_own_output() {
    run(&[
        Row {
            name: "quitting gives Windows the PC's own output and makes This PC the place",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.cmd(Command::Quit).unwrap();
            },
            then: |r, dos| {
                assert!(asks_no_player(dos));
                assert!(dos.iter().any(|d| matches!(d, Do::Silence { then: Some(WinTarget::Output(o)), .. } if *o == out("speakers"))));
                assert_eq!(r.core.kept().place, None, "quitting ends the switch");
            },
        },
        Row {
            name: "quitting while a note stands keeps the note for the next start",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| {
                r.cmd(Command::Quit).unwrap();
            },
            then: |r, dos| {
                assert!(asks_no_player(dos) && !dos.iter().any(|d| matches!(d, Do::Windows(_) | Do::Silence { .. })));
                assert!(matches!(r.core.kept().note.map(|n| n.note.cause), Some(Cause::PlayingElse { .. })));
            },
        },
        Row {
            name: "signing out keeps the switch, and gives Windows the PC's own output",
            given: Rig::playing_on_kitchen,
            when: |r| r.event(Event::Power(PowerEvent::SigningOut)),
            then: |r, dos| {
                assert!(to_speakers(dos) && asks_no_player(dos));
                assert_eq!(r.core.kept().place, Some(id(K)));
            },
        },
        Row {
            name: "shutting down the same",
            given: Rig::on_kitchen,
            when: |r| r.event(Event::Power(PowerEvent::ShuttingDown)),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert_eq!(r.core.kept().place, Some(id(K)));
            },
        },
        Row {
            name: "an ordinary update leaves Windows on the app's device",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.cmd(Command::ReleaseWindows { for_update: true, new_driver: false }).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Silence { .. } | Do::Pause(_))));
                assert!(dos.contains(&Do::Save));
                assert_eq!(r.core.kept().place, Some(id(K)));
            },
        },
        Row {
            name: "an update that replaces the sound driver: the same, and nothing is paused",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.cmd(Command::ReleaseWindows { for_update: true, new_driver: true }).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Silence { .. } | Do::Pause(_))));
                assert_eq!(r.core.kept().place, Some(id(K)));
            },
        },
    ]);
}

#[test]
fn sleep_keeps_windows_on_the_apps_device() {
    let mut r = Rig::playing_on_kitchen();
    r.pause_by_hand(SPOTIFY);
    r.event(Event::Power(PowerEvent::Suspending));
    let dos = r.take();
    assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Silence { then: Some(_), .. })));
    assert_eq!(r.w.default_output, Some(OutputRef::Ours));
    r.now = r.now.later(3_600_000);
    r.w.connections.clear();
    r.event(Event::Power(PowerEvent::Resumed));
    r.event(Event::Network);
    // For a minute nothing calls Kitchen away on its own.
    r.speaker(K, SpeakerSignal::NotResponding);
    let s = r.sit();
    assert_eq!(s.spot, Spot::Speaker(id(K)));
    assert_eq!(s.away, None);
    assert_eq!(row(&r, K), RowState::Plain);
}

#[test]
fn the_try_at_a_start_waits_for_the_network_after_a_wake() {
    let mut r = Rig::playing_on_kitchen();
    r.pause_by_hand(SPOTIFY);
    r.event(Event::Power(PowerEvent::Suspending));
    r.now = r.now.later(3_600_000);
    r.w.connections.clear();
    r.event(Event::Power(PowerEvent::Resumed));
    r.event(Event::Network);
    r.take();
    r.play(SPOTIFY);
    let dos = r.take();
    assert!(asks_no_player(&dos), "nothing is held while it waits");
    assert!(!connects(&dos, Purpose::SoundStarted), "no try without a network");
    assert_eq!(r.sit().doing, Doing::Connecting);
    r.wait(5_000);
    r.w.connections = world().connections;
    r.event(Event::Network);
    assert!(connects(&r.take(), Purpose::SoundStarted), "the one try once the network is there");
    r.end(Purpose::SoundStarted, Ended::Ready);
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
    assert!(asks_no_player(&r.take()));
    // Without a connection within about 10 s, the app tries once anyway.
    let mut r = Rig::playing_on_kitchen();
    r.pause_by_hand(SPOTIFY);
    r.event(Event::Power(PowerEvent::Suspending));
    r.w.connections.clear();
    r.event(Event::Power(PowerEvent::Resumed));
    r.event(Event::Network);
    r.play(SPOTIFY);
    r.wait(9_000);
    assert!(!r.has_job(Purpose::SoundStarted));
    r.wait(1_250);
    assert!(r.has_job(Purpose::SoundStarted));
}

#[test]
fn what_is_kept_across_a_restart() {
    let mut r = Rig::playing_on_kitchen();
    r.cmd(Command::ChooseWay { speaker: id(OFFICE), way: WayChoice::AirPlayShort }).unwrap();
    r.speaker(K, SpeakerSignal::PlayingElse { who: None });
    let kept = r.core.kept();
    assert_eq!(kept.place, None);
    assert_eq!(kept.ways.get(&id(OFFICE)), Some(&WayChoice::AirPlayShort));
    assert_eq!(kept.ways.get(&id(K)), Some(&WayChoice::Native));
    assert!(matches!(kept.note.as_ref().map(|n| &n.note.cause), Some(Cause::PlayingElse { .. })));
    assert!(kept.last_sound.is_some());
    let text = serde_json::to_string(&kept).unwrap();
    let back: Kept = serde_json::from_str(&text).unwrap();
    assert_eq!(back, kept);
    // After the restart the note stands as it was, with its time.
    let before = note(&r);
    let again = Rig::starting_at(r.now.later(5_000), back, world(), StartKind::Normal, true);
    assert!(matches!(again.sit().note, Some(Note { cause: Cause::PlayingElse { .. }, at: Some(_), .. })));
    assert_eq!(again.sit().note.map(|n| (n.cause, n.at)), before.map(|n| (n.cause, n.at)));
    assert_eq!(popup(&again), None, "nothing is said by the clock again");
    // The speaker that was the place before headphones is kept with them.
    let mut r = Rig::on_kitchen();
    r.plug_headphones(true);
    let kept = r.core.kept();
    let text = serde_json::to_string(&kept).unwrap();
    let back: Kept = serde_json::from_str(&text).unwrap();
    assert_eq!(back, kept);
    assert_eq!(back.place, None);
    assert_eq!(back.headphones.map(|h| h.before), Some(id(K)));
}

#[test]
fn an_older_kept_record_reads_with_defaults() {
    // An older version wrote fewer fields.
    let old: Kept = serde_json::from_str(r#"{"place":"kitchen","ways":{"kitchen":"Native"}}"#).unwrap();
    assert_eq!(old.place, Some(id(K)));
    assert!(old.levels.is_empty() && !old.first_run_done && old.note.is_none());
    let empty: Kept = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, Kept::default());
    // A newer version wrote fields and values this one doesn't know: the
    // settings file reads value by value, so only those are lost.
    let newer = serde_json::json!({
        "place": "kitchen",
        "a_field_from_later": 7,
        "ways": {"kitchen": "SomeWayFromLater", "office": "AirPlayShort"},
        "note": {"note": {"cause": {"SomethingNew": {}}, "at": [21, 14], "available_again": false}, "at": 5},
        "first_run_done": true
    });
    let (read, lost) = crate::lenient_json::from_value::<Kept>(&newer).unwrap();
    assert_eq!(read.place, Some(id(K)));
    assert!(read.first_run_done);
    assert_eq!(read.ways.get(&id(OFFICE)), Some(&WayChoice::AirPlayShort));
    assert!(read.note.is_none());
    assert!(!lost.is_empty());
}
