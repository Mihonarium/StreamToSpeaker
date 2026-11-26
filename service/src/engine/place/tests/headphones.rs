//! Headphones are This PC; Windows' list of outputs, and the two-second
//! window of Windows' own changes.

use super::rig::*;

fn on_headphones() -> Spot {
    Spot::ThisPc(Some(out("headphones")))
}

fn this_pc() -> Spot {
    Spot::ThisPc(Some(out("speakers")))
}

/// Kitchen played, headphones took the sound, and they are still connected.
fn with_headphones() -> Rig {
    let mut r = Rig::playing_on_kitchen();
    r.plug_headphones(true);
    r.wait(4_000);
    r
}

/// … and they went: the sound is on this PC's speakers, with the note.
fn headphones_went() -> Rig {
    let mut r = with_headphones();
    r.unplug_headphones();
    r.wait(500);
    assert!(matches!(cause(&r), Some(Cause::HeadphonesGone { .. })), "set-up: the note");
    r
}

/// The world with the headphones connected, and Windows on them or not.
fn world_with_headphones(default: bool) -> World {
    let mut w = world();
    w.outputs.push(OutputWorld { id: out("headphones"), kind: OutputKind::Headphones, name: "Headphones".into() });
    if default {
        w.default_output = Some(OutputRef::Pc(out("headphones")));
    }
    w
}

#[test]
fn headphones_are_this_pc() {
    run(&[
        Row {
            name: "connected while Kitchen plays: This PC is the place, and the note offers the way back to Kitchen",
            given: Rig::playing_on_kitchen,
            when: |r| r.plug_headphones(true),
            then: |r, dos| {
                assert!(silenced(dos, K) && asks_no_player(dos));
                let s = r.sit();
                assert_eq!((s.spot.clone(), popup(r)), (on_headphones(), None));
                assert_eq!(cause(r), Some(Cause::HeadphonesConnected { before: id(K), name: "Headphones".into() }));
                assert_eq!(button(r), Some(NoteButton::SwitchTo(id(K))));
                let kept = r.core.kept();
                assert_eq!(kept.place, None);
                assert_eq!(
                    kept.headphones,
                    Some(KeptHeadphones { output: out("headphones"), before: id(K), name: "Headphones".into() })
                );
            },
        },
        Row {
            name: "the × of the note about connected headphones closes it for as long as they stay",
            given: with_headphones,
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
                r.wait(60_000);
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Connect(_))));
                assert_eq!((r.sit().spot, r.sit().note), (on_headphones(), None));
                assert!(r.core.kept().headphones.is_some(), "the way back when they go is kept");
                // Also across a restart of the app.
                let again = r.restart(world_with_headphones(true), StartKind::Normal);
                assert_eq!((again.sit().spot, again.sit().note), (on_headphones(), None));
            },
        },
        Row {
            name: "… and when they go, the note says so all the same",
            given: || {
                let mut r = with_headphones();
                r.cmd(Command::CloseNote).unwrap();
                r
            },
            when: |r| r.unplug_headphones(),
            then: |r, _| {
                assert!(matches!(cause(r), Some(Cause::HeadphonesGone { .. })));
                assert_eq!(r.core.kept().closed_note, None);
            },
        },
        Row {
            name: "the note's button is the way back, a switch like any other",
            given: headphones_went,
            when: |r| {
                assert_eq!(button(r), Some(NoteButton::SwitchTo(id(K))));
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
            },
            then: |r, dos| {
                assert!(to_ours(dos));
                assert_eq!((r.sit().spot, r.sit().note), (Spot::Speaker(id(K)), None));
            },
        },
        Row {
            name: "the note's × closes it, and the sound stays where it is",
            given: headphones_went,
            when: |r| {
                r.cmd(Command::CloseNote).unwrap();
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Connect(_))));
                assert_eq!((r.sit().spot, r.sit().note, popup(r)), (this_pc(), None, None));
            },
        },
        Row {
            name: "with the notes switch off nothing shows by the clock; the panel's note stays",
            given: || {
                let mut r = with_headphones();
                r.w.settings.notes = false;
                r
            },
            when: |r| r.unplug_headphones(),
            then: |r, _| {
                assert_eq!(popup(r), None);
                assert!(matches!(cause(r), Some(Cause::HeadphonesGone { .. })));
            },
        },
        Row {
            name: "a switch made while they are connected forgets the way back",
            given: || {
                let mut r = with_headphones();
                r.cmd(Command::SwitchOutput(out("speakers"))).unwrap();
                r.wait(500);
                r
            },
            when: |r| r.unplug_headphones(),
            then: |r, _| {
                assert_eq!((r.sit().spot, r.sit().note, popup(r)), (this_pc(), None, None));
                assert_eq!(r.core.kept().headphones, None);
            },
        },
        Row {
            name: "headphones that went while the app wasn't running: the note, without a time",
            given: || with_headphones().restart(world(), StartKind::Normal),
            when: |_| {},
            then: |r, _| {
                let want = Note {
                    cause: Cause::HeadphonesGone { before: id(K), name: "Headphones".into() },
                    at: None,
                    available_again: false,
                    ahead: None,
                };
                assert_eq!((r.sit().spot, r.sit().note, popup(r)), (this_pc(), Some(want), None));
            },
        },
        Row {
            name: "headphones still connected at a restart keep the way back",
            given: || with_headphones().restart(world_with_headphones(true), StartKind::Normal),
            when: |r| {
                assert_eq!(r.sit().spot, on_headphones());
                assert!(matches!(cause(r), Some(Cause::HeadphonesConnected { .. })));
                r.unplug_headphones();
            },
            then: |r, _| assert_eq!(button(r), Some(NoteButton::SwitchTo(id(K)))),
        },
        Row {
            name: "another output chosen while the app wasn't running forgets it",
            given: || with_headphones().restart(world_with_headphones(false), StartKind::Normal),
            when: |r| r.unplug_headphones(),
            then: |r, _| assert_eq!(r.sit().note, None),
        },
        Row {
            name: "connected while a note stands: the note stays, as after a click on This PC's row",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(10_250);
                r
            },
            when: |r| r.plug_headphones(true),
            then: |r, _| {
                assert_eq!(r.sit().spot, on_headphones());
                assert!(matches!(cause(r), Some(Cause::StoppedResponding { .. })));
                assert_eq!(r.core.kept().headphones, None, "no speaker was the place before them");
            },
        },
        Row {
            name: "headphones cancel a switch connecting",
            given: || {
                let mut r = Rig::new();
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r
            },
            when: |r| r.plug_headphones(true),
            then: |r, dos| {
                assert!(dos.iter().any(|d| matches!(d, Do::Supersede(_))));
                let s = r.sit();
                assert!(s.picking.is_none());
                assert_eq!(s.spot, on_headphones());
            },
        },
        Row {
            name: "where Windows doesn't switch to headphones, the app does",
            given: Rig::on_kitchen,
            when: |r| {
                r.plug_headphones(false);
                r.wait(2_500);
            },
            then: |r, dos| {
                assert!(windows_to(dos, &WinTarget::Output(out("headphones"))));
                assert_eq!(r.sit().spot, on_headphones());
            },
        },
        Row {
            name: "another new output changes nothing unless Windows switches to it",
            given: Rig::on_kitchen,
            when: |r| {
                r.w.outputs.push(OutputWorld { id: out("tv"), kind: OutputKind::Display, name: "LG TV".into() });
                let at = r.now.mono_ms;
                r.event(Event::Output(OutputEvent::Added { id: out("tv"), kind: OutputKind::Display, name: "LG TV".into(), at_ms: at }));
                r.wait(3_000);
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_))));
                assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
            },
        },
    ]);
}

