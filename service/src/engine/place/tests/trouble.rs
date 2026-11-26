//! When the speaker can't play: the tries, what the note says, the try at
//! a sound's start, the speaker's own pause and play, and the engine's
//! manners.

use super::rig::*;

fn this_pc() -> Spot {
    Spot::ThisPc(Some(out("speakers")))
}

/// Kitchen stopped responding while Spotify played on it.
fn dropped() -> Rig {
    let mut r = Rig::playing_on_kitchen();
    r.speaker(K, SpeakerSignal::NotResponding);
    r
}

/// … and didn't respond within the tries: the sound is on this PC.
fn moved() -> Rig {
    let mut r = dropped();
    r.wait(10_250);
    assert_eq!(r.sit().spot, this_pc(), "set-up: the app moved the sound");
    r
}

/// Kitchen is the place and its stream has rested.
fn rested() -> Rig {
    let mut r = Rig::on_kitchen();
    r.wait_long(31 * 60_000);
    r
}

/// … and a player started: the try is on its way.
fn starting() -> Rig {
    let mut r = rested();
    r.play(SPOTIFY);
    r
}

#[test]
fn the_note_says_why_the_sound_moved() {
    run(&[
        Row {
            name: "a speaker the app can't reach isn't told to stop",
            given: dropped,
            when: |r| r.wait(10_250),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert!(!silenced(dos, K), "it plays out what it has");
                assert!(matches!(cause(r), Some(Cause::StoppedResponding { vpn: None, .. })));
            },
        },
        Row {
            name: "this PC's own network gone: the cause blames the PC",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.w.connections.clear();
                r.event(Event::Network);
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(10_250);
            },
            then: |r, _| {
                assert!(matches!(cause(r), Some(Cause::NetworkLost { kind: ConnKind::WiFi, .. })));
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))));
            },
        },
        Row {
            name: "a VPN that came up just before is named",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.w.connections.push(Connection { net: NetId("vpn".into()), name: Some("Corp VPN".into()), kind: ConnKind::Other });
                r.event(Event::Network);
                r.wait(5_000);
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(10_250);
            },
            then: |r, _| match cause(r) {
                Some(Cause::StoppedResponding { vpn: Some(v), .. }) => assert_eq!(v.name.as_deref(), Some("Corp VPN")),
                other => panic!("{other:?}"),
            },
        },
        Row {
            name: "sound that no player plays moves the same way",
            given: || {
                let mut r = Rig::on_kitchen();
                r.sound(GAME);
                r.wait(4_000);
                r.speaker(K, SpeakerSignal::NotResponding);
                r
            },
            when: |r| r.wait(10_250),
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert_eq!(r.sit().spot, this_pc());
                assert!(matches!(popup(r), Some(Popup::Note(Note { cause: Cause::StoppedResponding { .. }, .. }))));
            },
        },
        Row {
            name: "removing the place's speaker: this PC, a note without a button, nothing by the clock",
            given: Rig::playing_on_kitchen,
            when: |r| {
                r.cmd(Command::RemoveSpeaker(id(K))).unwrap();
            },
            then: |r, dos| {
                assert!(silenced(dos, K) && to_speakers(dos));
                assert!(asks_no_player(dos));
                assert_eq!(r.sit().spot, this_pc());
                assert!(matches!(cause(r), Some(Cause::Removed { .. })));
                assert_eq!((button(r), popup(r)), (None, None), "the person is in the app's window");
            },
        },
    ]);
}

#[test]
fn tries_at_one_three_and_a_half_and_eight_and_a_half_seconds() {
    let mut r = dropped();
    r.take();
    let mut at = Vec::new();
    let start = r.now.mono_ms;
    for _ in 0..(11_000 / 250) {
        r.wait(250);
        for d in r.take() {
            if matches!(&d, Do::Connect(j) if j.purpose == Purpose::Retry) {
                at.push(r.now.mono_ms - start);
                // Each try fails at once.
                r.end(Purpose::Retry, Ended::NotResponding);
            }
        }
    }
    assert_eq!(at, vec![1_000, 3_500, 8_500]);
    assert_eq!(r.sit().spot, this_pc(), "moved at 10 s");
}

