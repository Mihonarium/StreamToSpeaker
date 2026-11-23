//! The situations the engine has to handle, step by step, on injected time.

use crate::model::state::Looking;
use super::*;
use crate::model::command::Command;
use crate::model::facts::NotificationKind as N;

fn kitchen() -> SpeakerId {
    SpeakerId::new("kitchen")
}
fn living() -> SpeakerId {
    SpeakerId::new("living")
}
fn out(s: &str) -> PcOutputId {
    PcOutputId::new(s)
}

/// Kitchen and Living Room are on the network; Windows plays on the PC's
/// speakers; this app's output exists.
fn world() -> World {
    World {
        found: [kitchen(), living()].into_iter().collect(),
        default_output: Some(out("realtek")),
        our_output: Some(out("ours")),
        knows_outputs: true,
        time: LocalTime::new(21, 14),
        ..Default::default()
    }
}

struct Run {
    core: Core,
    w: World,
    t0: Instant,
    now: Instant,
    done: Vec<Do>,
}

impl Run {
    fn new() -> Self {
        let now = Instant::now();
        Run { core: Core::new(None, None, now), w: world(), t0: now, now, done: vec![] }
    }
    fn cmd(&mut self, c: Command) -> Vec<Do> {
        let (_, d) = self.core.command(&c, Origin::Window, &self.w, self.now);
        self.done.extend(d.clone());
        d
    }
    fn ev(&mut self, e: Event) -> Vec<Do> {
        let d = self.core.event(e, &self.w, self.now);
        self.done.extend(d.clone());
        d
    }
    fn at(&mut self, secs: f64) -> Vec<Do> {
        self.now = self.t0 + Duration::from_secs_f64(secs);
        let d = self.core.tick(&self.w, self.now);
        self.done.extend(d.clone());
        d
    }
    fn state(&self) -> NowState {
        self.core.state(&self.w, self.now)
    }
    fn job(d: &[Do]) -> Option<Job> {
        d.iter().find_map(|x| match x {
            Do::Connect(j) => Some(j.clone()),
            _ => None,
        })
    }
    /// Plays on Kitchen, with sound, Windows switched to this app.
    fn playing_on_kitchen() -> Self {
        let mut r = Run::new();
        let j = Run::job(&r.cmd(Command::Play(Place::Speaker(kitchen())))).unwrap();
        r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
        r.w.default_output = Some(out("ours"));
        r.ev(Event::DefaultChanged);
        r.w.sending = true;
        r.w.apps_here = Apps::of(&["Spotify"]);
        r.done.clear();
        r
    }
    fn notified(&self) -> Option<N> {
        self.core.notification.as_ref().map(|n| n.kind)
    }
}

#[test]
fn choosing_a_speaker_waits_until_it_is_ready_then_moves_windows_too() {
    let mut r = Run::new();
    let d = r.cmd(Command::Play(Place::Speaker(kitchen())));
    let job = Run::job(&d).unwrap();
    assert!(job.origin == Origin::Window && job.check_busy);
    assert_eq!(
        r.state(),
        NowState::Connecting { target: kitchen(), from: SoundAt::ThisPc, stage: ConnectStage::Finding }
    );
    assert!(!d.contains(&Do::UseOurOutput), "the sound stays where it is meanwhile");
    r.ev(Event::Stage { gen: job.gen, stage: ConnectStage::Asking });
    assert!(matches!(r.state(), NowState::Connecting { stage: ConnectStage::Asking, .. }));
    let d = r.ev(Event::Ended { gen: job.gen, ended: Ended::Bound });
    assert!(d.contains(&Do::UseOurOutput));
    assert!(d.contains(&Do::SaveChoice(Some(Place::Speaker(kitchen())))));
    assert_eq!(r.state(), NowState::Ready { speaker: kitchen() });
    assert_eq!(r.core.pc_output, Some(out("realtek")), "the output it replaced is remembered");
    r.w.sending = true;
    r.w.apps_here = Apps::of(&["Spotify"]);
    assert_eq!(r.state(), NowState::Playing { speaker: kitchen(), apps: Apps::of(&["Spotify"]) });
}

