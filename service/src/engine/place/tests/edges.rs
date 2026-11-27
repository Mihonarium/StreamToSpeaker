//! The cases at the edges, one row each, and what stands while it lasts.

use super::rig::*;

fn on_home_then_away() -> Rig {
    let mut r = Rig::on_kitchen();
    r.w.connections = vec![Connection { net: NetId("hotspot".into()), name: Some("Phone".into()), kind: ConnKind::WiFi }];
    for s in &mut r.w.speakers {
        s.found = false;
    }
    r.event(Event::Network);
    r
}

#[test]
fn at_the_edges() {
    run(&[
        Row {
            name: "sleep, restart, sign-out, shutdown: the switch stays",
            given: Rig::on_kitchen,
            when: |r| {
                r.event(Event::Power(PowerEvent::SigningOut));
                *r = r.restart(world(), StartKind::Normal);
            },
            then: |r, _| assert_eq!(r.sit().spot, Spot::Speaker(id(K))),
        },
        Row {
            name: "for a minute after a start no speaker is called away on its own",
            given: || Rig::starting(Kept { place: Some(id(K)), first_run_done: true, ..Kept::default() }, world(), StartKind::Normal, false),
            when: |r| {
                for s in &mut r.w.speakers {
                    s.found = false;
                }
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(30_000);
            },
            then: |r, _| {
                assert_eq!(r.sit().away, None);
                assert_eq!(row(r, K), RowState::Plain);
            },
        },
        Row {
            name: "…after it, a speaker that isn't found is grey",
            given: || Rig::starting(Kept { place: Some(id(K)), first_run_done: true, ..Kept::default() }, world(), StartKind::Normal, false),
            when: |r| {
                for s in &mut r.w.speakers {
                    s.found = false;
                }
                r.wait(61_000);
            },
            then: |r, _| assert_eq!(row(r, B), RowState::NotFound),
        },
        Row {
            name: "another network: this PC after a minute, and nothing is said",
            given: on_home_then_away,
            when: |r| r.wait_long(61_000),
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))));
                assert_eq!((s.note.clone(), popup(r), s.amber), (None, None, false), "there the speaker can't be meant");
                assert!(
                    matches!(r.core.kept().note.map(|n| n.note.cause), Some(Cause::OtherNetwork { .. })),
                    "kept for when it can play again"
                );
            },
        },
        Row {
            name: "…nor when sound plays there afterwards",
            given: || {
                let mut r = on_home_then_away();
                r.wait_long(61_000);
                r
            },
            when: |r| {
                r.play(SPOTIFY);
                r.wait(4_000);
            },
            then: |r, _| assert_eq!((note(r), popup(r)), (None, None)),
        },
        Row {
            name: "…back on it, the note says that Kitchen is available again, and nothing shows by the clock",
            given: || {
                let mut r = on_home_then_away();
                r.wait_long(61_000);
                r
            },
            when: |r| {
                r.w = world();
                r.event(Event::Network);
            },
            then: |r, dos| {
                assert!(!to_ours(dos), "the app never moves the sound back by itself");
                let want = Note { cause: Cause::OtherNetwork { speaker: id(K) }, at: None, available_again: true, ahead: None };
                assert_eq!(note(r), Some(want));
                assert_eq!((button(r), popup(r)), (Some(NoteButton::SwitchTo(id(K))), None));
            },
        },
        Row {
            name: "another network found after the tries moved the sound: the note takes the truer cause and is no longer said",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.w.connections = vec![Connection { net: NetId("hotspot".into()), name: Some("Phone".into()), kind: ConnKind::WiFi }];
                for s in &mut r.w.speakers {
                    s.found = false;
                }
                r.event(Event::Network);
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(10_250);
                assert!(matches!(cause(&r), Some(Cause::StoppedResponding { .. })), "set-up: the tries' note");
                assert!(r.sit().amber);
                r
            },
            when: |r| r.wait(51_000),
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_))), "it isn't a new move");
                assert_eq!((note(r), popup(r), r.sit().amber), (None, None, false));
                assert!(matches!(r.core.kept().note.map(|n| n.note.cause), Some(Cause::OtherNetwork { .. })));
            },
        },
        Row {
            name: "a VPN, or the speaker found on the new connection, isn't another network",
            given: Rig::on_kitchen,
            when: |r| {
                r.w.connections.push(Connection { net: NetId("vpn".into()), name: Some("Corp VPN".into()), kind: ConnKind::Other });
                r.event(Event::Network);
                r.wait_long(2 * 60_000);
            },
            then: |r, _| assert_eq!(r.sit().spot, Spot::Speaker(id(K))),
        },
        Row {
            name: "no output of its own: nothing to play on, then the next output plays it",
            given: Rig::new,
            when: |r| {
                r.w.outputs.clear();
                r.tick();
            },
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(None));
                assert_eq!(s.doing, Doing::NothingToPlayOn);
            },
        },
        Row {
            name: "the app's sound output missing: the sound stays on this PC, rows read Repair first",
            given: Rig::on_kitchen,
            when: |r| {
                r.w.our_device = false;
                r.tick();
            },
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.spot, Spot::ThisPc(Some(out("speakers"))));
                assert!(s.standing.contains(&Standing::OutputMissing));
                assert_eq!(row(r, K), RowState::RepairFirst);
                assert_eq!(r.core.kept().place, Some(id(K)), "the place is kept for when it is back");
            },
        },
        Row {
            name: "…back: the sound goes on at the speaker",
            given: || {
                let mut r = Rig::on_kitchen();
                r.w.our_device = false;
                r.w.set_playback(OutputRef::Pc(out("speakers")));
                r.tick();
                r
            },
            when: |r| {
                r.w.our_device = true;
                r.wait(3_500);
            },
            then: |r, dos| {
                assert!(to_ours(dos));
                assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
            },
        },
        Row {
            name: "Only send some apps: Windows' output is never changed",
            given: || {
                let mut w = world();
                w.settings.only_some_apps = true;
                Rig::with(Kept { first_run_done: true, ..Kept::default() }, w)
            },
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
            },
            then: |r, dos| {
                assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_))));
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(id(K)));
                assert!(s.standing.contains(&Standing::OnlySomeApps { none_playing: true }));
            },
        },
        Row {
            name: "Only send some apps: the chosen apps play on the PC's output while this PC is the place",
            given: || {
                let mut w = world();
                w.settings.only_some_apps = true;
                Rig::with(Kept { first_run_done: true, ..Kept::default() }, w)
            },
            when: |r| {
                r.tick();
            },
            then: |r, _| assert_eq!(r.core.pass, Some(out("speakers"))),
        },
        Row {
            name: "an app plays on another output while a speaker is the place",
            given: Rig::on_kitchen,
            when: |r| {
                r.sources.push(Source { app: AppKey::new(CHROME), player: None, output: Some(OutputRef::Pc(out("speakers"))) });
                r.wait(4_000);
            },
            then: |r, _| {
                assert!(r.sit().standing.contains(&Standing::AppElsewhere { app: CHROME.into(), output: out("speakers") }));
            },
        },
        Row {
            name: "Music Assistant hasn't started playing this PC's sound",
            given: || {
                let mut w = world();
                w.speakers.push(SpeakerWorld::found(crate::model::state::MUSIC_ASSISTANT_ID));
                let mut r = Rig::with(Kept { first_run_done: true, ..Kept::default() }, w);
                r.cmd(Command::Switch { place: Place::MusicAssistant, way: None }).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
                r
            },
            when: |r| r.tick(),
            then: |r, _| {
                let s = r.sit();
                assert_eq!(s.spot, Spot::Speaker(SpeakerId::music_assistant()));
                assert!(s.standing.contains(&Standing::MusicAssistantNotStarted));
            },
        },
        Row {
            name: "while this PC is the place, what still arrives at the app's device plays on its output",
            given: Rig::on_kitchen,
            when: |r| {
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
            },
            then: |_, dos| assert!(dos.contains(&Do::PassThrough(Some(out("speakers"))))),
        },
        Row {
            name: "an AirPlay speaker starts at a level no louder than 30 after 3 hours",
            given: || {
                let mut r = Rig::new();
                r.switch_to(OFFICE);
                r.cmd(Command::SetVolume { owner: crate::model::command::VolumeOwner::Speaker(id(OFFICE)), level: 70 }).unwrap();
                r.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
                r.wait_long(4 * 3_600_000);
                r
            },
            when: |r| {
                r.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
                r.end(Purpose::Switch, Ended::Ready);
            },
            then: |_, dos| assert!(dos.contains(&Do::SetLevel { speaker: id(OFFICE), level: 30 })),
        },
    ]);
}