#[test]
fn a_stalled_try_counts_as_failed_when_the_next_is_due() {
    let mut r = dropped();
    r.wait(1_000);
    let first = r.job(Purpose::Retry);
    r.take();
    r.wait(2_500);
    let dos = r.take();
    assert!(dos.contains(&Do::Supersede(first.gen)), "the first try gives way to the second");
    assert_ne!(r.job(Purpose::Retry).gen, first.gen);
    // The last one stalls too: the move comes at 10 s anyway.
    r.wait(6_250);
    let last = r.job(Purpose::Retry);
    r.take();
    r.wait(500);
    let dos = r.take();
    assert_eq!(r.sit().spot, this_pc());
    assert!(!r.has_job(Purpose::Retry));
    // The connection that stopped answering is let go, then the try on its way.
    let let_go = dos.iter().position(|d| *d == Do::StopSending { speaker: id(K), take_off: false });
    let given_up = dos.iter().position(|d| *d == Do::Supersede(last.gen));
    assert!(let_go.is_some() && let_go < given_up, "{dos:?}");
    assert_eq!(dos.iter().filter(|d| matches!(d, Do::Supersede(_))).count(), 1, "{dos:?}");
}

#[test]
fn tries_that_end_while_nothing_plays_any_more_leave_the_speaker_the_place() {
    let mut r = dropped();
    r.wait(2_000);
    r.pause_by_hand(SPOTIFY);
    r.take();
    r.wait(9_000);
    let dos = r.take();
    assert!(!to_speakers(&dos), "there is no sound to move: {dos:?}");
    assert!(dos.contains(&Do::StopSending { speaker: id(K), take_off: false }), "the connection is let go: {dos:?}");
    let s = r.sit();
    assert_eq!((s.spot.clone(), s.doing, s.away), (Spot::Speaker(id(K)), Doing::NothingPlaying, Some(Away::NotResponding)));
    assert_eq!((popup(&r), s.amber), (None, false));
    let n = s.note.clone().expect("the note says what happens when sound starts");
    assert_eq!((n.ahead, n.button()), (Some(Ahead::WhenSoundStarts), Some(NoteButton::SwitchToThisPc)));
    assert!(!r.has_job(Purpose::Retry));
    // What starts then plays on this PC, the answer being known.
    r.play(SPOTIFY);
    let dos = r.take();
    assert!(to_speakers(&dos) && !connects(&dos, Purpose::SoundStarted), "{dos:?}");
    assert!(matches!(cause(&r), Some(Cause::NotRespondingAtStart { why: Away::NotResponding, .. })));
}