#[test]
fn headphones_that_go_can_send_the_sound_back_to_the_speaker() {
    // The other value of the fixed choice.
    let mut r = Rig::playing_on_kitchen();
    r.core.choose(AfterHeadphones::BackToSpeaker, AfterFailedStart::ThisPc);
    r.plug_headphones(true);
    r.wait(4_000);
    assert_eq!(r.sit().spot, on_headphones());
    r.take();
    r.unplug_headphones();
    let dos = r.take();
    assert!(to_ours(&dos), "Windows goes back to the app's device: {dos:?}");
    assert!(connects(&dos, Purpose::SoundStarted), "what plays connects the speaker: {dos:?}");
    assert!(asks_no_player(&dos));
    assert_eq!((r.sit().spot, r.sit().note), (Spot::Speaker(id(K)), None));
    r.end(Purpose::SoundStarted, Ended::Ready);
    r.wait(500);
    assert_eq!(r.sit().doing, Doing::Playing);

    // While nothing plays the speaker is the place again and stays at rest.
    let mut r = Rig::on_kitchen();
    r.core.choose(AfterHeadphones::BackToSpeaker, AfterFailedStart::ThisPc);
    r.plug_headphones(true);
    r.wait(4_000);
    r.take();
    r.unplug_headphones();
    let dos = r.take();
    assert!(to_ours(&dos) && !dos.iter().any(|d| matches!(d, Do::Connect(_))), "{dos:?}");
    assert_eq!((r.sit().spot, r.sit().note, popup(&r)), (Spot::Speaker(id(K)), None, None));
}