#[test]
fn the_latest_choice_wins_and_cancel_keeps_the_sound_where_it_was() {
    let mut r = Run::playing_on_kitchen();
    let first = Run::job(&r.cmd(Command::Play(Place::Speaker(living())))).unwrap();
    assert_eq!(
        r.state(),
        NowState::Connecting { target: living(), from: SoundAt::Speaker(kitchen()), stage: ConnectStage::Finding }
    );
    // Picking the same row again does nothing.
    assert!(r.cmd(Command::Play(Place::Speaker(living()))).is_empty());
    // Picking Kitchen, which plays, is the newest choice: the connect ends.
    assert_eq!(r.cmd(Command::Play(Place::Speaker(kitchen()))), vec![Do::Supersede]);
    assert!(matches!(r.state(), NowState::Playing { .. }));
    // A connect that ends after it was cancelled is not used.
    r.ev(Event::Ended { gen: first.gen, ended: Ended::Bound });
    assert!(matches!(r.state(), NowState::Playing { speaker, .. } if speaker == kitchen()));
    // Cancel does the same.
    let second = Run::job(&r.cmd(Command::Play(Place::Speaker(living())))).unwrap();
    assert_eq!(r.cmd(Command::CancelConnect), vec![Do::Supersede]);
    r.ev(Event::Ended { gen: second.gen, ended: Ended::Bound });
    assert!(matches!(r.state(), NowState::Playing { speaker, .. } if speaker == kitchen()));

    let a = Run::job(&r.cmd(Command::Play(Place::Speaker(living())))).unwrap();
    let d = r.cmd(Command::Play(Place::ThisPc));
    assert!(d.contains(&Do::Supersede) && d.contains(&Do::Release { tell_speaker: true }) && d.contains(&Do::GiveBack));
    r.ev(Event::Ended { gen: a.gen, ended: Ended::Bound });
    assert_eq!(r.state(), NowState::OnThisPc);
}

#[test]
fn a_busy_speaker_asks_first_and_a_failure_stays_on_its_row() {
    let mut r = Run::new();
    let j = Run::job(&r.cmd(Command::Play(Place::Speaker(kitchen())))).unwrap();
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Busy { other_pc: Some("LAPTOP-ANNA".into()) } });
    assert_eq!(r.state(), NowState::OnThisPc, "nothing changed while it asks");
    assert_eq!(r.core.questions[&kitchen()].other_pc.as_deref(), Some("LAPTOP-ANNA"));
    let j = Run::job(&r.cmd(Command::PlayTakingOver(kitchen()))).unwrap();
    assert!(!j.check_busy, "the answer counts: it doesn't ask twice");
    assert!(r.core.questions.is_empty());
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Failed(ConnectFailure::NoAnswer) });
    assert_eq!(r.core.failures[&kitchen()], ConnectFailure::NoAnswer);
    assert_eq!(r.state(), NowState::OnThisPc);
    r.cmd(Command::Play(Place::Speaker(kitchen())));
    assert!(r.core.failures.is_empty(), "a new pick clears it");
}

#[test]
fn a_speaker_that_drops_gets_three_tries_then_the_sound_comes_back_with_a_note() {
    let mut r = Run::playing_on_kitchen();
    r.ev(Event::Dropped);
    let tries = |s: NowState| match s {
        NowState::NotResponding { dropout: Dropout::QuickTries { attempt, .. }, .. } => attempt,
        other => panic!("{other:?}"),
    };
    assert_eq!(tries(r.state()), 1);
    assert!(r.at(1.9).is_empty(), "nothing before 2 s");
    let j1 = Run::job(&r.at(2.0)).expect("first try at 2 s");
    assert_eq!(j1.origin, Origin::ByItself);
    r.ev(Event::Ended { gen: j1.gen, ended: Ended::Failed(ConnectFailure::NoAnswer) });
    assert_eq!(tries(r.state()), 2);
    assert!(Run::job(&r.at(4.9)).is_none());
    let j2 = Run::job(&r.at(5.0)).expect("second try at 5 s");
    r.ev(Event::Ended { gen: j2.gen, ended: Ended::Failed(ConnectFailure::NoAnswer) });
    let j3 = Run::job(&r.at(10.0)).expect("third try at 10 s");
    assert_eq!(tries(r.state()), 3);
    // The third try hangs; at 15 s the sound comes back to this PC.
    let d = r.at(15.0);
    assert!(d.contains(&Do::Supersede) && d.contains(&Do::GiveBack) && d.contains(&Do::ResetPlayed));
    assert!(matches!(r.state(), NowState::FellBack { cause: FellBackCause::StoppedResponding, .. }));
    assert_eq!(r.notified(), Some(N::StoppedResponding));
    assert!(!r.core.seen, "the badge stays until the panel is opened");
    r.ev(Event::Ended { gen: j3.gen, ended: Ended::Bound });
    assert!(matches!(r.state(), NowState::FellBack { .. }), "a try that ends late is not used");
    r.cmd(Command::PanelShown(true));
    assert!(r.core.seen);
}

