//! A person's switches, confirmations, the ways, the chime and
//! first run.

use super::rig::*;

#[test]
fn a_switch_stays_however_long_nothing_plays() {
    let mut rig = Rig::on_kitchen();
    // A day without sound, a sleep, a restart of the app.
    rig.wait_long(24 * 3_600_000);
    assert_eq!(rig.sit().spot, Spot::Speaker(id(K)));
    rig.event(Event::Power(PowerEvent::Suspending));
    rig.now = rig.now.later(8 * 3_600_000);
    rig.event(Event::Power(PowerEvent::Resumed));
    rig.wait(61_000);
    assert_eq!(rig.sit().spot, Spot::Speaker(id(K)));
    let mut again = rig.restart(world(), StartKind::Normal);
    again.wait(61_000);
    assert_eq!(again.sit().spot, Spot::Speaker(id(K)), "across a restart");
    assert_eq!(again.sit().doing, Doing::NothingPlaying);
    // The stream rested after 30 quiet minutes; the place didn't change.
    assert!(rig.out.iter().any(|d| matches!(d, Do::StopSending { take_off: true, .. })));
}

#[test]
fn a_switch_tries_once_and_the_last_switch_wins() {
    run(&[
        Row {
            name: "a switch connects once and the sound stays until it is ready",
            given: Rig::new,
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert_eq!(dos.iter().filter(|d| matches!(d, Do::Connect(_))).count(), 1);
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))), "still on this PC");
                assert_eq!(s.picking.as_ref().map(|p| p.speaker.clone()), Some(id(K)));
                assert_eq!(row(r, K), RowState::Connecting);
                assert!(!to_ours(dos), "Windows doesn't move before the speaker is ready");
            },
        },
        Row {
            name: "a newer switch cancels the one still connecting",
            given: || {
                let mut r = Rig::new();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(B)), way: None }).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().any(|d| matches!(d, Do::Supersede(_))));
                assert_eq!(r.sit().picking.map(|p| p.speaker), Some(id(B)));
                assert!(!r.jobs.iter().any(|j| j.speaker == id(K)), "Kitchen's connection is no longer wanted");
            },
        },
        Row {
            name: "a switch that doesn't get ready changes nothing and says why",
            given: || {
                let mut r = Rig::new();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| r.end(Purpose::Switch, Ended::NotResponding),
            then: |r, dos| {
                assert!(!to_ours(dos));
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))));
                assert_eq!(s.pick_failed.as_ref().map(|p| p.why), Some(PickWhy::NotResponding));
                assert_eq!(row(r, K), RowState::NotResponding, "the row shows what its try found");
                assert!(popup(r).is_none(), "no pop-up for a switch from the panel");
            },
        },
        Row {
            name: "when it is ready it takes effect: Windows moves to the app's device",
            given: || {
                let mut r = Rig::new();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| r.end(Purpose::Switch, Ended::Ready),
            then: |r, dos| {
                assert!(to_ours(dos));
                assert!(dos.contains(&Do::Save));
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(id(K)));
                assert!(s.picking.is_none() && s.note.is_none());
                assert_eq!(r.core.named(), Some(&id(K)), "Windows' device is named after it");
            },
        },
        Row {
            name: "the row of the place does nothing",
            given: Rig::on_kitchen,
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |_, dos| assert!(dos.iter().all(|d| !matches!(d, Do::Connect(_)))),
        },
        Row {
            name: "a switch to this PC makes the speaker silent before Windows moves",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().any(|d| matches!(d, Do::Silence { speaker, then: Some(WinTarget::Output(o)) } if *speaker == id(K) && *o == out("speakers"))));
                assert!(asks_no_player(dos), "a switch pauses nothing");
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
            },
        },
        Row {
            name: "a receiver that announces another speaker's identity plays only from the app's windows",
            given: || {
                let mut r = Rig::new();
                r.w.speakers[1].only_from_window = true;
                r
            },
            when: |r| {
                let refused = r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(B)), way: None });
                assert_eq!(refused, Err(Refusal::OnlyFromWindow { speaker: id(B) }));
            },
            then: |_, dos| assert!(!connects(dos, Purpose::Switch)),
        },
    ]);
}