#[test]
fn the_option_counts_quiet_across_a_restart() {
    let mut r = Rig::on_kitchen();
    r.w.settings.switch_back_after_quiet = true;
    r.wait_long(20 * 60_000);
    let mut w = world();
    w.settings.switch_back_after_quiet = true;
    w.set_playback(OutputRef::Ours);
    let mut again = r.restart(w, StartKind::AfterUpdate);
    again.wait_long(9 * 60_000);
    assert_eq!(again.sit().spot, Spot::Speaker(id(K)));
    again.wait_long(2 * 60_000);
    assert!(matches!(cause(&again), Some(Cause::SwitchedAfterQuiet { .. })), "30 quiet minutes in all");
}

#[test]
fn a_level_set_from_this_pc_is_saved_once_it_stands() {
    let mut r = Rig::new();
    r.switch_to(OFFICE);
    r.take();
    let set = |r: &mut Rig, level| {
        r.cmd(Command::SetVolume { owner: crate::model::command::VolumeOwner::Speaker(id(OFFICE)), level }).unwrap();
    };
    // A slider on its way: nothing is written while it moves.
    set(&mut r, 40);
    r.wait(1_000);
    set(&mut r, 55);
    r.wait(1_750);
    assert!(!r.take().contains(&Do::Save), "it moved less than two seconds ago");
    r.wait(250);
    assert!(r.take().contains(&Do::Save), "it has stood for two seconds");
    assert_eq!(r.core.kept().levels.get(&id(OFFICE)).map(|l| l.level), Some(55));
    r.wait(5_000);
    assert!(!r.take().contains(&Do::Save), "it is saved once");
}