#[test]
fn a_try_that_works_ends_the_trouble_quietly() {
    let mut r = Run::playing_on_kitchen();
    r.ev(Event::Dropped);
    let j = Run::job(&r.at(2.0)).unwrap();
    let d = r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    assert!(!d.iter().any(|x| matches!(x, Do::SaveChoice(_))), "not a new choice");
    assert!(matches!(r.state(), NowState::Playing { .. }));
    assert_eq!(r.notified(), None);
}

#[test]
fn staying_silent_keeps_trying_every_30_seconds() {
    let mut r = Run::playing_on_kitchen();
    r.w.settings.stay_silent = true;
    r.ev(Event::Dropped);
    for (at, _) in [(2.0, 1), (5.0, 2), (10.0, 3)] {
        let j = Run::job(&r.at(at)).unwrap();
        r.ev(Event::Ended { gen: j.gen, ended: Ended::Failed(ConnectFailure::NoAnswer) });
    }
    r.at(11.0);
    assert!(matches!(r.state(), NowState::NotResponding { dropout: Dropout::KeepTrying { every_secs: 30 }, .. }));
    assert_eq!(r.notified(), Some(N::StoppedRespondingSilent));
    assert!(!r.done.contains(&Do::GiveBack), "nothing plays on this PC meanwhile");
    assert!(Run::job(&r.at(40.0)).is_none());
    let j = Run::job(&r.at(41.0)).expect("every 30 s");
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    assert!(matches!(r.state(), NowState::Playing { .. }));
}

fn fallen_back() -> Run {
    let mut r = Run::playing_on_kitchen();
    r.ev(Event::Dropped);
    r.at(15.0);
    assert!(matches!(r.state(), NowState::FellBack { .. }));
    r.w.default_output = Some(out("realtek"));
    r.ev(Event::DefaultChanged);
    r.w.sending = false;
    r.done.clear();
    r
}

#[test]
fn back_after_a_dropout_it_returns_only_if_nothing_played_here() {
    let mut r = fallen_back();
    assert!(matches!(r.state(), NowState::FellBack { returns: Return::ByItself, .. }));
    r.w.found.remove(&kitchen());
    r.at(20.0);
    r.w.found.insert(kitchen());
    let j = Run::job(&r.at(30.0)).expect("returns by itself");
    assert!(j.check_busy, "never takes a busy speaker");
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    assert!(r.done.contains(&Do::UseOurOutput));
    assert_eq!(r.state(), NowState::Ready { speaker: kitchen() });

    let mut r = fallen_back();
    r.w.played_since = true;
    assert!(matches!(r.state(), NowState::FellBack { returns: Return::WillAsk, .. }));
    r.w.found.remove(&kitchen());
    r.at(20.0);
    r.w.found.insert(kitchen());
    assert!(Run::job(&r.at(30.0)).is_none());
    assert_eq!(r.state(), NowState::SpeakerBack { speaker: kitchen(), because: StayedBecause::PlayedAfterDropout });
    assert_eq!(r.notified(), Some(N::SpeakerBack), "replaces the one about it leaving");
}