#[test]
fn a_busy_speaker_is_taken_without_a_question() {
    fn busy() -> Rig {
        let mut r = Rig::new();
        r.speaker(K, SpeakerSignal::PlayingElse { who: Some("LAPTOP-7Q2K".into()) });
        assert_eq!(row(&r, K), RowState::PlayingElse, "set-up: its row says so before the click");
        r
    }
    run(&[
        Row {
            name: "a click on a busy speaker's row connects at once, and stops what plays there",
            given: busy,
            when: |r| {
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert!(!r.job(Purpose::Switch).look_first, "it doesn't look first: it takes the speaker");
                let s = r.sit();
                assert_eq!((s.confirm_in_panel.clone(), s.popups.confirmation.clone()), (None, None));
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))), "the sound stays until Kitchen is ready");
                r.end(Purpose::Switch, Ended::Ready);
                assert_eq!((r.sit().spot, row(r, K)), (Spot::Speaker(id(K)), RowState::Plain));
            },
        },
        Row {
            name: "from the tray, the phone and the API too",
            given: busy,
            when: |r| {
                for from in [Surface::Tray, Surface::Phone, Surface::Api] {
                    assert_eq!(r.cmd_from(from, Command::Switch { place: Place::Speaker(id(K)), way: None }), Ok(Effect::Done));
                    assert!(!r.job(Purpose::Switch).look_first);
                    r.cmd(Command::CancelSwitch).unwrap();
                }
            },
            then: |r, _| {
                let s = r.sit();
                assert_eq!((s.confirm_in_panel.clone(), s.popups.confirmation.clone()), (None, None));
            },
        },
        Row {
            name: "a speaker that may be in use is taken the same way",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.switch_to(OFFICE);
                r.speaker(OFFICE, SpeakerSignal::EndedStream { reachable: true });
                r.open_panel();
                r
            },
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
            },
            then: |r, _| {
                assert!(r.sit().confirm_in_panel.is_none());
                assert!(!r.job(Purpose::Switch).look_first);
            },
        },
        Row {
            name: "a speaker that refuses all the same didn't get ready: nothing changes",
            given: || {
                let mut r = busy();
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| r.end(Purpose::Switch, Ended::Busy { who: None }),
            then: |r, dos| {
                assert!(!to_ours(dos));
                let s = r.sit();
                assert_eq!((s.spot.clone(), s.confirm_in_panel.clone()), (Spot::ThisPc(Some(out("speakers"))), None));
                assert_eq!(cause(r), Some(Cause::DidntGetReady { speaker: id(K), why: PickWhy::Refused }));
                assert_eq!(row(r, K), RowState::PlayingElse);
            },
        },
        Row {
            name: "while a screen reader runs a switch still asks, once per speaker",
            given: || {
                let mut r = busy();
                r.w.screen_reader = true;
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| {
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::ScreenReader { .. })));
                assert!(!r.has_job(Purpose::Switch), "nothing connects until it is answered");
                r.cmd(Command::Confirm { yes: true }).unwrap();
                assert!(!r.job(Purpose::Switch).look_first);
                r.end(Purpose::Switch, Ended::Ready);
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, _| {
                assert!(r.sit().confirm_in_panel.is_none(), "answered once for Kitchen");
                assert!(r.has_job(Purpose::Switch));
            },
        },
        Row {
            name: "a surface that can't ask is refused where a switch has to ask",
            given: || {
                let mut r = Rig::new();
                r.w.screen_reader = true;
                r
            },
            when: |r| {
                let result = r.cmd_from(Surface::Api, Command::Switch { place: Place::Speaker(id(K)), way: None });
                assert_eq!(result, Err(Refusal::OnlyFromWindow { speaker: id(K) }));
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Connect(_))));
                assert!(r.sit().confirm_in_panel.is_none() && r.sit().popups.confirmation.is_none());
            },
        },
        Row {
            name: "with Only send some apps on, no screen-reader confirmation",
            given: || {
                let mut r = Rig::new();
                r.w.screen_reader = true;
                r.w.settings.only_some_apps = true;
                r
            },
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, _| assert!(r.has_job(Purpose::Switch)),
        },
    ]);
}