#[test]
fn one_try_when_sound_starts() {
    run(&[
        Row {
            name: "it looks first, and the place's row shows that it connects",
            given: rested,
            when: |r| r.play(SPOTIFY),
            then: |r, dos| {
                assert!(asks_no_player(dos), "the player isn't held");
                let j = r.job(Purpose::SoundStarted);
                assert!(j.look_first);
                assert_eq!(r.sit().doing, Doing::Connecting);
                assert_eq!(row(r, K), RowState::Connecting);
            },
        },
        Row {
            name: "the look finds it busy: this PC, and the note has no time",
            given: starting,
            when: |r| r.end(Purpose::SoundStarted, Ended::Busy { who: None }),
            then: |r, dos| {
                assert!(to_speakers(dos));
                let want = Note { cause: Cause::PlayingElse { speaker: id(K), who: None }, at: None, available_again: false, ahead: None };
                assert_eq!(note(r), Some(want.clone()));
                assert_eq!(popup(r), Some(Popup::Note(want)));
                assert_eq!(button(r), Some(NoteButton::SwitchTo(id(K))));
                assert_eq!(r.sit().spot, this_pc());
                assert_eq!(row(r, K), RowState::PlayingElse);
            },
        },
        Row {
            name: "it asks for its code: this PC, and the note's button enters it",
            given: starting,
            when: |r| r.end(Purpose::SoundStarted, Ended::NeedsSecret(Secret::Code)),
            then: |r, _| {
                assert_eq!(r.sit().spot, this_pc());
                assert_eq!(button(r), Some(NoteButton::EnterSecret(id(K), Secret::Code)));
            },
        },
        Row {
            name: "it isn't on this network: this PC, and its row says so",
            given: starting,
            when: |r| r.end(Purpose::SoundStarted, Ended::NotFound),
            then: |r, _| {
                assert!(matches!(cause(r), Some(Cause::NotRespondingAtStart { why: Away::NotFound, .. })));
                assert_eq!(row(r, K), RowState::NotFound);
            },
        },
        Row {
            name: "ready: the note about an earlier failed start has nothing left to say",
            given: || {
                // Only where a failed start leaves the speaker the place.
                let mut r = rested();
                r.core.choose(AfterHeadphones::StayOnThisPc, AfterFailedStart::Speaker);
                r.play(SPOTIFY);
                r.wait(1_000);
                r.end(Purpose::SoundStarted, Ended::NotResponding);
                r.pause_by_hand(SPOTIFY);
                r.wait(6_000);
                r.speaker(K, SpeakerSignal::Responding);
                r.play(SPOTIFY);
                r
            },
            when: |r| r.end(Purpose::SoundStarted, Ended::Ready),
            then: |r, _| assert_eq!((r.sit().spot, r.sit().note), (Spot::Speaker(id(K)), None)),
        },
    ]);
}

#[test]
fn a_failed_start_can_leave_the_speaker_the_place() {
    // The other value of the fixed choice.
    let mut r = rested();
    r.core.choose(AfterHeadphones::StayOnThisPc, AfterFailedStart::Speaker);
    r.play(SPOTIFY);
    r.wait(1_000);
    r.take();
    r.end(Purpose::SoundStarted, Ended::NotResponding);
    let dos = r.take();
    assert!(dos.contains(&Do::PassThrough(Some(out("speakers")))), "what plays is heard on this PC: {dos:?}");
    assert!(!to_speakers(&dos), "Windows stays on the app's device: {dos:?}");
    assert!(asks_no_player(&dos));
    r.wait(500);
    let s = r.sit();
    assert_eq!((s.spot.clone(), s.doing, s.away), (Spot::Speaker(id(K)), Doing::Playing, Some(Away::NotResponding)));
    assert!(matches!(cause(&r), Some(Cause::NotRespondingAtStart { .. })));
    assert!(matches!(popup(&r), Some(Popup::Note(_))));
    // Someone else playing there, or a code it asks for, moves the sound
    // all the same: waiting doesn't make it play.
    let mut r = rested();
    r.core.choose(AfterHeadphones::StayOnThisPc, AfterFailedStart::Speaker);
    r.play(SPOTIFY);
    r.end(Purpose::SoundStarted, Ended::Busy { who: None });
    assert_eq!(r.sit().spot, this_pc());

    // When that sound has ended, the next start tries the speaker again.
    let mut r = rested();
    r.core.choose(AfterHeadphones::StayOnThisPc, AfterFailedStart::Speaker);
    r.play(SPOTIFY);
    r.wait(1_000);
    r.end(Purpose::SoundStarted, Ended::NotResponding);
    r.pause_by_hand(SPOTIFY);
    r.wait(6_000);
    assert!(r.take().contains(&Do::PassThrough(None)));
    r.speaker(K, SpeakerSignal::Responding);
    r.play(SPOTIFY);
    assert!(connects(&r.take(), Purpose::SoundStarted));
}

#[test]
fn a_speaker_that_asks_for_its_code_is_never_available_again_by_itself() {
    let mut r = Rig::playing_on_kitchen();
    r.speaker(K, SpeakerSignal::AsksSecret(Secret::Code));
    let dos = r.take();
    assert!(!connects(&dos, Purpose::Retry), "no tries");
    r.speaker(K, SpeakerSignal::Responding);
    r.wait_long(10 * 60_000);
    let n = note(&r).expect("the note stands");
    assert!(!n.available_again && n.failure());
    assert_eq!(n.button(), Some(NoteButton::EnterSecret(id(K), Secret::Code)));
}