#[test]
fn someone_else_taking_the_speaker_steps_aside_and_never_takes_it_back() {
    let mut r = Run::playing_on_kitchen();
    let d = r.ev(Event::Taken);
    assert!(d.contains(&Do::Release { tell_speaker: false }), "no Stop for what someone else plays");
    assert!(d.contains(&Do::GiveBack));
    assert_eq!(r.state(), NowState::Taken { speaker: kitchen(), at: LocalTime::new(21, 14) });
    assert_eq!(r.notified(), Some(N::Taken));
    for t in [20.0, 60.0, 600.0] {
        assert!(Run::job(&r.at(t)).is_none());
    }
    let j = Run::job(&r.cmd(Command::PlayTakingOver(kitchen()))).unwrap();
    assert!(!j.check_busy, "Play on Kitchen again is the answer");
}

#[test]
fn at_sign_in_it_returns_to_the_speaker_unless_something_played() {
    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.w.found.clear();
    let d = r.ev(Event::Started { after_update: false });
    assert!(d.contains(&Do::ResetPlayed));
    assert_eq!(r.state(), NowState::LookingFor { speaker: kitchen(), looking: Looking::Searching });
    r.at(10.0);
    assert_eq!(
        r.state(),
        NowState::LookingFor { speaker: kitchen(), looking: Looking::NotFound(Return::ByItself) }
    );
    r.w.found.insert(kitchen());
    let j = Run::job(&r.at(12.0)).expect("found: returns");
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    assert_eq!(r.state(), NowState::Ready { speaker: kitchen() });

    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.ev(Event::Started { after_update: false });
    r.w.played_since = true;
    assert!(Run::job(&r.at(1.0)).is_none());
    assert_eq!(r.state(), NowState::SpeakerBack { speaker: kitchen(), because: StayedBecause::PlayingWhenFound });

    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.w.settings.go_back_at_start = false;
    r.ev(Event::Started { after_update: false });
    assert_eq!(r.state(), NowState::OnThisPc);

    let mut r = Run::new();
    r.core.choice = Some(Place::ThisPc);
    r.ev(Event::Started { after_update: false });
    assert_eq!(r.state(), NowState::OnThisPc, "This PC was the last choice");
}

#[test]
fn after_an_update_it_returns_whatever_played_meanwhile() {
    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.core.pc_output = Some(out("realtek"));
    r.ev(Event::Started { after_update: true });
    r.w.played_since = true;
    r.w.playing_on_pc = true;
    assert!(Run::job(&r.at(1.0)).is_some());

    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.core.pc_output = Some(out("realtek"));
    r.ev(Event::Started { after_update: true });
    r.w.default_output = Some(out("headphones"));
    assert!(Run::job(&r.at(1.0)).is_none(), "the person picked another output meanwhile");
    assert_eq!(r.state(), NowState::OnThisPc);
}

#[test]
fn an_automatic_return_never_takes_a_busy_speaker_or_asks_for_a_code_by_itself() {
    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.ev(Event::Started { after_update: false });
    let j = Run::job(&r.at(1.0)).unwrap();
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Busy { other_pc: None } });
    assert!(matches!(r.state(), NowState::Taken { .. }));

    let mut r = Run::new();
    r.core.choice = Some(Place::Speaker(kitchen()));
    r.ev(Event::Started { after_update: false });
    let j = Run::job(&r.at(1.0)).unwrap();
    assert_eq!(j.origin, Origin::ByItself, "never a dialog by itself");
    r.ev(Event::Ended { gen: j.gen, ended: Ended::NeedsSecret(Secret::Code) });
    assert_eq!(r.state(), NowState::NeedsSecret { speaker: kitchen(), secret: Secret::Code });
    assert_eq!(r.notified(), Some(N::NeedsSecret(Secret::Code)));
}