#[test]
fn a_switch_can_ask_before_it_takes_a_busy_speaker() {
    // The other value of the fixed choice: a question after the click.
    fn asking() -> Rig {
        let mut r = Rig::new();
        r.core.ask_before_taking(true);
        r
    }
    run(&[
        Row {
            name: "a click on a busy speaker's row asks in place and changes nothing",
            given: || {
                let mut r = asking();
                r.speaker(K, SpeakerSignal::PlayingElse { who: Some("LAPTOP-7Q2K".into()) });
                r
            },
            when: |r| {
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, dos| {
                assert!(!connects(dos, Purpose::Switch));
                let s = r.sit();
                assert_eq!(
                    s.confirm_in_panel,
                    Some(Confirm::Busy { speaker: id(K), way: WayChoice::Native, who: Some("LAPTOP-7Q2K".into()) })
                );
                assert!(s.popups.confirmation.is_none());
                assert_eq!(row(r, K), RowState::PlayingElse);
            },
        },
        Row {
            name: "confirmed, it switches without looking again",
            given: || {
                let mut r = asking();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| {
                r.cmd(Command::Confirm { yes: true }).unwrap();
            },
            then: |r, _| {
                let j = r.job(Purpose::Switch);
                assert!(!j.look_first, "the person said it may stop what plays there");
                assert!(r.sit().confirm_in_panel.is_none());
            },
        },
        Row {
            name: "Cancel changes nothing",
            given: || {
                let mut r = asking();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| {
                r.cmd(Command::Confirm { yes: false }).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Connect(_) | Do::Windows(_))));
                assert!(r.sit().confirm_in_panel.is_none());
            },
        },
        Row {
            name: "from the tray the confirmation comes by the clock",
            given: || {
                let mut r = asking();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| {
                r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, _| {
                let s = r.sit();
                assert!(matches!(s.popups.confirmation.map(|c| c.what), Some(Confirm::Busy { .. })));
                assert!(s.confirm_in_panel.is_none());
            },
        },
        Row {
            name: "a look that finds it busy asks too",
            given: || {
                let mut r = asking();
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                assert!(r.job(Purpose::Switch).look_first, "set-up: it looks first");
                r
            },
            when: |r| r.end(Purpose::Switch, Ended::Busy { who: None }),
            then: |r, dos| {
                assert!(!to_ours(dos));
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::Busy { .. })));
            },
        },
        Row {
            name: "from the API a switch that needs a confirmation changes nothing",
            given: || {
                let mut r = asking();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| {
                let result = r.cmd_from(Surface::Api, Command::Switch { place: Place::Speaker(id(K)), way: None });
                assert_eq!(result, Err(Refusal::OnlyFromWindow { speaker: id(K) }));
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Connect(_))));
                assert!(r.sit().confirm_in_panel.is_none() && r.sit().popups.confirmation.is_none());
            },
        },
        Row {
            name: "a speaker that may be in use asks first",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.core.ask_before_taking(true);
                r.switch_to(OFFICE);
                r.speaker(OFFICE, SpeakerSignal::EndedStream { reachable: true });
                r.open_panel();
                r
            },
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
            },
            then: |r, _| {
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::MayBeInUse { .. })));
            },
        },
        Row {
            name: "while a screen reader runs a switch asks once per speaker, busy first",
            given: || {
                let mut r = asking();
                r.w.screen_reader = true;
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| {
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::Busy { .. })), "busy first");
                r.cmd(Command::Confirm { yes: true }).unwrap();
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::ScreenReader { .. })), "then the screen reader");
                r.cmd(Command::Confirm { yes: true }).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r.speaker(K, SpeakerSignal::Free);
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
            },
            then: |r, _| {
                assert!(r.sit().confirm_in_panel.is_none(), "answered once for Kitchen");
                assert!(r.has_job(Purpose::Switch));
            },
        },
    ]);
}