#[test]
fn the_speakers_own_pause_and_play_are_passed_on() {
    run(&[
        Row {
            name: "a stop pressed on the speaker is passed on as a pause",
            given: Rig::playing_on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::StoppedAtSpeaker),
            then: |r, dos| {
                assert!(paused(dos, SPOTIFY));
                assert_eq!((r.sit().spot, r.sit().note, popup(r)), (Spot::Speaker(id(K)), None, None));
            },
        },
        Row {
            name: "a speaker that shows what it does and ends the stream while it can be reached: a stop, no tries",
            given: Rig::playing_on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::EndedStream { reachable: true }),
            then: |r, dos| {
                assert!(!connects(dos, Purpose::Retry));
                assert!(paused(dos, SPOTIFY));
                assert_eq!((r.sit().spot, r.sit().note), (Spot::Speaker(id(K)), None));
            },
        },
        Row {
            name: "only the players that were playing there get the pause",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.play(CHROME);
                r.wait(4_000);
                r.pause_by_hand(CHROME);
                r.wait(6_000);
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::PausedAtSpeaker),
            then: |_, dos| assert!(paused(dos, SPOTIFY) && !paused(dos, CHROME)),
        },
        Row {
            name: "nothing of this PC's plays there: nothing is passed on",
            given: Rig::on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::PausedAtSpeaker),
            then: |r, dos| {
                assert!(asks_no_player(dos));
                assert!(r.sit().standing.is_empty());
            },
        },
        Row {
            name: "sound that no player's pause stops goes on unheard, and the situation says so while it lasts",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.sound(GAME);
                r.wait(4_000);
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::PausedAtSpeaker),
            then: |r, dos| {
                assert!(paused(dos, SPOTIFY));
                assert_eq!(r.sit().standing, vec![Standing::PausedAtSpeaker { speaker: id(K) }]);
            },
        },
        Row {
            name: "… until that sound ends",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.sound(GAME);
                r.wait(4_000);
                r.speaker(K, SpeakerSignal::PausedAtSpeaker);
                r
            },
            when: |r| {
                r.silence(GAME);
                r.wait(6_000);
            },
            then: |r, _| assert!(r.sit().standing.is_empty()),
        },
        Row {
            name: "play pressed on the speaker starts the players its pause stopped only where that is counted on",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::PausedAtSpeaker);
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::PlayPressed),
            then: |_, dos| assert_eq!(started(dos, SPOTIFY), trials::T3_PLAY_ON_SPEAKER_STARTS_PLAYER),
        },
        Row {
            name: "play pressed on the speaker never starts a player a person paused",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.pause_by_hand(SPOTIFY);
                r.wait(6_000);
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::PlayPressed),
            then: |_, dos| assert!(asks_no_player(dos)),
        },
    ]);
}