#[test]
fn changing_the_pc_default_keeps_the_chosen_apps_on_their_speaker() {
    let mut r = Rig::on_kitchen();
    r.w.settings.only_some_apps = true;
    r.tick();
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    r.take();
    r.sound("Local game");
    r.plug_headphones(true);
    r.wait(4_000);
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
    assert_eq!(r.sit().doing, Doing::NothingPlaying);
    assert!(note(&r).is_none());
    r.sources.push(Source { app: AppKey::new("Chosen game"), player: None, output: Some(OutputRef::Ours) });
    r.wait(4_000);
    r.unplug_headphones();
    r.wait(4_000);
    let s = r.sit();
    assert_eq!(s.doing, Doing::Playing);
    assert!(s.standing.contains(&Standing::OnlySomeApps { none_playing: false }));
    assert!(note(&r).is_none());
    assert!(r.take().iter().all(|action| !matches!(action, Do::Windows(_) | Do::Silence { then: Some(_), .. })));
    r.w.settings.only_some_apps = false;
    r.tick();
    assert_eq!(r.w.default_output, Some(OutputRef::Ours));
    assert!(r.take().iter().all(|a| !matches!(a, Do::Connect(_) | Do::StopSending { .. } | Do::Silence { .. })));
}

#[test]
fn chosen_apps_return_a_refused_default_without_changing_place() {
    let mut r = Rig::playing_on_kitchen();
    r.obliging = false;
    r.w.settings.only_some_apps = true;
    r.tick();
    assert!(to_speakers(&r.take()));
    r.event(Event::Output(OutputEvent::NotTaken { at_ms: r.now.mono_ms }));
    assert_eq!(r.sit().doing, Doing::Playing);
    assert!(!r.sit().standing.iter().any(|s| matches!(s, Standing::OnlySomeApps { .. })));
    r.obliging = true;
    r.wait(2_000);
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
}

#[test]
fn chosen_apps_return_the_default_at_start() {
    let mut w = world();
    w.settings.only_some_apps = true;
    w.set_playback(OutputRef::Ours);
    let r = Rig::with(Kept { place: Some(id(K)), ..Default::default() }, w);
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
}

#[test]
fn chosen_apps_return_the_default_after_headphones_go() {
    let mut w = world();
    w.settings.only_some_apps = true;
    let mut r = Rig::with(Kept { place: Some(id(K)), ..Default::default() }, w);
    r.plug_headphones(true);
    r.w.outputs.retain(|o| o.id != out("headphones"));
    r.choose_in_windows(OutputRef::Ours);
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    r.w.outputs.clear();
    r.choose_in_windows(OutputRef::Ours);
    r.plug_headphones(false);
    r.wait(12_000);
    assert_eq!(r.w.default_output, Some(OutputRef::Ours));
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
}