#[test]
fn ways_are_chosen_per_speaker_and_switching_way_is_a_switch() {
    run(&[
        Row {
            name: "a click uses the way with the shortest delay at first",
            given: Rig::new,
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
            },
            then: |r, _| {
                assert_eq!(r.job(Purpose::Switch).way, WayChoice::AirPlaySteady);
                assert_eq!(r.sit().speaker(&id(OFFICE)).unwrap().way, WayChoice::AirPlaySteady);
            },
        },
        Row {
            name: "a way chosen for a speaker that isn't the place is its next switch's",
            given: Rig::new,
            when: |r| {
                r.cmd(Command::ChooseWay { speaker: id(OFFICE), way: WayChoice::AirPlayShort }).unwrap();
            },
            then: |r, dos| {
                assert!(!connects(dos, Purpose::Way) && !connects(dos, Purpose::Switch), "it switches nothing");
                assert_eq!(r.sit().speaker(&id(OFFICE)).unwrap().way, WayChoice::AirPlayShort);
            },
        },
        Row {
            name: "a way chosen for the speaker a switch is on its way to: the switch goes on by that way, which is kept",
            given: || {
                let mut r = Rig::new();
                r.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
                assert_eq!(r.job(Purpose::Switch).way, WayChoice::AirPlaySteady);
                r
            },
            when: |r| {
                r.cmd(Command::ChooseWay { speaker: id(OFFICE), way: WayChoice::AirPlayShort }).unwrap();
            },
            then: |r, dos| {
                assert!(connects(dos, Purpose::Switch), "the switch starts again by the new way");
                assert_eq!(r.job(Purpose::Switch).way, WayChoice::AirPlayShort);
                r.end(Purpose::Switch, Ended::Ready);
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(id(OFFICE)));
                assert_eq!(s.speaker(&id(OFFICE)).unwrap().way, WayChoice::AirPlayShort);
            },
        },
        Row {
            name: "a way chosen while a question about a switch to that speaker stands: a yes switches by that way",
            given: || {
                let mut r = Rig::new();
                r.w.screen_reader = true;
                r.open_panel();
                r.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::ScreenReader { way: WayChoice::AirPlaySteady, .. })));
                r
            },
            when: |r| {
                r.cmd(Command::ChooseWay { speaker: id(OFFICE), way: WayChoice::AirPlayShort }).unwrap();
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::ScreenReader { way: WayChoice::AirPlayShort, .. })));
                r.cmd(Command::Confirm { yes: true }).unwrap();
            },
            then: |r, _| {
                assert_eq!(r.job(Purpose::Switch).way, WayChoice::AirPlayShort);
                r.end(Purpose::Switch, Ended::Ready);
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(id(OFFICE)));
                assert_eq!(s.speaker(&id(OFFICE)).unwrap().way, WayChoice::AirPlayShort);
            },
        },
        Row {
            name: "another way of the place is a switch: the sound stays on the old way until ready",
            given: || {
                let mut r = Rig::new();
                r.switch_to(OFFICE);
                r
            },
            when: |r| {
                r.cmd(Command::ChooseWay { speaker: id(OFFICE), way: WayChoice::AirPlayShort }).unwrap();
            },
            then: |r, dos| {
                assert!(connects(dos, Purpose::Way));
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(id(OFFICE)));
                assert_eq!(s.picking.map(|p| (p.by, p.way)), Some((SwitchBy::Way, WayChoice::AirPlayShort)));
            },
        },
    ]);
}