#[test]
fn someone_else_playing_on_the_speaker() {
    /// Kitchen is the place, nothing plays, and someone else plays on it.
    fn busy_while_quiet() -> Rig {
        let mut r = Rig::on_kitchen();
        r.speaker(K, SpeakerSignal::PlayingElse { who: None });
        r
    }
    run(&[
        Row {
            name: "the other PC is named where the speaker says it, and its sound isn't stopped",
            given: Rig::playing_on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::PlayingElse { who: Some("LAPTOP-7Q2K".into()) }),
            then: |r, dos| {
                assert!(!silenced(dos, K), "their sound isn't stopped");
                assert!(dos.contains(&Do::StopSending { speaker: id(K), take_off: false }));
                assert!(matches!(cause(r), Some(Cause::PlayingElse { who: Some(w), .. }) if w == "LAPTOP-7Q2K"));
                assert!(r.sit().amber);
            },
        },
        Row {
            name: "while nothing plays the speaker stays the place, and the note says where sound that starts will play",
            given: Rig::on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::PlayingElse { who: None }),
            then: |r, dos| {
                assert!(!to_speakers(dos));
                let s = r.sit();
                assert_eq!((s.spot.clone(), s.away, popup(r), s.amber), (Spot::Speaker(id(K)), None, None, false));
                let want = Note {
                    cause: Cause::PlayingElse { speaker: id(K), who: None },
                    at: None,
                    available_again: false,
                    ahead: Some(Ahead::WhenSoundStarts),
                };
                assert_eq!(s.note, Some(want));
                assert_eq!(button(r), Some(NoteButton::SwitchToThisPc));
                assert_eq!(row(r, K), RowState::PlayingElse);
            },
        },
        Row {
            name: "… sound that starts then plays on this PC without a try, and the note has no time",
            given: busy_while_quiet,
            when: |r| r.play(SPOTIFY),
            then: |r, dos| {
                assert!(to_speakers(dos) && !connects(dos, Purpose::SoundStarted));
                let want = Note { cause: Cause::PlayingElse { speaker: id(K), who: None }, at: None, available_again: false, ahead: None };
                assert_eq!(note(r), Some(want.clone()));
                assert_eq!(popup(r), Some(Popup::Note(want)));
            },
        },
        Row {
            name: "… unless they stopped meanwhile: it plays on the speaker as ever",
            given: || {
                let mut r = busy_while_quiet();
                r.speaker(K, SpeakerSignal::Free);
                r
            },
            when: |r| r.play(SPOTIFY),
            then: |r, dos| {
                assert!(connects(dos, Purpose::SoundStarted));
                assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
            },
        },
        Row {
            name: "a room taken into another group has no way back until it leaves the group",
            given: Rig::playing_on_kitchen,
            when: |r| r.speaker(K, SpeakerSignal::GroupedInto { with: "Living Room".into() }),
            then: |r, _| {
                assert!(matches!(cause(r), Some(Cause::GroupedInto { with, .. }) if with == "Living Room"));
                assert_eq!((r.sit().spot, button(r)), (this_pc(), None));
            },
        },
        Row {
            name: "… then a plain switch",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::GroupedInto { with: "Living Room".into() });
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::LeftGroup),
            then: |r, _| assert_eq!(button(r), Some(NoteButton::SwitchTo(id(K)))),
        },
    ]);
}

#[test]
fn a_speaker_that_cant_show_is_may_be_in_use_after_it_disconnects() {
    let mut r = Rig::new();
    r.switch_to(OFFICE);
    r.play(SPOTIFY);
    r.wait(4_000);
    r.take();
    r.speaker(OFFICE, SpeakerSignal::EndedStream { reachable: true });
    let dos = r.take();
    assert!(!connects(&dos, Purpose::Retry), "no tries");
    assert!(to_speakers(&dos) && asks_no_player(&dos));
    assert!(matches!(cause(&r), Some(Cause::Disconnected { .. })));
    assert_eq!(row(&r, OFFICE), RowState::MayBeInUse);
    // Never available again by itself, also when it answers.
    r.speaker(OFFICE, SpeakerSignal::Responding);
    r.wait_long(60 * 60_000);
    assert_eq!(row(&r, OFFICE), RowState::MayBeInUse, "until a person switches to it, with no clock");
    assert!(!note(&r).unwrap().available_again);
    let kept = r.core.kept();
    assert!(kept.disconnected.contains_key(&id(OFFICE)), "kept across a restart");
    let mut again = Rig::starting_at(r.now.later(5_000), kept, world(), StartKind::Normal, true);
    assert_eq!(row(&again, OFFICE), RowState::MayBeInUse);
    again.open_panel();
    again.cmd(Command::Switch { place: Place::Speaker(id(OFFICE)), way: None }).unwrap();
    assert!(again.sit().confirm_in_panel.is_none(), "the switch takes it without a question");
    again.end(Purpose::Switch, Ended::Ready);
    assert_eq!(row(&again, OFFICE), RowState::Plain);
    assert_eq!(again.sit().note, None);
}