#[test]
fn sending_everything_again_asks_the_screen_reader_and_explains_a_refusal() {
    let mut r = Rig::on_kitchen();
    r.w.settings.only_some_apps = true;
    r.tick();
    r.sources.push(Source { app: AppKey::new(GAME), player: None, output: Some(OutputRef::Ours) });
    r.wait(4_000);
    r.take();
    r.w.screen_reader = true;
    r.w.settings.only_some_apps = false;
    r.tick();
    assert_eq!(r.sit().confirm_under_routing, Some(crate::model::situation::RoutingQuestion::TakePlayback));
    assert!(matches!(r.core.confirm.as_ref().map(|c| &c.what), Some(Confirm::ScreenReader { .. })));
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    assert_eq!(r.core.connected_speaker(), Some(&id(K)));
    assert_eq!(r.sit().doing, Doing::Playing);
    assert!(r.take().iter().all(|a| !matches!(a, Do::Connect(_) | Do::StopSending { .. } | Do::Silence { .. })));
    r.obliging = false;
    r.cmd(Command::Confirm { yes: true }).unwrap();
    let actions = r.take();
    assert!(to_ours(&actions));
    assert!(actions.iter().all(|a| !matches!(a, Do::Connect(_) | Do::StopSending { .. } | Do::Silence { .. })));
    r.event(Event::Output(OutputEvent::NotTaken { at_ms: r.now.mono_ms }));
    assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
    assert!(r.sit().standing.contains(&Standing::WindowsDidntSwitch { speaker: id(K) }));
}

#[test]
fn sending_everything_again_on_this_pc_uses_the_chosen_output() {
    let mut r = Rig::new();
    r.w.settings.only_some_apps = true;
    r.plug_headphones(false);
    r.cmd(Command::SwitchOutput(out("headphones"))).unwrap();
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    r.w.settings.only_some_apps = false;
    r.tick();
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("headphones"))));
}

#[test]
fn chosen_apps_leave_a_persons_playback_choice_in_windows() {
    let mut r = Rig::playing_on_kitchen();
    r.w.settings.only_some_apps = true;
    r.tick();
    assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
    r.take();
    r.choose_in_windows(OutputRef::Ours);
    r.wait(12_000);
    assert_eq!(r.w.default_output, Some(OutputRef::Ours));
    assert!(!to_speakers(&r.take()));
    assert_eq!(r.sit().doing, Doing::Playing);
    assert!(!r.sit().standing.iter().any(|s| matches!(s, Standing::OnlySomeApps { .. })));
}

#[test]
fn chosen_apps_stop_retrying_a_refused_playback_move_after_ten_seconds() {
    let mut r = Rig::on_kitchen();
    r.obliging = false;
    r.w.settings.only_some_apps = true;
    r.tick();
    for delay in [2_000, 3_000, 5_000] {
        assert!(to_speakers(&r.take()));
        r.event(Event::Output(OutputEvent::NotTaken { at_ms: r.now.mono_ms }));
        r.wait(delay - 1);
        assert!(!to_speakers(&r.take()));
        r.wait(1);
    }
    assert!(to_speakers(&r.take()));
    r.event(Event::Output(OutputEvent::NotTaken { at_ms: r.now.mono_ms }));
    r.wait(60_000);
    assert!(!to_speakers(&r.take()));
}

#[test]
fn chosen_apps_notice_when_the_previous_default_disappears_after_the_switch() {
    for delay in [500, WINDOWS_DOING_MS + 1] {
        let mut w = world();
        w.settings.only_some_apps = true;
        let mut r = Rig::with(Kept { place: Some(id(K)), ..Default::default() }, w);
        r.plug_headphones(true);
        r.w.set_playback(OutputRef::Ours);
        r.event(Event::Output(OutputEvent::DefaultChanged { at_ms: r.now.mono_ms }));
        r.wait(delay);
        assert_eq!(r.w.default_output, Some(OutputRef::Ours));
        r.unplug_headphones();
        let expected = if delay <= WINDOWS_DOING_MS { OutputRef::Pc(out("speakers")) } else { OutputRef::Ours };
        assert_eq!(r.w.default_output, Some(expected));
        assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
    }
}