#[test]
fn try_again_is_a_switch_tried_once() {
    /// Kitchen stopped responding during play and the app moved the sound.
    fn moved() -> Rig {
        let mut r = Rig::playing_on_kitchen();
        r.speaker(K, SpeakerSignal::NotResponding);
        r.wait(10_250);
        assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))), "set-up: the sound is on this PC");
        r
    }
    fn trying() -> Rig {
        let mut r = moved();
        r.cmd(Command::TryAgain(id(K))).unwrap();
        r
    }
    run(&[
        Row {
            name: "Try Kitchen again connects once, and the sound plays on where it is",
            given: moved,
            when: |r| {
                r.cmd(Command::TryAgain(id(K))).unwrap();
                r.wait(500);
            },
            then: |r, dos| {
                assert!(connects(dos, Purpose::TryAgain));
                assert!(!to_ours(dos) && asks_no_player(dos));
                let s = r.sit();
                assert_eq!(s.picking.map(|p| p.by), Some(SwitchBy::TryAgain));
                assert_eq!((s.spot, s.doing), (Spot::ThisPc(Some(out("speakers"))), Doing::Playing));
                assert_eq!(row(r, K), RowState::Connecting);
                assert!(note(r).is_some(), "the note stands until a switch takes effect");
            },
        },
        Row {
            name: "Cancel stops the try and changes nothing else",
            given: trying,
            when: |r| {
                r.cmd(Command::CancelSwitch).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().any(|d| matches!(d, Do::Supersede(_))));
                assert!(r.sit().picking.is_none());
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))), "the note and its button stay");
                assert_eq!(row(r, K), RowState::NotResponding, "its row says again why the sound left it");
            },
        },
        Row {
            name: "This PC's row does the same, being the place",
            given: trying,
            when: |r| {
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().any(|d| matches!(d, Do::Supersede(_))));
                assert!(r.sit().picking.is_none());
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))), "the note and its button stay");
            },
        },
        Row {
            name: "a try that fails changes nothing: the note answers it, keeps the button, and the row says why",
            given: || {
                let mut r = trying();
                r.open_panel();
                r
            },
            when: |r| r.end(Purpose::TryAgain, Ended::NotResponding),
            then: |r, dos| {
                assert!(!to_ours(dos));
                let s = r.sit();
                assert_eq!(s.pick_failed.map(|p| (p.by, p.why)), Some((SwitchBy::TryAgain, PickWhy::NotResponding)));
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))));
                assert_eq!(cause(r), Some(Cause::DidntGetReady { speaker: id(K), why: PickWhy::NotResponding }));
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))));
                assert_eq!(row(r, K), RowState::NotResponding);
                // The note about the move is back once the panel has closed.
                r.close_panel();
                assert!(matches!(cause(r), Some(Cause::StoppedResponding { .. })));
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))));
            },
        },
        Row {
            name: "when it takes effect the sound moves and the note goes",
            given: trying,
            when: |r| {
                r.end(Purpose::TryAgain, Ended::Ready);
                r.wait(500);
            },
            then: |r, dos| {
                assert!(to_ours(dos) && asks_no_player(dos));
                let s = r.sit();
                assert_eq!((s.spot.clone(), s.doing), (Spot::Speaker(id(K)), Doing::Playing));
                assert_eq!((s.note.clone(), popup(r), s.amber), (None, None, false));
                assert_eq!(row(r, K), RowState::Plain);
            },
        },
        Row {
            name: "from the pop-up's button a try that fails says so by the clock",
            given: moved,
            when: |r| {
                r.cmd_from(Surface::Notification, Command::TryAgain(id(K))).unwrap();
                r.end(Purpose::TryAgain, Ended::NotResponding);
            },
            then: |r, _| {
                assert_eq!(popup(r), Some(Popup::DidntGetReady { speaker: id(K), why: PickWhy::NotResponding }));
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))), "the panel's note offers the same again");
            },
        },
    ]);
}

#[test]
fn first_run_rows_only_play_a_chime() {
    fn first_run() -> Rig {
        Rig::with(Kept::default(), world())
    }
    run(&[
        Row {
            name: "step 1: a click plays a chime on that speaker only",
            given: first_run,
            when: |r| {
                assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
                r.cmd(Command::FirstRunAction(FirstRunAction::Chime(id(K)))).unwrap();
            },
            then: |r, dos| {
                assert!(connects(dos, Purpose::Chime));
                assert!(!connects(dos, Purpose::Switch) && !to_ours(dos));
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))), "the sound stays where it is");
                assert_eq!(s.first_run, Some(FirstRunStep::One), "wait for the speaker to play the chime");
            },
        },
        Row {
            name: "on a busy speaker the row asks first",
            given: || {
                let mut r = first_run();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| {
                r.cmd(Command::FirstRunAction(FirstRunAction::Chime(id(K)))).unwrap();
            },
            then: |r, dos| {
                assert!(!connects(dos, Purpose::Chime));
                assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::ChimeBusy { .. })));
                assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
            },
        },
        Row {
            name: "I heard it makes Kitchen the place and goes on to step 3",
            given: || {
                let mut r = first_run();
                r.cmd(Command::FirstRunAction(FirstRunAction::Chime(id(K)))).unwrap();
                r.end(Purpose::Chime, Ended::Ready);
                r
            },
            when: |r| {
                r.cmd(Command::FirstRunAction(FirstRunAction::Heard)).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
            },
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(id(K)));
                assert_eq!(s.first_run, Some(FirstRunStep::Three { speaker: id(K) }));
                assert!(r.core.kept().first_run_done, "once step 3 has shown, first run is over");
            },
        },
        Row {
            name: "hiding the panel leaves step 2 in the window",
            given: || {
                let mut r = first_run();
                r.open_panel();
                r.cmd(Command::FirstRunAction(FirstRunAction::Chime(id(K)))).unwrap();
                r.end(Purpose::Chime, Ended::Ready);
                r
            },
            when: |r| r.close_panel(),
            then: |r, _| {
                assert_eq!(r.sit().first_run, Some(FirstRunStep::Two { speaker: id(K), level: Some(25), limit: 100 }));
                assert!(!r.core.kept().first_run_done);
            },
        },
        Row {
            name: "the × ends first run",
            given: first_run,
            when: |r| {
                r.cmd(Command::FirstRunAction(FirstRunAction::Close)).unwrap();
            },
            then: |r, _| {
                assert_eq!(r.sit().first_run, None);
                assert!(r.core.kept().first_run_done);
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
            },
        },
        Row {
            name: "first run shows again at each start until it is over",
            given: || {
                let r = first_run();
                Rig::with(r.core.kept(), world())
            },
            when: |_| {},
            then: |r, _| assert_eq!(r.sit().first_run, Some(FirstRunStep::One)),
        },
        Row {
            name: "the sign-in switch acts when it is moved",
            given: first_run,
            when: |r| {
                r.cmd(Command::FirstRunAction(FirstRunAction::StartAtSignIn(false))).unwrap();
            },
            then: |_, dos| assert!(dos.contains(&Do::StartAtSignIn(false))),
        },
    ]);
}

