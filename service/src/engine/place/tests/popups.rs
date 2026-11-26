//! Pop-ups by the clock: what brings one, how long it shows, the Settings
//! switch, the open panel, and the two × buttons.

use super::rig::*;

/// Sound started on Kitchen after a break: **Playing on Kitchen** shows.
fn playing_after_a_break() -> Rig {
    let mut r = Rig::on_kitchen();
    r.wait_long(11 * 60_000);
    r.play(SPOTIFY);
    r.wait(4_000);
    r
}

/// Kitchen stopped responding during play; the app has just moved the sound
/// and its note shows by the clock.
fn moved() -> Rig {
    let mut r = Rig::playing_on_kitchen();
    r.speaker(K, SpeakerSignal::NotResponding);
    r.wait(10_250);
    assert!(matches!(popup(&r), Some(Popup::Note(_))), "set-up: the note by the clock");
    r
}

#[test]
fn popups_say_where_sound_is_heard_and_the_switch_turns_them_off() {
    run(&[
        Row {
            name: "Playing on Kitchen when sound starts on it after a break",
            given: || {
                let mut r = Rig::on_kitchen();
                r.wait_long(11 * 60_000);
                r
            },
            when: |r| {
                r.play(SPOTIFY);
                r.wait(4_000);
            },
            then: |r, _| {
                let p = popup(r).expect("a pop-up");
                assert_eq!(p, Popup::PlayingOn { speaker: id(K) });
                assert!(p.has_button(), "[Switch to this PC]");
            },
        },
        Row {
            name: "nothing when sound starts again within 10 minutes",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.pause_by_hand(SPOTIFY);
                r.wait_long(5 * 60_000);
                r
            },
            when: |r| {
                r.play(SPOTIFY);
                r.wait(4_000);
            },
            then: |r, _| assert!(popup(r).is_none()),
        },
        Row {
            name: "nothing right after a switch: somebody switched",
            given: || {
                let mut r = Rig::new();
                r.wait_long(60 * 60_000);
                r.switch_to(K);
                r
            },
            when: |r| {
                r.play(SPOTIFY);
                r.wait(4_000);
            },
            then: |r, _| assert!(popup(r).is_none()),
        },
        Row {
            name: "the Settings switch turns pop-ups about where sound plays off",
            given: || {
                let mut r = Rig::on_kitchen();
                r.w.settings.notes = false;
                r.wait_long(11 * 60_000);
                r
            },
            when: |r| {
                r.play(SPOTIFY);
                r.wait(4_000);
            },
            then: |r, _| assert!(popup(r).is_none()),
        },
        Row {
            name: "the switch leaves a failure on: when the app moves the sound, it says so",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.w.settings.notes = false;
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::PlayingElse { who: None }),
            then: |r, _| assert!(matches!(popup(r), Some(Popup::Note(Note { cause: Cause::PlayingElse { .. }, .. })))),
        },
        Row {
            name: "a pop-up with a button shows 15 seconds",
            given: playing_after_a_break,
            when: |r| r.wait(14_000),
            then: |r, _| assert!(popup(r).is_some()),
        },
        Row {
            name: "… and then goes",
            given: playing_after_a_break,
            when: |r| r.wait(16_000),
            then: |r, _| assert!(popup(r).is_none()),
        },
        Row {
            name: "a room joining the playing group: also playing there, and the situation says so while that sound lasts",
            given: Rig::playing_on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::JoinedGroup { room: "Bedroom".into() }),
            then: |r, _| {
                assert_eq!(popup(r), Some(Popup::AlsoPlayingIn { room: "Bedroom".into() }));
                assert!(r.sit().standing.contains(&Standing::RoomJoined { room: "Bedroom".into() }));
            },
        },
        Row {
            name: "… it has no button and shows 8 seconds",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::JoinedGroup { room: "Bedroom".into() });
                r.wait(7_000);
                assert!(popup(&r).is_some());
                r
            },
            when: |r| r.wait(2_000),
            then: |r, _| assert!(popup(r).is_none()),
        },
    ]);
}