#[test]
fn the_note_follows_whether_the_speaker_can_play_again() {
    run(&[
        Row {
            name: "the app asks the speaker every 15 seconds at first",
            given: moved,
            when: |r| r.wait(16_000),
            then: |_, dos| assert!(dos.contains(&Do::Look(id(K)))),
        },
        Row {
            name: "it answers while the pop-up still shows: the pop-up says the same as the note",
            given: moved,
            when: |r| r.speaker(K, SpeakerSignal::Responding),
            then: |r, dos| {
                assert!(!to_ours(dos), "the app never moves the sound back by itself");
                let n = note(r).expect("the note");
                assert!(n.available_again && !n.failure());
                assert_eq!(popup(r), Some(Popup::Note(n)));
                assert!(!r.sit().amber);
                assert_eq!(row(r, K), RowState::Plain, "the row loses its state name");
                assert_eq!(r.sit().spot, this_pc());
            },
        },
        Row {
            name: "hours later it is the same: the note has no clock",
            given: || {
                let mut r = moved();
                r.pause_by_hand(SPOTIFY);
                r.wait_long(5 * 3_600_000);
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::Responding),
            then: |r, _| {
                assert!(note(r).expect("the note still stands").available_again);
                assert_eq!(popup(r), None, "nothing by the clock");
            },
        },
        Row {
            name: "the speaker fails again: the note goes back, and the app asks again",
            given: || {
                let mut r = moved();
                r.speaker(K, SpeakerSignal::Responding);
                r
            },
            when: |r| {
                r.speaker(K, SpeakerSignal::NotResponding);
                r.wait(16_000);
            },
            then: |r, dos| {
                assert!(!note(r).unwrap().available_again);
                assert_eq!(button(r), Some(NoteButton::TryAgain(id(K))));
                assert!(dos.contains(&Do::Look(id(K))));
            },
        },
        Row {
            name: "someone else stopped playing there",
            given: || {
                let mut r = Rig::playing_on_kitchen();
                r.speaker(K, SpeakerSignal::PlayingElse { who: None });
                r
            },
            when: |r| r.speaker(K, SpeakerSignal::Free),
            then: |r, _| {
                let n = note(r).unwrap();
                assert!(n.available_again && !n.failure());
                assert_eq!(n.button(), Some(NoteButton::SwitchTo(id(K))));
                assert_eq!(row(r, K), RowState::Plain);
            },
        },
    ]);
}

#[test]
fn the_app_asks_a_speaker_that_couldnt_play_for_thirty_minutes() {
    fn asked(dos: &[Do]) -> usize {
        dos.iter().filter(|d| **d == Do::Look(id(K))).count()
    }
    // The sound moved a quarter of a second ago.
    let mut r = moved();
    r.pause_by_hand(SPOTIFY);
    r.take();
    r.wait_long(2 * 60_000 - 250);
    assert_eq!(asked(&r.take()), 8, "every 15 seconds for two minutes");
    r.wait_long(28 * 60_000 - 1_000);
    assert_eq!(asked(&r.take()), 27, "then every minute");
    r.wait_long(60 * 60_000);
    assert_eq!(asked(&r.take()), 0, "after 30 minutes it stops asking");
    // The note stands all the same, and its button still tries.
    assert_eq!(button(&r), Some(NoteButton::TryAgain(id(K))));
}