#[test]
fn a_chime_plays_on_one_speaker_and_leaves_the_place() {
    let mut r = Rig::playing_on_kitchen();
    r.cmd(Command::PlayChime { speaker: id(B), confirmed: false }).unwrap();
    let dos = r.take();
    assert!(connects(&dos, Purpose::Chime));
    assert_eq!(r.job(Purpose::Chime).speaker, id(B));
    r.end(Purpose::Chime, Ended::Ready);
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)), "the sound stays where it is");
    assert_eq!(r.sit().doing, Doing::Playing);
}

#[test]
fn welcome_checks_only_the_chime_then_switches_or_closes() {
    use FirstRunAction as A;
    let start = || Rig::with(Kept::default(), world());
    let chime = |r: &mut Rig| {
        r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
        r.end(Purpose::Chime, Ended::Ready);
    };
    for finish in [A::Heard, A::UseWithoutChecking] {
        let mut r = start();
        chime(&mut r);
        r.cmd(Command::FirstRunAction(A::ChimeVolume(41))).unwrap();
        r.cmd(Command::FirstRunAction(A::ChimeAgain)).unwrap();
        assert_eq!(r.job(Purpose::Chime).welcome_level, Some(41));
        r.end(Purpose::Chime, Ended::Ready);
        r.cmd(Command::FirstRunAction(A::DidntHear)).unwrap();
        r.open_panel();
        r.close_panel();
        assert_eq!(r.sit().first_run, Some(FirstRunStep::Two { speaker: id(K), level: Some(41), limit: 100 }));
        assert!(r.sit().place_speaker().is_none());
        assert_eq!(r.restart(world(), StartKind::Normal).sit().first_run, Some(FirstRunStep::One));
        r.cmd(Command::FirstRunAction(A::ChooseAnother)).unwrap();
        assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
        chime(&mut r);
        r.cmd(Command::FirstRunAction(A::ChimeVolume(37))).unwrap();
        r.cmd(Command::FirstRunAction(finish)).unwrap();
        // The level set for the chime is Kitchen's level from this PC.
        assert_eq!(r.job(Purpose::Switch).welcome_level, Some(37));
        r.end(Purpose::Switch, Ended::Ready);
        assert_eq!(r.core.kept().levels.get(&id(K)).map(|l| l.level), Some(37));
        assert_eq!(r.sit().place_speaker(), Some(&id(K)));
        assert_eq!(r.sit().first_run, Some(FirstRunStep::Three { speaker: id(K) }));
        assert_eq!(r.restart(world(), StartKind::Normal).sit().first_run, None);
        r.cmd(Command::FirstRunAction(A::Done)).unwrap();
        assert_eq!(r.sit().first_run, None);
    }
    for two in [false, true] {
        let mut r = start();
        if two { chime(&mut r); }
        for command in crate::shell::main_window::close_commands(true, true) { r.cmd(command).unwrap(); }
        assert_eq!(r.sit().first_run, None);
        assert_eq!(popup(&r), Some(Popup::WindowClosed));
        assert_eq!(r.restart(world(), StartKind::Normal).sit().first_run, None);
    }
}