#[test]
fn the_two_closing_buttons() {
    run(&[
        Row {
            name: "the pop-up's × closes only the pop-up: the note stays in the panel",
            given: moved,
            when: |r| {
                r.cmd_from(Surface::Notification, Command::ClosePopup).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Connect(_))));
                assert_eq!(popup(r), None);
                assert!(note(r).is_some());
            },
        },
        Row {
            name: "the note's × closes the note, and the pop-up that shows it",
            given: moved,
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
            },
            then: |r, dos| {
                assert!(dos.contains(&Do::Save));
                assert_eq!((note(r), popup(r), r.sit().amber), (None, None, false));
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))), "the sound stays where it is");
                assert_eq!(r.core.kept().note, None);
            },
        },
        Row {
            name: "… and nothing brings it back: the speaker answering changes no note",
            given: || {
                let mut r = moved();
                r.cmd(Command::CloseNote).unwrap();
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::Responding),
            then: |r, _| assert_eq!((note(r), popup(r)), (None, None)),
        },
        Row {
            name: "the note about a move seconds ahead has no ×: a close changes nothing",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(2_000);
                r
            },
            when: |r| {
                assert_eq!(note(r).map(|n| n.can_close()), Some(false));
                r.cmd(Command::CloseNote).unwrap();
            },
            then: |r, dos| {
                assert!(!dos.contains(&Do::Save));
                assert_eq!(note(r).and_then(|n| n.ahead), Some(Ahead::InAFewSeconds));
                assert_eq!(r.core.kept().closed_note, None);
                // The move comes, and its note with it.
                r.wait(8_250);
                assert!(matches!(popup(r), Some(Popup::Note(_))));
                assert_eq!(note(r).map(|n| (n.ahead, n.can_close())), Some((None, true)));
            },
        },
        Row {
            name: "the × of the note about a place that can't play closes it for as long as that stays so",
            given: || {
                let mut r = Rig::on_kitchen();
                r.speaker(K, SpeakerSignal::NotResponding);
                assert_eq!(note(&r).and_then(|n| n.ahead), Some(Ahead::WhenSoundStarts), "set-up: the note");
                r
            },
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
                r.wait(60_000);
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Connect(_))));
                let s = r.sit();
                assert_eq!((s.spot.clone(), s.note.clone()), (Spot::Speaker(id(K)), None));
                assert_eq!((s.away, row(r, K)), (Some(Away::NotResponding), RowState::NotResponding), "line 2 and the row go on saying it");
                assert!(r.core.kept().closed_note.is_some(), "kept, so that a restart doesn't say it again");
                // It answers and goes away again: that is said anew.
                r.speaker(K, SpeakerSignal::Responding);
                assert_eq!(r.core.kept().closed_note, None);
                r.speaker(K, SpeakerSignal::NotResponding);
                assert_eq!(note(r).and_then(|n| n.ahead), Some(Ahead::WhenSoundStarts));
            },
        },
        Row {
            name: "… and sound that starts then moves to this PC with its note all the same",
            given: || {
                let mut r = Rig::on_kitchen();
                r.speaker(K, SpeakerSignal::NotResponding);
                r.cmd(Command::CloseNote).unwrap();
                r
            },
            when: |r| r.play(SPOTIFY),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert!(matches!(cause(r), Some(Cause::NotRespondingAtStart { .. })));
                assert_eq!(note(r).map(|n| n.ahead), Some(None), "the app moved the sound");
                assert!(matches!(popup(r), Some(Popup::Note(_))));
            },
        },
        Row {
            name: "the × of the answer to a switch that didn't get ready closes it; the row goes on saying why",
            given: || {
                let mut r = Rig::new();
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::NotResponding);
                assert!(matches!(cause(&r), Some(Cause::DidntGetReady { .. })), "set-up: the note");
                r
            },
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
            },
            then: |r, _| {
                assert_eq!((note(r), r.sit().pick_failed), (None, None));
                assert_eq!(row(r, K), RowState::NotResponding);
            },
        },
        Row {
            name: "… together with the note that stood about the same speaker",
            given: || {
                let mut r = moved();
                r.open_panel();
                r.cmd(Command::TryAgain(id(K))).unwrap();
                r.end(Purpose::TryAgain, Ended::NotResponding);
                assert!(matches!(cause(&r), Some(Cause::DidntGetReady { .. })), "set-up: the answer shows first");
                r
            },
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
            },
            then: |r, dos| {
                assert!(dos.contains(&Do::Save));
                assert_eq!((note(r), r.core.kept().note), (None, None));
            },
        },
        Row {
            name: "… while a note that stood about another speaker is back",
            given: || {
                let mut r = moved();
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(B)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::NotFound);
                assert_eq!(cause(&r), Some(Cause::DidntGetReady { speaker: id(B), why: PickWhy::NotFound }), "set-up");
                r
            },
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
            },
            then: |r, _| assert!(matches!(cause(r), Some(Cause::StoppedResponding { ref speaker, .. }) if *speaker == id(K))),
        },
        Row {
            name: "neither exists outside the app's own surfaces",
            given: moved,
            when: |r| {
                for from in [Surface::Phone, Surface::CommandLine, Surface::Api, Surface::Shortcut] {
                    assert_eq!(r.cmd_from(from, Command::CloseNote), Err(Refusal::WindowOnly));
                    assert_eq!(r.cmd_from(from, Command::ClosePopup), Err(Refusal::WindowOnly));
                }
            },
            then: |r, _| assert!(note(r).is_some() && popup(r).is_some()),
        },
    ]);
}