#[test]
fn a_short_sound_is_played_on_this_pc() {
    run(&[
        Row {
            name: "a sound that ends within 3 seconds doesn't connect the resting speaker",
            given: rested,
            when: |r| {
                r.sound("Discord");
                r.wait(1_000);
                r.silence("Discord");
                r.wait(2_000);
            },
            then: |r, dos| {
                assert!(!connects(dos, Purpose::SoundStarted));
                // From the sample it was first heard in to the one that
                // found it over, and the half second a sample can be late.
                assert!(dos.contains(&Do::ReplayShort { on: out("speakers"), ms: 2_500 }), "{dos:?}");
                assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
            },
        },
        Row {
            name: "a longer sound connects it after 3 seconds",
            given: rested,
            when: |r| {
                r.sound(GAME);
                r.wait(3_500);
            },
            then: |_, dos| {
                assert!(connects(dos, Purpose::SoundStarted));
                assert!(!dos.iter().any(|d| matches!(d, Do::ReplayShort { .. })));
            },
        },
        Row {
            name: "a sound of two and a half seconds is a short one: its end decides, not the third second",
            given: rested,
            when: |r| {
                r.sound("Discord");
                r.wait(2_500);
                r.silence("Discord");
                r.wait(1_500);
            },
            then: |r, dos| {
                assert!(!connects(dos, Purpose::SoundStarted), "{dos:?}");
                assert!(dos.contains(&Do::ReplayShort { on: out("speakers"), ms: 4_000 }), "{dos:?}");
                assert_eq!(r.sit().spot, Spot::Speaker(id(K)));
            },
        },
        Row {
            name: "a sound that goes on after a pause within it connects the speaker",
            given: rested,
            when: |r| {
                r.sound(GAME);
                r.wait(2_750);
                r.silence(GAME);
                r.wait(500);
                r.sound(GAME);
                r.wait(500);
            },
            then: |_, dos| {
                assert!(connects(dos, Purpose::SoundStarted), "{dos:?}");
                assert!(!dos.iter().any(|d| matches!(d, Do::ReplayShort { .. })), "{dos:?}");
            },
        },
        Row {
            name: "a PC with no output of its own connects at once",
            given: || {
                let mut r = rested();
                r.w.outputs.clear();
                r
            },
            when: |r| {
                r.sound("Discord");
                r.wait(250);
            },
            then: |_, dos| assert!(connects(dos, Purpose::SoundStarted)),
        },
        Row {
            name: "a longer sound on a speaker known to be away moves to this PC after 3 seconds",
            given: || {
                let mut r = Rig::on_kitchen();
                r.speaker(K, SpeakerSignal::NotResponding);
                r
            },
            when: |r| {
                r.sound(GAME);
                r.wait(3_500);
            },
            then: |r, dos| {
                assert!(to_speakers(dos));
                assert_eq!(r.sit().spot, this_pc());
                assert!(matches!(cause(r), Some(Cause::NotRespondingAtStart { .. })));
            },
        },
    ]);
}

#[test]
fn the_option_switches_back_after_thirty_minutes() {
    /// The option is on and has moved the sound from Kitchen.
    fn switched_back() -> Rig {
        let mut r = Rig::on_kitchen();
        r.w.settings.switch_back_after_quiet = true;
        r.wait_long(31 * 60_000);
        r
    }
    run(&[
        Row {
            name: "off by default: the place stays",
            given: Rig::on_kitchen,
            when: |r| r.wait_long(31 * 60_000),
            then: |r, _| assert_eq!(r.sit().spot, Spot::Speaker(id(K))),
        },
        Row {
            name: "on: this PC after 30 minutes without sound, with a note and nothing by the clock",
            given: || {
                let mut r = Rig::on_kitchen();
                r.w.settings.switch_back_after_quiet = true;
                r
            },
            when: |r| r.wait_long(31 * 60_000),
            then: |r, dos| {
                assert!(to_speakers(dos));
                let s = r.sit();
                assert_eq!(s.spot, this_pc());
                let n = s.note.clone().expect("the note");
                assert_eq!(n.cause, Cause::SwitchedAfterQuiet { from: id(K) });
                assert!(!n.failure() && !s.amber);
                assert_eq!(n.button(), Some(NoteButton::SwitchTo(id(K))));
                assert_eq!(popup(r), None, "nothing announces it");
            },
        },
        Row {
            name: "what plays on this PC hours later brings nothing by the clock: the note says it",
            given: switched_back,
            when: |r| {
                r.wait_long(5 * 3_600_000);
                r.play(SPOTIFY);
                r.wait(4_000);
            },
            then: |r, _| {
                assert_eq!(popup(r), None);
                assert!(matches!(cause(r), Some(Cause::SwitchedAfterQuiet { .. })), "there is no clock on the note");
            },
        },
    ]);
}