#[test]
fn a_windows_change_within_two_seconds_is_windows_doing() {
    run(&[
        Row {
            name: "a change of output with nothing coming or going is a person's: This PC",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.w.outputs.push(OutputWorld { id: out("usb"), kind: OutputKind::Other, name: "USB Audio".into() });
                r.choose_in_windows(OutputRef::Pc(out("usb")));
            },
            then: |r, dos| {
                assert!(silenced(dos, K));
                assert!(asks_no_player(dos), "a switch pauses nothing");
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("usb"))));
            },
        },
        Row {
            name: "another output chosen in Windows' list while a note stands is a switch: the note goes",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(10_250);
                assert!(note(&r).is_some(), "set-up: the note");
                r
            },
            when: |r| {
                r.w.outputs.push(OutputWorld { id: out("usb"), kind: OutputKind::Other, name: "USB Audio".into() });
                r.choose_in_windows(OutputRef::Pc(out("usb")));
            },
            then: |r, _| assert_eq!((r.sit().spot, r.sit().note), (Spot::ThisPc(Some(out("usb"))), None)),
        },
        Row {
            name: "Windows landing on the app's device because an output went, with This PC the place: back to the PC's own",
            given: || {
                let mut r = Rig::new();
                r.w.outputs.push(OutputWorld { id: out("usb"), kind: OutputKind::Other, name: "USB Audio".into() });
                r.choose_in_windows(OutputRef::Pc(out("usb")));
                r
            },
            when: |r| {
                r.w.outputs.retain(|o| o.id != out("usb"));
                let at = r.now.mono_ms;
                r.event(Event::Output(OutputEvent::Removed { id: out("usb"), at_ms: at }));
                r.now = r.now.later(300);
                r.w.default_output = Some(OutputRef::Ours);
                let at = r.now.mono_ms;
                r.event(Event::Output(OutputEvent::DefaultChanged { at_ms: at }));
                r.wait(2_500);
            },
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert!(!connects(dos, Purpose::Switch), "not a switch to a speaker");
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
            },
        },
    ]);
}

#[test]
fn a_switch_in_windows_list_plays_meanwhile_on_the_old_output() {
    run(&[
        Row {
            name: "choosing the app's device switches to its speaker; what arrives plays on the old output",
            given: || {
                let mut r = Rig::on_kitchen();
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r
            },
            when: |r| r.choose_in_windows(OutputRef::Ours),
            then: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert_eq!(r.core.pass, Some(out("speakers")));
                assert_eq!(r.sit().picking.map(|p| p.from), Some(SwitchFrom::WindowsList));
            },
        },
        Row {
            name: "it doesn't get ready: Windows is put back, with a note",
            given: || {
                let mut r = Rig::on_kitchen();
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r.choose_in_windows(OutputRef::Ours);
                r
            },
            when: |r| r.end(Purpose::Switch, Ended::NotFound),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert_eq!(popup(r), Some(Popup::DidntGetReady { speaker: id(K), why: PickWhy::NotFound }));
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
            },
        },
        Row {
            name: "the device names no speaker: Windows is put back, No speaker chosen",
            given: Rig::new,
            when: |r| r.choose_in_windows(OutputRef::Ours),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert_eq!(popup(r), Some(Popup::NoSpeakerChosen));
            },
        },
        Row {
            name: "a switch that has to ask first asks by the clock, with Windows back on its output meanwhile",
            given: || {
                let mut r = Rig::on_kitchen();
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r.w.screen_reader = true;
                r
            },
            when: |r| r.choose_in_windows(OutputRef::Ours),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert!(matches!(r.sit().popups.confirmation.map(|c| c.what), Some(Confirm::ScreenReader { .. })));
            },
        },
        Row {
            name: "a speaker that plays something else is taken without a question, as from any other surface",
            given: || {
                let mut r = Rig::on_kitchen();
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| r.choose_in_windows(OutputRef::Ours),
            then: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert!(r.sit().popups.confirmation.is_none());
            },
        },
        Row {
            name: "Windows didn't take the app's device: the sound stays where it was",
            given: || {
                let mut r = Rig::new();
                r.obliging = false;
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
                r
            },
            when: |r| {
                let at = r.now.mono_ms;
                r.event(Event::Output(OutputEvent::NotTaken { at_ms: at }));
            },
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))));
                assert!(s.standing.contains(&Standing::WindowsDidntSwitch { speaker: id(K) }));
            },
        },
    ]);
}