#[test]
fn welcome_chime_errors_and_busy_answers_belong_to_the_row() {
    use FirstRunAction as A;
    for (error, state) in [(Ended::NotResponding, RowState::NotResponding), (Ended::NotFound, RowState::NotFound)] {
        let mut r = Rig::with(Kept::default(), world());
        r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
        r.end(Purpose::Chime, Ended::Busy { who: None });
        r.cmd(Command::Confirm { yes: false }).unwrap();
        assert!(!r.has_job(Purpose::Chime));
        r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
        r.cmd(Command::Confirm { yes: true }).unwrap();
        assert!(!r.job(Purpose::Chime).look_first);
        assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
        r.end(Purpose::Chime, error);
        assert_eq!(row(&r, K), state);
        assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
        assert!(r.sit().place_speaker().is_none());
    }
}

#[test]
fn the_welcome_switch_sets_the_slider_level_even_on_the_current_speaker() {
    use FirstRunAction as A;
    for current in [false, true] {
        for finish in [Command::FirstRunAction(A::Heard), Command::FirstRunAction(A::UseWithoutChecking),
            Command::Switch { place: Place::Speaker(id(K)), way: None }] {
            let mut r = Rig::with(Kept::default(), world());
            if current { r.switch_to(K); }
            r.w.speakers[0].level = Some(64);
            r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
            r.end(Purpose::Chime, Ended::Ready);
            r.cmd(Command::FirstRunAction(A::ChimeVolume(42))).unwrap();
            r.w.speakers[0].max_level = Some(39);
            r.take();
            r.cmd(finish).unwrap();
            if !current { r.end(Purpose::Switch, Ended::Ready); }
            assert_eq!(r.sit().first_run, Some(FirstRunStep::Three { speaker: id(K) }));
            assert_eq!(r.core.kept().levels[&id(K)].level, 39);
            assert!(r.take().contains(&Do::SetLevel { speaker: id(K), level: 39 }));
        }
    }
}

#[test]
fn a_speaker_without_a_chime_can_be_chosen_in_first_run() {
    use FirstRunAction as A;
    let mut r = Rig::with(Kept::default(), world());
    r.w.speakers[0].can_chime = false;
    r.cmd(Command::FirstRunAction(A::Chime(id(B)))).unwrap();
    let old = r.job(Purpose::Chime);
    r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
    r.event(Event::Connected { gen: old.gen, ended: Ended::Ready });
    assert_eq!(r.sit().first_run, Some(FirstRunStep::Two { speaker: id(K), level: None, limit: 100 }));
    r.cmd(Command::FirstRunAction(A::ChimeAgain)).unwrap();
    assert!(!r.has_job(Purpose::Chime));
    r.cmd(Command::FirstRunAction(A::ChooseAnother)).unwrap();
    assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
    r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
    // The page has no slider without a chime: a level sent anyway isn't used.
    r.cmd(Command::FirstRunAction(A::ChimeVolume(31))).unwrap();
    r.cmd(Command::FirstRunAction(A::UseWithoutChecking)).unwrap();
    assert_eq!(r.job(Purpose::Switch).welcome_level, None);
    r.end(Purpose::Switch, Ended::Ready);
    assert!(!r.take().contains(&Do::SetLevel { speaker: id(K), level: 31 }));
    assert_eq!(r.sit().first_run, Some(FirstRunStep::Three { speaker: id(K) }));
}

#[test]
fn an_ordinary_chime_failure_belongs_to_its_surface() {
    let mut r = Rig::with(Kept::default(), world());
    r.cmd_from(Surface::Settings, Command::PlayTestSound(id(K))).unwrap();
    r.end(Purpose::Chime, Ended::Refused);
    let failure = r.sit().pick_failed.unwrap();
    assert_eq!((failure.by, failure.from), (SwitchBy::Switch, SwitchFrom::Window));
    assert!(!r.core.first_run_failure(&failure));
}

#[test]
fn first_run_keeps_the_chime_failure_until_another_choice_or_close() {
    use FirstRunAction as A;
    for (end, why) in [
        (Ended::NoNetwork, PickWhy::NoNetwork), (Ended::Refused, PickWhy::Refused),
        (Ended::CouldntStart, PickWhy::CouldntStart), (Ended::WrongSecret(Secret::Code), PickWhy::WrongSecret(Secret::Code)),
        (Ended::NeedsSecret(Secret::Code), PickWhy::NeedsSecret(Secret::Code)),
    ] {
        let mut r = Rig::with(Kept::default(), world());
        r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
        r.end(Purpose::Chime, end.clone());
        assert_eq!(r.sit().first_run, Some(FirstRunStep::One));
        assert_eq!(r.sit().pick_failed.map(|p| (p.speaker, p.why)), Some((id(K), why)));
        assert_eq!(r.core.recs[&id(K)].last_try, Some(why));
        r.cmd(Command::FirstRunAction(A::Chime(id(B)))).unwrap();
        assert!(r.sit().pick_failed.is_none());
        r.end(Purpose::Chime, end);
        r.cmd(Command::FirstRunAction(A::Close)).unwrap();
        r.cmd(Command::FirstRunAction(A::ShowAgain)).unwrap();
        assert!(r.sit().pick_failed.is_none());
    }
}