#[test]
fn turning_off_chosen_apps_during_a_switch_asks_before_taking_playback() {
    for yes in [false, true] {
        let mut r = Rig::new();
        r.w.screen_reader = true;
        r.w.settings.only_some_apps = true;
        r.tick();
        r.cmd(Command::Switch { place: crate::model::Place::Speaker(id(K)), way: None }).unwrap();
        let gen = r.core.switch.as_ref().unwrap().gen;
        r.w.settings.only_some_apps = false;
        r.event(Event::Connected { gen, ended: Ended::Ready });
        assert_eq!(r.w.default_output, Some(OutputRef::Pc(out("speakers"))));
        assert_eq!(r.sit().confirm_under_routing, Some(crate::model::situation::RoutingQuestion::StartSwitch));
        assert!(matches!(r.core.confirm.as_ref().map(|c| &c.what), Some(Confirm::ScreenReader { speaker, .. }) if *speaker == id(K)));
        r.cmd_from(Surface::Settings, Command::Confirm { yes }).unwrap();
        if yes {
            r.end(Purpose::Switch, Ended::Ready);
            assert_eq!(r.w.default_output, Some(OutputRef::Ours));
        } else {
            assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
        }
    }
}

#[test]
fn repair_preserves_windows_outputs_and_reports_the_observed_endpoint() {
    use crate::model::scene::RepairNow;
    use crate::platform::RepairExit;
    for exit in [RepairExit::Success, RepairExit::Failed] {
        for (active, off, note) in [(true, false, RepairNow::Worked), (false, true, RepairNow::TurnedOff), (false, false, RepairNow::DidntWork)] {
            let mut r = Rig::on_kitchen();
            r.w.our_device = false;
            r.w.outputs.push(OutputWorld { id: out("headphones"), kind: OutputKind::Headphones, name: "Headphones".into() });
            r.w.set_playback(OutputRef::Pc(out("headphones")));
            r.tick();
            r.take();
            r.cmd(Command::RepairOutput).unwrap();
            let actions = r.take();
            let [Do::RepairOutput(run)] = actions.as_slice() else { panic!("{actions:?}") };
            r.event(Event::RepairApproved(*run));
            r.w.our_device = active;
            r.w.output_turned_off = off;
            r.wait(20_000);
            assert_eq!(r.core.repair_note(&r.w), RepairNow::Running);
            assert_eq!(row(&r, K), RowState::RepairFirst);
            r.event(Event::RepairExited(*run, exit));
            if exit == RepairExit::Success && !active {
                r.wait(4_999);
                assert_eq!(r.core.repair_note(&r.w), RepairNow::Running);
                r.wait(1);
            }
            assert_eq!(r.core.repair_note(&r.w), note);
            assert_eq!(row(&r, K) == RowState::RepairFirst, !active);
            assert!(r.w.playback_on(&OutputRef::Pc(out("headphones"))));
            assert!(!r.take().iter().any(|d| matches!(d, Do::Windows(_) | Do::Silence { then: Some(_), .. })));
        }
    }
}

#[test]
fn turning_on_a_saved_speakers_output_leaves_it_available_for_the_next_switch() {
    use crate::model::scene::RepairNow;
    let mut r = Rig::on_kitchen();
    r.w.our_device = false;
    r.w.output_turned_off = true;
    r.w.set_playback(OutputRef::Pc(out("speakers")));
    r.tick();
    r.cmd(Command::TurnOnOutput).unwrap();
    assert!(r.take().contains(&Do::TurnOnOutput));
    r.w.our_device = true;
    r.w.output_turned_off = false;
    r.wait(60_000);
    assert_eq!(r.core.repair_note(&r.w), RepairNow::Worked);
    assert!(r.w.playback_on(&OutputRef::Pc(out("speakers"))));
    r.switch_to(K);
    assert_eq!(r.core.repair_note(&r.w), RepairNow::NotAsked);
    assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
}

#[test]
fn observing_the_output_after_the_helper_exits_completes_repair() {
    use crate::model::scene::RepairNow;
    let mut r = Rig::new();
    r.w.our_device = false;
    r.cmd(Command::RepairOutput).unwrap();
    let run = r.take().into_iter().find_map(|d| match d { Do::RepairOutput(id) => Some(id), _ => None }).unwrap();
    r.event(Event::RepairApproved(run));
    r.event(Event::RepairExited(run, crate::platform::RepairExit::Success));
    r.wait(4_999);
    assert_eq!(r.core.repair_note(&r.w), RepairNow::Running);
    r.w.our_device = true;
    r.tick();
    assert_eq!(r.core.repair_note(&r.w), RepairNow::Worked);
}