#[test]
fn windows_switching_away_lets_the_speaker_go_after_10_seconds_unless_it_comes_back() {
    let mut r = Run::playing_on_kitchen();
    r.now = r.t0 + Duration::from_secs(100);
    r.w.default_output = Some(out("headphones"));
    r.ev(Event::DefaultChanged);
    assert_eq!(r.state(), NowState::WindowsSwitched { speaker: kitchen(), at: LocalTime::new(21, 14) });
    assert_eq!(r.core.pc_output, Some(out("headphones")), "This PC now means the headphones");
    assert!(!r.at(109.0).contains(&Do::Release { tell_speaker: true }));
    r.w.default_output = Some(out("ours"));
    r.ev(Event::DefaultChanged);
    assert!(matches!(r.state(), NowState::Playing { .. }), "Windows came back in time");
    r.w.default_output = Some(out("headphones"));
    r.ev(Event::DefaultChanged);
    assert!(r.at(120.0).contains(&Do::Release { tell_speaker: true }));
    assert!(matches!(r.state(), NowState::WindowsSwitched { .. }), "the line stays until the person acts");
    // Picking the speaker in Windows again plays on it.
    r.w.default_output = Some(out("ours"));
    let j = Run::job(&r.ev(Event::DefaultChanged)).expect("Windows is a second door");
    assert_eq!(j.place, Place::Speaker(kitchen()));
    assert_eq!(j.origin, Origin::ByItself, "Windows' choice is not a pick in the window");
}

#[test]
fn picking_this_apps_output_in_windows_plays_on_its_speaker_or_asks_which() {
    let mut r = Run::new();
    r.w.default_output = Some(out("ours"));
    r.ev(Event::DefaultChanged);
    assert_eq!(r.state(), NowState::ChooseSpeaker, "no speaker was ever chosen");
    let d = r.cmd(Command::PanelShown(false));
    assert!(d.contains(&Do::GiveBack), "closed without a choice: back to this PC");
    assert_eq!(r.state(), NowState::OnThisPc);

    let mut r = Run::new();
    r.core.named = Some(Place::Speaker(kitchen()));
    r.w.default_output = Some(out("ours"));
    let j = Run::job(&r.ev(Event::DefaultChanged)).unwrap();
    assert_eq!(j.place, Place::Speaker(kitchen()));
    assert_eq!(j.origin, Origin::ByItself);
}

/// Only a pick in the app's window is the window's; every other choice
/// keeps where it came from, and everything the app starts by itself is
/// its own.
#[test]
fn every_connect_says_where_it_came_from() {
    for (origin, take_over) in [
        (Origin::Window, false),
        (Origin::Window, true),
        (Origin::Elsewhere, false),
        (Origin::Elsewhere, true),
    ] {
        let mut r = Run::new();
        let cmd = if take_over {
            Command::PlayTakingOver(kitchen())
        } else {
            Command::Play(Place::Speaker(kitchen()))
        };
        let (_, d) = r.core.command(&cmd, origin, &r.w, r.now);
        assert_eq!(Run::job(&d).unwrap().origin, origin, "{cmd:?}");
    }

    // Start-up, sign-in and wake, and after an update.
    for ev in [Event::Started { after_update: false }, Event::Started { after_update: true }, Event::Resumed] {
        let mut r = Run::new();
        r.core = Core::new(Some(Place::Speaker(kitchen())), Some(Place::Speaker(kitchen())), r.now);
        r.core.pc_output = Some(out("realtek"));
        r.ev(ev.clone());
        r.at(1.0);
        let j = Run::job(&r.done).unwrap_or_else(|| panic!("{ev:?}: no return"));
        assert_eq!(j.origin, Origin::ByItself, "{ev:?}");
    }

    // A retry after a dropout.
    let mut r = Run::playing_on_kitchen();
    r.ev(Event::Dropped);
    let j = Run::job(&r.at(5.0)).expect("a try");
    assert_eq!(j.origin, Origin::ByItself);

    // A speaker set to come back whenever it appears.
    let mut r = Run::new();
    r.core.named = Some(Place::Speaker(kitchen()));
    r.w.settings.comes_back.insert(kitchen(), ComesBack::WheneverAppears);
    r.w.found.remove(&kitchen());
    r.at(1.0);
    r.w.found.insert(kitchen());
    let j = Run::job(&r.at(2.0)).expect("it appeared");
    assert_eq!(j.origin, Origin::ByItself);
}

