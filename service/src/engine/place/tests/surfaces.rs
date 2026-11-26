//! Every command carries the surface it came from. What only the app's own
//! windows may do (answer a confirmation where it shows, first run, the ×
//! of the note or of a pop-up, removing or resetting a speaker, the switch
//! that skips every question) is refused from any other surface and changes
//! nothing; the same command from the window works.

use super::rig::*;

const OTHERS: [Surface; 5] = [Surface::Tray, Surface::Shortcut, Surface::Phone, Surface::CommandLine, Surface::Api];

/// A moment, the command, and the surfaces that may send it.
struct Case {
    name: &'static str,
    given: fn() -> Rig,
    cmd: fn() -> Command,
    allowed: &'static [Surface],
    /// What the command does from an allowed surface.
    works: fn(&Rig, &[Do]),
}

fn busy_kitchen() -> Rig {
    let mut r = Rig::new();
    r.speaker(K, SpeakerSignal::PlayingElse { who: None });
    r
}

/// A screen reader runs: a switch asks first, since its voice would play
/// on the speaker too.
fn with_screen_reader() -> Rig {
    let mut r = Rig::new();
    r.w.screen_reader = true;
    r
}

fn confirmation_in_panel() -> Rig {
    let mut r = with_screen_reader();
    r.open_panel();
    r.cmd(Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
    assert!(r.sit().confirm_in_panel.is_some(), "set-up: it asks in the panel");
    r
}

fn confirmation_by_the_clock() -> Rig {
    let mut r = with_screen_reader();
    r.cmd_from(Surface::Tray, Command::Switch { place: Place::Speaker(id(K)), way: None }).unwrap();
    assert!(r.sit().popups.confirmation.is_some(), "set-up: it asks by the clock");
    r
}

fn first_run_on_step_two() -> Rig {
    let mut r = Rig::with(Kept::default(), world());
    r.cmd(Command::FirstRunAction(FirstRunAction::Chime(id(K)))).unwrap();
    r.end(Purpose::Chime, Ended::Ready);
    r
}

/// The app moved the sound to this PC: the note stands and shows by the
/// clock.
fn a_note_by_the_clock() -> Rig {
    let mut r = Rig::playing_on_kitchen();
    r.speaker(K, SpeakerSignal::PlayingElse { who: None });
    r
}

const WINDOW: &[Surface] = &[Surface::Panel, Surface::Settings];
const PANEL_OR_POPUP: &[Surface] = &[Surface::Panel, Surface::Settings, Surface::Notification];

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "the answer to a confirmation in the panel",
            given: confirmation_in_panel,
            cmd: || Command::Confirm { yes: true },
            allowed: WINDOW,
            works: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert!(r.job(Purpose::Switch).in_window);
            },
        },
        Case {
            name: "the answer to a confirmation by the clock",
            given: confirmation_by_the_clock,
            cmd: || Command::Confirm { yes: true },
            allowed: &[Surface::Notification],
            works: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert!(!r.job(Purpose::Switch).in_window, "the switch is still the tray's");
            },
        },
        Case {
            name: "a confirmation's Cancel",
            given: confirmation_in_panel,
            cmd: || Command::Confirm { yes: false },
            allowed: WINDOW,
            works: |r, _| assert!(r.sit().confirm_in_panel.is_none()),
        },
        Case {
            name: "first run's I heard it",
            given: first_run_on_step_two,
            cmd: || Command::FirstRunAction(FirstRunAction::Heard),
            allowed: WINDOW,
            works: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert!(r.job(Purpose::Switch).in_window);
            },
        },
        Case {
            name: "first run's Use Kitchen without checking",
            given: first_run_on_step_two,
            cmd: || Command::FirstRunAction(FirstRunAction::UseWithoutChecking),
            allowed: WINDOW,
            works: |_, dos| assert!(connects(dos, Purpose::Switch)),
        },
        Case {
            name: "first run's chime",
            given: || Rig::with(Kept::default(), world()),
            cmd: || Command::FirstRunAction(FirstRunAction::Chime(id(B))),
            allowed: WINDOW,
            works: |_, dos| assert!(connects(dos, Purpose::Chime)),
        },
        Case {
            name: "first run's ×",
            given: || Rig::with(Kept::default(), world()),
            cmd: || Command::FirstRunAction(FirstRunAction::Close),
            allowed: WINDOW,
            works: |r, _| assert_eq!(r.sit().first_run, None),
        },
        Case {
            name: "Show the welcome again",
            given: Rig::new,
            cmd: || Command::FirstRunAction(FirstRunAction::ShowAgain),
            allowed: WINDOW,
            works: |r, _| assert_eq!(r.sit().first_run, Some(FirstRunStep::One)),
        },
        Case {
            name: "removing the place's speaker",
            given: Rig::playing_on_kitchen,
            cmd: || Command::RemoveSpeaker(id(K)),
            allowed: WINDOW,
            works: |r, dos| {
                assert!(silenced(dos, K) && asks_no_player(dos));
                assert_eq!(r.sit().spot, Spot::ThisPc(Some(out("speakers"))));
            },
        },
        Case {
            name: "resetting a speaker",
            given: || {
                let mut r = Rig::new();
                r.cmd(Command::ChooseWay { speaker: id(OFFICE), way: WayChoice::AirPlayShort }).unwrap();
                r
            },
            cmd: || Command::ResetSpeaker(id(OFFICE)),
            allowed: WINDOW,
            works: |r, _| assert_eq!(r.sit().speaker(&id(OFFICE)).unwrap().way, WayChoice::AirPlaySteady),
        },
        Case {
            name: "the note's ×",
            given: a_note_by_the_clock,
            cmd: || Command::CloseNote,
            allowed: PANEL_OR_POPUP,
            works: |r, _| assert_eq!((note(r), popup(r)), (None, None)),
        },
        Case {
            name: "a pop-up's ×",
            given: a_note_by_the_clock,
            cmd: || Command::ClosePopup,
            allowed: PANEL_OR_POPUP,
            works: |r, _| assert!(popup(r).is_none() && note(r).is_some()),
        },
        Case {
            name: "the panel opening",
            given: a_note_by_the_clock,
            cmd: || Command::PanelShown(true),
            allowed: WINDOW,
            works: |r, _| assert!(r.core.panel_open),
        },
        Case {
            name: "a switch that has to ask first",
            given: with_screen_reader,
            cmd: || Command::Switch { place: Place::Speaker(id(K)), way: None },
            allowed: &[Surface::Panel, Surface::Settings, Surface::Tray, Surface::Notification],
            works: |r, dos| {
                assert!(!connects(dos, Purpose::Switch), "it asks first");
                let s = r.sit();
                assert!(s.confirm_in_panel.is_some() || s.popups.confirmation.is_some());
            },
        },
        Case {
            name: "the switch that skips every question",
            given: busy_kitchen,
            cmd: || Command::PlayTakingOver(id(K)),
            allowed: WINDOW,
            works: |r, dos| {
                assert!(connects(dos, Purpose::Switch));
                assert!(!r.job(Purpose::Switch).look_first);
            },
        },
        Case {
            name: "a chime on a busy speaker",
            given: busy_kitchen,
            cmd: || Command::PlayChime { speaker: id(K), confirmed: false },
            allowed: WINDOW,
            works: |r, dos| {
                assert!(!connects(dos, Purpose::Chime), "it asks first");
                let s = r.sit();
                assert!(s.confirm_in_panel.is_some() || s.popups.confirmation.is_some());
            },
        },
        Case {
            name: "a chime said to be confirmed",
            given: busy_kitchen,
            cmd: || Command::PlayChime { speaker: id(K), confirmed: true },
            allowed: WINDOW,
            works: |r, dos| {
                assert!(connects(dos, Purpose::Chime));
                assert!(r.job(Purpose::Chime).in_window);
            },
        },
        Case {
            name: "Try again where a switch has to ask first",
            given: with_screen_reader,
            cmd: || Command::TryAgain(id(K)),
            allowed: &[Surface::Panel, Surface::Settings, Surface::Tray, Surface::Notification],
            works: |r, dos| {
                assert!(!connects(dos, Purpose::TryAgain));
                let s = r.sit();
                assert!(s.confirm_in_panel.is_some() || s.popups.confirmation.is_some());
            },
        },
    ]
}