#[test]
fn restoring_the_windows_default_after_repair_does_not_reconnect_kitchen() {
    for delay in [0, WINDOWS_DOING_MS] {
        let mut r = Rig::on_kitchen();
        r.w.our_device = false;
        r.w.output_turned_off = true;
        r.w.set_playback(OutputRef::Pc(out("speakers")));
        r.event(Event::Output(OutputEvent::DefaultChanged { at_ms: r.now.mono_ms }));
        r.cmd(Command::TurnOnOutput).unwrap();
        assert!(r.take().contains(&Do::Save));
        assert!(r.core.kept().named.is_none() && r.core.kept().place.is_none());
        r.w.output_turned_off = false;
        r.w.our_device = true;
        if delay > 0 {
            r.tick();
            r.wait(delay);
        }
        r.w.set_playback(OutputRef::Ours);
        r.event(Event::Output(OutputEvent::DefaultChanged { at_ms: r.now.mono_ms }));
        r.wait(WINDOWS_DOING_MS + 1);
        assert!(r.jobs.is_empty());
        assert!(r.core.kept().place.is_none());
        assert!(!r.core.no_speaker_chosen && r.core.popup.is_none());
        assert!(r.w.playback_on(&OutputRef::Pc(out("speakers"))));
    }
}

fn request_repair(r: &mut Rig) -> u64 {
    r.cmd(Command::RepairOutput).unwrap();
    r.take().into_iter().find_map(|action| match action {
        Do::RepairOutput(run) => Some(run),
        _ => None,
    }).expect("Repair should launch")
}

fn per_app_repair() -> (Rig, u64) {
    let mut r = Rig::new();
    r.w.settings.only_some_apps = true;
    r.plug_headphones(true);
    r.w.our_device = false;
    let run = request_repair(&mut r);
    (r, run)
}

#[test]
fn per_app_turn_on_checks_playback_when_the_output_returns_and_two_seconds_later() {
    let mut r = Rig::new();
    r.w.settings.only_some_apps = true;
    r.plug_headphones(true);
    r.w.our_device = false;
    r.w.output_turned_off = true;
    r.cmd(Command::TurnOnOutput).unwrap();
    r.wait(3_000);

    let headphones = OutputRef::Pc(out("headphones"));
    r.w.output_turned_off = false;
    r.w.our_device = true;
    r.w.default_output = Some(OutputRef::Ours);
    r.tick();
    assert_eq!(r.w.default_output, Some(headphones.clone()));
    assert_eq!(r.core.next_due(), Some(r.now.mono_ms + 2_000));

    r.w.multimedia_output = Some(OutputRef::Ours);
    r.wait(1_999);
    assert_eq!(r.w.multimedia_output, Some(OutputRef::Ours));
    r.wait(1);
    assert!(r.w.playback_on(&headphones));
}

#[test]
fn per_app_repair_checks_each_role_at_the_end_and_two_seconds_later() {
    use crate::platform::RepairExit;
    enum Approval { BeforeOutput, BeforeExit, AfterExit }
    for exit in [RepairExit::Success, RepairExit::Cancelled, RepairExit::Failed] {
        for approval in [Approval::BeforeOutput, Approval::BeforeExit, Approval::AfterExit] {
            for console_first in [true, false] {
                let (mut r, run) = per_app_repair();
                if matches!(approval, Approval::BeforeOutput) { r.event(Event::RepairApproved(run)); }
                r.w.our_device = true;
                if console_first { r.w.default_output = Some(OutputRef::Ours); }
                else { r.w.multimedia_output = Some(OutputRef::Ours); }
                r.event(Event::Output(OutputEvent::DefaultChanged { at_ms: r.now.mono_ms }));
                if matches!(approval, Approval::BeforeExit) { r.event(Event::RepairApproved(run)); }
                r.wait(12_000);
                assert!(r.w.uses_app_output());
                let headphones = WinTarget::Output(out("headphones"));
                assert!(!windows_to(&r.take(), &headphones));

                r.event(Event::RepairExited(run, exit));
                let pc = OutputRef::Pc(out("headphones"));
                assert!(r.w.playback_on(&pc));
                assert_eq!(r.core.next_due(), Some(r.now.mono_ms + 2_000));
                if matches!(approval, Approval::AfterExit) { r.event(Event::RepairApproved(run)); }

                if console_first { r.w.multimedia_output = Some(OutputRef::Ours); }
                else { r.w.default_output = Some(OutputRef::Ours); }
                r.wait(1_999);
                assert!(r.w.uses_app_output());
                r.wait(1);
                assert!(r.w.playback_on(&pc));

                r.choose_in_windows(OutputRef::Ours);
                r.wait(12_000);
                assert!(r.w.playback_on(&OutputRef::Ours));
            }
        }
    }
}

