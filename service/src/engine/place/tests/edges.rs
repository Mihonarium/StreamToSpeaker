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
                r.w.default_output = Some(OutputRef::Pc(out("speakers")));
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
    w.default_output = Some(OutputRef::Ours);
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