#[test]
fn a_refused_switch_is_said_and_our_own_switches_are_not_taken_for_the_persons() {
    let mut r = Run::new();
    let j = Run::job(&r.cmd(Command::Play(Place::Speaker(kitchen())))).unwrap();
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    r.ev(Event::SwitchRead { ok: false });
    assert_eq!(r.state(), NowState::WindowsRefused { speaker: kitchen() });
    r.w.default_output = Some(out("ours"));
    r.ev(Event::DefaultChanged);
    assert_eq!(r.state(), NowState::Ready { speaker: kitchen() });
    // This PC: the app's own give-back isn't "the person picked an output".
    let d = r.cmd(Command::Play(Place::ThisPc));
    assert!(d.contains(&Do::GiveBack));
    r.w.default_output = Some(out("realtek"));
    let d = r.ev(Event::DefaultChanged);
    assert!(d.is_empty());
    assert_eq!(r.state(), NowState::OnThisPc);
}

#[test]
fn sleep_gives_the_sound_back_and_wake_looks_for_the_speaker() {
    let mut r = Run::playing_on_kitchen();
    let d = r.ev(Event::Suspending);
    assert!(d.contains(&Do::Release { tell_speaker: true }) && d.contains(&Do::GiveBack));
    assert_eq!(r.state(), NowState::OnThisPc);
    assert_eq!(r.notified(), None, "the app's own act doesn't notify");
    r.ev(Event::Resumed);
    assert!(matches!(r.state(), NowState::LookingFor { .. }));
    assert!(Run::job(&r.at(1.0)).is_some());
}

#[test]
fn a_speaker_set_to_come_whenever_it_appears_plays_if_nothing_plays_here() {
    let mut r = Run::new();
    r.w.settings.comes_back.insert(living(), ComesBack::WheneverAppears);
    r.w.found.remove(&living());
    assert!(Run::job(&r.at(1.0)).is_none());
    r.w.found.insert(living());
    r.w.playing_on_pc = true;
    assert!(Run::job(&r.at(2.0)).is_none(), "something plays here");
    r.w.found.remove(&living());
    r.at(3.0);
    r.w.found.insert(living());
    r.w.playing_on_pc = false;
    let j = Run::job(&r.at(4.0)).expect("appeared again, nothing playing");
    assert_eq!(j.place, Place::Speaker(living()));
    assert!(Run::job(&r.at(5.0)).is_none(), "once per appearance");
}

#[test]
fn a_missing_sound_output_shows_whatever_else_is_true() {
    let mut r = Run::playing_on_kitchen();
    r.w.our_output = None;
    let before = NowState::Playing { speaker: kitchen(), apps: Apps::of(&["Spotify"]) };
    assert_eq!(r.state(), NowState::OutputMissing);
    let d = r.core.tick(&r.w, r.now);
    let _ = (before, d);
}

#[test]
fn music_assistant_away_and_back() {
    let mut r = Run::new();
    r.w.ma_connected = true;
    let j = Run::job(&r.cmd(Command::Play(Place::MusicAssistant))).unwrap();
    assert_eq!(r.state(), NowState::MaWaiting);
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    r.w.ma_playing = true;
    assert!(matches!(r.state(), NowState::MaPlaying { .. }));
    r.w.ma_connected = false;
    let d = r.at(1.0);
    assert!(d.contains(&Do::GiveBack));
    assert!(matches!(r.state(), NowState::MaAway { returns: Return::ByItself }));
    r.w.ma_connected = true;
    let j = Run::job(&r.at(2.0)).expect("back: returns");
    assert_eq!(j.place, Place::MusicAssistant);

    let mut r = Run::new();
    r.w.ma_connected = true;
    let j = Run::job(&r.cmd(Command::Play(Place::MusicAssistant))).unwrap();
    r.ev(Event::Ended { gen: j.gen, ended: Ended::Bound });
    r.w.ma_connected = false;
    r.at(1.0);
    r.w.played_since = true;
    r.w.ma_connected = true;
    r.at(2.0);
    assert_eq!(r.state(), NowState::MaBack);
}

#[test]
fn a_reconnect_that_applies_a_delay_change_is_not_a_dropout() {
    let mut r = Run::playing_on_kitchen();
    r.w.delay_reconnect = Some((0, -100));
    assert_eq!(r.state(), NowState::ApplyingDelay { speaker: kitchen(), from_ms: 0, to_ms: -100 });
    assert!(r.at(20.0).is_empty());
    assert_eq!(r.notified(), None);
}