#[test]
fn per_app_repair_checks_when_the_output_appears_after_a_successful_exit() {
    use crate::platform::RepairExit;
    enum Approval { BeforeExit, BeforeOutput, AfterOutput }
    for approval in [Approval::BeforeExit, Approval::BeforeOutput, Approval::AfterOutput] {
        let (mut r, run) = per_app_repair();
        if matches!(approval, Approval::BeforeExit) { r.event(Event::RepairApproved(run)); }
        r.event(Event::RepairExited(run, RepairExit::Success));
        if matches!(approval, Approval::BeforeOutput) { r.event(Event::RepairApproved(run)); }
        r.wait(4_000);
        r.w.our_device = true;
        r.w.multimedia_output = Some(OutputRef::Ours);
        r.tick();
        assert!(r.w.playback_on(&OutputRef::Pc(out("headphones"))));
        if matches!(approval, Approval::AfterOutput) { r.event(Event::RepairApproved(run)); }
        r.w.default_output = Some(OutputRef::Ours);
        r.wait(1_999);
        assert_eq!(r.w.default_output, Some(OutputRef::Ours));
        r.wait(1);
        assert!(r.w.playback_on(&OutputRef::Pc(out("headphones"))));
        r.choose_in_windows(OutputRef::Ours);
        r.wait(12_000);
        assert!(r.w.playback_on(&OutputRef::Ours));
    }
}

#[test]
fn per_app_repair_checks_after_a_timeout_even_if_both_roles_were_on_the_pc() {
    for approved in [true, false] {
        let (mut r, run) = per_app_repair();
        if approved { r.event(Event::RepairApproved(run)); }
        let until = r.core.recovery.due().unwrap();
        r.now = r.now.later(until - r.now.mono_ms);
        r.tick();
        r.w.our_device = true;
        r.w.multimedia_output = Some(OutputRef::Ours);
        r.wait(1_999);
        assert_eq!(r.w.multimedia_output, Some(OutputRef::Ours));
        r.wait(1);
        assert!(r.w.playback_on(&OutputRef::Pc(out("headphones"))));
    }
}

#[test]
fn an_active_output_ends_an_unanswered_approval_and_unlocks_the_rows() {
    use crate::model::scene::RepairNow;
    let mut r = Rig::new();
    r.w.our_device = false;
    let run = request_repair(&mut r);
    assert_eq!(row(&r, K), RowState::RepairFirst);
    r.w.our_device = true;
    r.tick();
    assert_eq!(r.core.repair_note(&r.w), RepairNow::Worked);
    assert_ne!(row(&r, K), RowState::RepairFirst);
    r.event(Event::RepairApproved(run));
    r.event(Event::RepairExited(run, crate::platform::RepairExit::Cancelled));
    assert_eq!(r.core.repair_note(&r.w), RepairNow::Worked);
}

#[test]
fn repair_can_launch_again_at_five_minutes_and_ignores_the_old_prompt() {
    use crate::model::scene::RepairNow;
    let mut r = Rig::new();
    r.w.our_device = false;
    let first = request_repair(&mut r);
    let deadline = r.now.mono_ms + 300_000;
    assert_eq!(r.core.recovery.due(), Some(deadline));
    r.now = r.now.later(299_999);
    r.cmd(Command::RepairOutput).unwrap();
    assert!(!r.take().iter().any(|a| matches!(a, Do::RepairOutput(_))));
    r.now = r.now.later(1);
    let second = request_repair(&mut r);
    assert_ne!(first, second);
    r.event(Event::RepairApproved(first));
    r.event(Event::RepairExited(first, crate::platform::RepairExit::Cancelled));
    assert_eq!(r.core.repair_note(&r.w), RepairNow::NotAsked);
    r.event(Event::RepairApproved(second));
    assert_eq!(r.core.repair_note(&r.w), RepairNow::Running);
}