#[test]
fn the_welcome_switch_takes_the_level_when_confirmation_is_answered() {
    use FirstRunAction as A;
    let mut r = Rig::with(Kept::default(), world());
    r.w.screen_reader = true;
    r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
    r.end(Purpose::Chime, Ended::Ready);
    r.cmd(Command::FirstRunAction(A::ChimeVolume(40))).unwrap();
    r.cmd(Command::FirstRunAction(A::Heard)).unwrap();
    assert!(matches!(r.sit().confirm_in_panel, Some(Confirm::ScreenReader { .. })));
    r.cmd(Command::FirstRunAction(A::ChimeVolume(10))).unwrap();
    r.cmd(Command::Confirm { yes: true }).unwrap();
    assert_eq!(r.job(Purpose::Switch).welcome_level, Some(10));
    assert!(!r.core.kept().levels.contains_key(&id(K)));
    r.cmd(Command::FirstRunAction(A::ChimeVolume(30))).unwrap();
    r.end(Purpose::Switch, Ended::Ready);
    assert_eq!(r.core.kept().levels[&id(K)].level, 10);
    assert!(r.take().contains(&Do::SetLevel { speaker: id(K), level: 10 }));
}

#[test]
fn another_switch_to_the_welcome_speaker_finishes_step_two() {
    use FirstRunAction as A;
    let mut r = Rig::with(Kept::default(), world());
    r.w.speakers[0].can_chime = false;
    r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
    r.cmd(Command::FirstRunAction(A::UseWithoutChecking)).unwrap();
    r.end(Purpose::Switch, Ended::NeedsSecret(Secret::Code));
    r.open_panel();
    r.close_panel();
    assert_eq!(r.sit().pick_failed.unwrap().why, PickWhy::NeedsSecret(Secret::Code));
    r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
    r.end(Purpose::Switch, Ended::Refused);
    r.open_panel();
    r.close_panel();
    assert_eq!(r.sit().pick_failed.unwrap().why, PickWhy::Refused);
    r.switch_to(B);
    assert!(matches!(r.sit().first_run, Some(FirstRunStep::Two { .. })));
    r.switch_to(K);
    assert_eq!(r.sit().first_run, Some(FirstRunStep::Three { speaker: id(K) }));
    assert!(r.core.kept().first_run_done);

    r.cmd(Command::FirstRunAction(A::ShowAgain)).unwrap();
    r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
    r.take();
    r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
    assert_eq!(r.sit().first_run, Some(FirstRunStep::Three { speaker: id(K) }));
    assert!(!r.take().iter().any(|action| matches!(action, Do::SetLevel { .. } | Do::Connect(_))));
}

#[test]
fn the_tray_switch_carries_the_welcome_level_and_heard_can_update_it() {
    use FirstRunAction as A;
    for heard in [false, true] {
        let mut r = Rig::with(Kept::default(), world());
        r.cmd(Command::FirstRunAction(A::Chime(id(K)))).unwrap();
        r.end(Purpose::Chime, Ended::Ready);
        r.cmd(Command::FirstRunAction(A::ChimeVolume(10))).unwrap();
        r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
        let job = r.job(Purpose::Switch);
        assert_eq!(job.welcome_level, Some(10));
        let expected = if heard {
            r.cmd(Command::FirstRunAction(A::ChimeVolume(14))).unwrap();
            r.cmd(Command::FirstRunAction(A::Heard)).unwrap();
            assert_eq!(r.job(Purpose::Switch).gen, job.gen);
            14
        } else { 10 };
        r.event(Event::Connected { gen: job.gen, ended: Ended::Ready });
        assert!(r.take().contains(&Do::SetLevel { speaker: id(K), level: expected }));
        assert_eq!(r.sit().first_run, Some(FirstRunStep::Three { speaker: id(K) }));
    }
}