#[test]
fn one_popup_and_one_confirmation_at_most() {
    let mut r = Rig::playing_on_kitchen();
    r.speaker(K, SpeakerSignal::PlayingElse { who: None });
    let first = r.sit().popups.popup.expect("the note by the clock");
    // The same happens on Bedroom: the newer pop-up replaces the older one.
    // A confirmation can show beside it (a screen reader runs, so a switch
    // asks first).
    r.switch_to(B);
    r.wait(4_000);
    r.speaker(B, SpeakerSignal::PlayingElse { who: None });
    r.w.screen_reader = true;
    r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
    let s = r.sit();
    let second = s.popups.popup.expect("a pop-up");
    assert!(second.seq > first.seq);
    assert!(matches!(&second.what, Popup::Note(Note { cause: Cause::PlayingElse { speaker, .. }, .. }) if *speaker == id(B)));
    assert!(matches!(s.note, Some(Note { cause: Cause::PlayingElse { ref speaker, .. }, .. }) if *speaker == id(B)), "one note");
    assert!(matches!(s.popups.confirmation.map(|c| c.what), Some(Confirm::ScreenReader { .. })), "a confirmation beside it");
    // The confirmation waits 30 minutes at most; the note has no clock.
    r.wait_long(31 * 60_000);
    let s = r.sit();
    assert_eq!(s.popups, Popups::default());
    assert!(s.note.is_some());
}

#[test]
fn nothing_shows_by_the_clock_while_the_panel_is_open() {
    run(&[
        Row {
            name: "opening the panel closes the pop-up; the panel's note says the same",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| r.open_panel(),
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.popups, Popups::default());
                assert!(matches!(s.note, Some(Note { cause: Cause::PlayingElse { .. }, .. })));
            },
        },
        Row {
            name: "a confirmation by the clock moves under its speaker's row",
            given: || {
                let mut r = Rig::new();
                r.w.screen_reader = true;
                r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                assert!(r.sit().popups.confirmation.is_some(), "set-up: it asks by the clock");
                r
            },
            when: |r| r.open_panel(),
            then: |r, _| {
                let s = r.sit();
                assert!(s.popups.confirmation.is_none());
                assert!(matches!(s.confirm_in_panel, Some(Confirm::ScreenReader { .. })));
            },
        },
        Row {
            name: "nothing new shows by the clock while it is open, and nothing waits for it to close",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.open_panel();
                r
            },
            when: |r| {
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r.close_panel();
            },
            then: |r, _| {
                assert_eq!(popup(r), None);
                assert!(note(r).is_some(), "the panel's note stands");
            },
        },
        Row {
            name: "a switch that didn't get ready shows until the panel closes after showing it",
            given: || {
                let mut r = Rig::new();
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::NotFound);
                r
            },
            when: |r| {
                assert!(r.sit().pick_failed.is_some());
                r.close_panel();
            },
            then: |r, _| assert!(r.sit().pick_failed.is_none()),
        },
    ]);
}

#[test]
fn players_are_matched_by_app() {
    let mut r = Rig::on_kitchen();
    // Chrome plays through several processes; its sound is Chrome's.
    r.play(CHROME);
    r.wait(4_000);
    // A player that isn't sounding on the app's device is left alone.
    r.w.players.push(PlayerWorld {
        id: PlayerId::new("vlc"),
        app: AppKey::new("vlc"),
        name: Some("VLC".into()),
        status: PlayerStatus::Playing,
    });
    r.take();
    r.speaker(K, SpeakerSignal::PausedAtSpeaker);
    let dos = r.take();
    assert!(paused(&dos, CHROME));
    assert!(!paused(&dos, "vlc"));
}

#[test]
fn closing_the_window_explains_the_tray_once_across_restarts() {
    let mut r = Rig::on_kitchen();
    r.open_panel();
    r.close_panel();
    r.w.settings.notes = false;
    r.cmd(Command::WindowClosed).unwrap();
    assert_eq!(popup(&r), Some(Popup::WindowClosed));
    assert_eq!(r.core.kept().place, Some(id(K)));
    let saved = serde_json::to_string(&r.core.kept()).unwrap();
    r.wait(crate::engine::place::numbers::POPUP_WITH_BUTTON_MS - 1);
    assert_eq!(popup(&r), Some(Popup::WindowClosed));
    r.wait(1);
    assert_eq!(popup(&r), None);
    r.cmd(Command::WindowClosed).unwrap();
    assert_eq!(popup(&r), None);
    let mut restarted = Rig::with(serde_json::from_str(&saved).unwrap(), world());
    restarted.cmd(Command::WindowClosed).unwrap();
    assert_eq!(popup(&restarted), None);
}

#[test]
fn a_new_playback_popup_replaces_the_window_hint() {
    let mut r = Rig::playing_on_kitchen();
    r.cmd(Command::ClosePopup).unwrap();
    r.cmd(Command::WindowClosed).unwrap();
    assert_eq!(popup(&r), Some(Popup::WindowClosed));
    r.speaker(K, SpeakerSignal::JoinedGroup { room: "Bedroom".into() });
    assert_eq!(popup(&r), Some(Popup::AlsoPlayingIn { room: "Bedroom".into() }));
}