#[test]
fn window_only_commands_are_refused_elsewhere_and_change_nothing() {
    let all = [
        Surface::Panel,
        Surface::Settings,
        Surface::Notification,
        Surface::Tray,
        Surface::Shortcut,
        Surface::Phone,
        Surface::CommandLine,
        Surface::Api,
    ];
    for case in cases() {
        for from in all {
            let mut r = (case.given)();
            r.take();
            let before = (r.sit(), r.core.kept(), r.jobs.clone());
            let result = r.cmd_from(from, (case.cmd)());
            let dos = r.take();
            if case.allowed.contains(&from) {
                assert_eq!(result, Ok(Effect::Done), "{}, from {from:?}", case.name);
                let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (case.works)(&r, &dos)));
                if check.is_err() {
                    panic!("{}, from {from:?}: it didn't work\n  asked: {dos:?}\n  situation: {:?}", case.name, r.sit());
                }
            } else {
                assert!(result.is_err(), "{}, from {from:?}: not refused", case.name);
                assert!(dos.is_empty(), "{}, from {from:?}: asked for {dos:?}", case.name);
                let after = (r.sit(), r.core.kept(), r.jobs.clone());
                assert_eq!(after, before, "{}, from {from:?}: something changed", case.name);
            }
        }
    }
    // Every surface outside the window is covered.
    assert_eq!(OTHERS.len() + 3, all.len());
}

#[test]
fn a_receiver_that_announces_another_speakers_identity_plays_only_from_the_window() {
    for from in OTHERS.iter().copied().chain([Surface::Notification]) {
        for cmd in [Command::Switch { place: Place::Speaker(id(B)), way: None }, Command::TryAgain(id(B))] {
            let mut r = Rig::new();
            r.w.speakers[1].only_from_window = true;
            r.take();
            let result = r.cmd_from(from, cmd.clone());
            assert_eq!(result, Err(Refusal::OnlyFromWindow { speaker: id(B) }), "{cmd:?} from {from:?}");
            assert!(r.take().is_empty());
        }
    }
    let mut r = Rig::new();
    r.w.speakers[1].only_from_window = true;
    r.cmd(Command::Switch { place: Place::Speaker(id(B)), way: None }).unwrap();
    assert!(r.job(Purpose::Switch).in_window, "from the panel it may, and may ask for a code");
}
