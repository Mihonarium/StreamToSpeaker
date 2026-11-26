//! The moments of the behaviour, one test each: what the app does, what
//! line 1 and line 2 are made from, the note in the panel, and what shows
//! by the clock.

use super::rig::*;

fn switch(s: &str) -> Command {
    Command::Switch { place: Place::Speaker(id(s)), way: None }
}

fn this_pc() -> Spot {
    Spot::ThisPc(Some(out("speakers")))
}

/// Kitchen is the place and its stream has rested: nothing played for half
/// an hour.
fn kitchen_rested() -> Rig {
    let mut rig = Rig::on_kitchen();
    rig.wait_long(31 * 60_000);
    assert!(rig.take().contains(&Do::StopSending { speaker: id(K), take_off: true }), "set-up: the stream rests");
    rig
}

/// This PC is the place and Spotify plays on its speakers.
fn playing_on_this_pc() -> Rig {
    let mut rig = Rig::new();
    rig.play(SPOTIFY);
    rig.wait(4_000);
    assert_eq!((rig.sit().spot, rig.sit().doing), (this_pc(), Doing::Playing), "set-up");
    rig.take();
    rig
}

/// This PC joins the office's network, where Kitchen isn't.
fn to_the_office(rig: &mut Rig) {
    rig.w.connections = vec![Connection { net: NetId("office".into()), name: Some("OfficeNet".into()), kind: ConnKind::WiFi }];
    rig.w.speakers[0].found = false;
    rig.event(Event::Network);
}

fn back_home(rig: &mut Rig) {
    rig.w.connections = vec![Connection { net: home(), name: Some("HomeNet".into()), kind: ConnKind::WiFi }];
    rig.w.speakers[0].found = true;
    rig.event(Event::Network);
}

#[test]
fn sound_plays_on_the_speaker() {
    let mut rig = Rig::playing_on_kitchen();
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (Spot::Speaker(id(K)), Doing::Playing));
    assert_eq!((s.note.clone(), popup(&rig)), (None, None), "soon after the switch nothing needs saying");

    // After a break of ten minutes or more in which nothing played and
    // nobody switched, the speaker that plays says so by the clock.
    rig.pause_by_hand(SPOTIFY);
    rig.wait_long(11 * 60_000);
    rig.take();
    rig.play(SPOTIFY);
    rig.wait(4_000);
    assert!(asks_no_player(&rig.take()));
    assert_eq!(popup(&rig), Some(Popup::PlayingOn { speaker: id(K) }));
    assert_eq!(rig.sit().note, None);
}

#[test]
fn nothing_plays() {
    let mut rig = Rig::playing_on_kitchen();
    rig.pause_by_hand(SPOTIFY);
    rig.wait_long(29 * 60_000);
    let dos = rig.take();
    assert!(!dos.iter().any(|d| matches!(d, Do::StopSending { .. })), "the stream stays for 30 minutes: {dos:?}");
    rig.wait_long(2 * 60_000);
    let dos = rig.take();
    assert!(dos.contains(&Do::StopSending { speaker: id(K), take_off: true }), "then it is taken off the speaker: {dos:?}");
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.away), (Spot::Speaker(id(K)), Doing::NothingPlaying, None));
    assert_eq!((s.note.clone(), popup(&rig)), (None, None));
}

#[test]
fn sound_starts_and_the_speaker_has_to_be_connected_first() {
    let mut rig = kitchen_rested();
    rig.play(SPOTIFY);
    let dos = rig.take();
    assert!(connects(&dos, Purpose::SoundStarted), "one try: {dos:?}");
    assert!(asks_no_player(&dos), "nothing is held while it connects");
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (Spot::Speaker(id(K)), Doing::Connecting));
    assert_eq!(row(&rig, K), RowState::Connecting);
    assert_eq!((s.note.clone(), popup(&rig), s.amber), (None, None, false));

    rig.end(Purpose::SoundStarted, Ended::Ready);
    rig.wait(500);
    assert_eq!((rig.sit().spot, rig.sit().doing), (Spot::Speaker(id(K)), Doing::Playing));
    assert!(asks_no_player(&rig.take()));

    // Two seconds after it was asked Kitchen hasn't answered anything: its
    // row and the note say so, the note says that the sound is about to
    // move, and offers This PC at once.
    let mut rig = kitchen_rested();
    rig.play(SPOTIFY);
    assert_eq!(rig.core.next_due(), Some(rig.now.mono_ms + 2_000), "the app wakes when the note is due");
    rig.wait(1_750);
    assert_eq!((rig.sit().doing, rig.sit().note, row(&rig, K)), (Doing::Connecting, None, RowState::Connecting), "not yet");
    rig.wait(250);
    let s = rig.sit();
    let want = Note {
        cause: Cause::NotRespondingAtStart { speaker: id(K), why: Away::NotResponding },
        at: None,
        available_again: false,
        ahead: Some(Ahead::InAFewSeconds),
    };
    assert_eq!((s.spot.clone(), s.doing, s.amber), (Spot::Speaker(id(K)), Doing::Connecting, true));
    assert_eq!(s.note, Some(want.clone()));
    assert_eq!(row(&rig, K), RowState::NotResponding);
    assert_eq!(button(&rig), Some(NoteButton::SwitchToThisPc));
    assert!(!want.can_close(), "as during the tries, it has no ×");
    assert_eq!(popup(&rig), None, "nothing by the clock");
    // It answers: it is getting ready, and nothing is wrong with it.
    rig.speaker(K, SpeakerSignal::Responding);
    assert_eq!((rig.sit().note, rig.sit().amber, row(&rig, K)), (None, false, RowState::Connecting));
    // A speaker that takes its time is waited for: six seconds after the
    // sound started it is ready, and plays.
    rig.wait(4_000);
    assert_eq!((rig.sit().spot, rig.sit().doing, rig.sit().note), (Spot::Speaker(id(K)), Doing::Connecting, None));
    rig.end(Purpose::SoundStarted, Ended::Ready);
    rig.wait(500);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.note.clone(), s.amber), (Spot::Speaker(id(K)), Doing::Playing, None, false));
    let dos = rig.take();
    assert!(!to_speakers(&dos) && asks_no_player(&dos), "nothing moved: {dos:?}");

    // Just after a wake the try waits for this PC's network, and the note
    // says that instead, two seconds after the sound started.
    let mut rig = Rig::playing_on_kitchen();
    rig.pause_by_hand(SPOTIFY);
    rig.event(Event::Power(PowerEvent::Suspending));
    rig.now = rig.now.later(3_600_000);
    rig.w.connections.clear();
    rig.event(Event::Power(PowerEvent::Resumed));
    rig.event(Event::Network);
    rig.play(SPOTIFY);
    rig.wait(1_750);
    assert_eq!((rig.sit().doing, rig.sit().note), (Doing::Connecting, None), "not yet");
    rig.wait(250);
    let want = Note {
        cause: Cause::NotRespondingAtStart { speaker: id(K), why: Away::NoNetwork },
        at: None,
        available_again: false,
        ahead: Some(Ahead::InAFewSeconds),
    };
    assert_eq!((rig.sit().note, rig.sit().amber, row(&rig, K)), (Some(want), true, RowState::Connecting));
    assert_eq!(button(&rig), Some(NoteButton::SwitchToThisPc));
    // The network comes: Kitchen is asked now, and nothing is said about
    // it before it has had its two seconds.
    rig.w.connections = world().connections;
    rig.event(Event::Network);
    assert!(rig.has_job(Purpose::SoundStarted));
    assert_eq!((rig.sit().note, rig.sit().amber, row(&rig, K)), (None, false, RowState::Connecting));
    rig.end(Purpose::SoundStarted, Ended::Ready);
    assert_eq!((rig.sit().note, rig.sit().amber, row(&rig, K)), (None, false, RowState::Plain));
}

#[test]
fn sound_starts_and_the_speaker_doesnt_respond() {
    let mut rig = kitchen_rested();
    rig.play(SPOTIFY);
    rig.wait(1_000);
    rig.take();
    let at = rig.now.local;
    rig.end(Purpose::SoundStarted, Ended::NotResponding);
    let dos = rig.take();
    assert!(to_speakers(&dos), "the sound goes to this PC's own output at once: {dos:?}");
    assert!(asks_no_player(&dos), "and carries on there");
    rig.wait(500);
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing));
    let want = Note {
        cause: Cause::NotRespondingAtStart { speaker: id(K), why: Away::NotResponding },
        at: Some(at),
        available_again: false,
        ahead: None,
    };
    assert_eq!(s.note, Some(want.clone()));
    assert!(want.failure());
    assert_eq!(button(&rig), Some(NoteButton::TryAgain(id(K))));
    assert_eq!(popup(&rig), Some(Popup::Note(want)), "the same by the clock");

    // The try has its limit: a speaker that isn't ready about ten seconds
    // after it was asked is given up, and the sound moves.
    let mut rig = kitchen_rested();
    rig.play(SPOTIFY);
    rig.wait(9_750);
    assert_eq!((rig.sit().spot, rig.sit().doing), (Spot::Speaker(id(K)), Doing::Connecting));
    rig.wait(500);
    assert_eq!(rig.sit().spot, this_pc());
    assert!(!rig.has_job(Purpose::SoundStarted), "the try is no longer wanted");
    // Also one that answered and never got ready; until then nothing is
    // said against it.
    let mut rig = kitchen_rested();
    rig.play(SPOTIFY);
    rig.wait(1_000);
    rig.speaker(K, SpeakerSignal::Responding);
    rig.wait(8_750);
    assert_eq!((rig.sit().spot, rig.sit().note), (Spot::Speaker(id(K)), None), "it is getting ready");
    rig.wait(500);
    assert_eq!(rig.sit().spot, this_pc());
    assert!(matches!(cause(&rig), Some(Cause::NotRespondingAtStart { why: Away::NotResponding, .. })));
}

#[test]
fn the_speaker_stops_responding_during_play() {
    let mut rig = Rig::playing_on_kitchen();
    let at = rig.now.local;
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.wait(9_500);
    let dos = rig.take();
    let tries = dos.iter().filter(|d| matches!(d, Do::Connect(j) if j.purpose == Purpose::Retry)).count();
    assert_eq!(tries, 3, "it tries again for about 10 seconds: {dos:?}");
    assert!(asks_no_player(&dos), "nothing is paused meanwhile");
    assert!(!to_speakers(&dos), "the sound isn't moved yet");
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.amber), (Spot::Speaker(id(K)), Doing::Trying, true));
    assert_eq!(row(&rig, K), RowState::NotResponding);
    // The note says what is about to happen, and its button doesn't wait
    // for it.
    let want = Note {
        cause: Cause::StoppedResponding { speaker: id(K), vpn: None },
        at: Some(at),
        available_again: false,
        ahead: Some(Ahead::InAFewSeconds),
    };
    assert_eq!(s.note, Some(want.clone()));
    assert_eq!(button(&rig), Some(NoteButton::SwitchToThisPc));
    assert_eq!(popup(&rig), None, "nothing by the clock");
    // It has no ×, so that closing it can't be taken for stopping the
    // move; a close that arrives all the same changes nothing.
    assert!(!want.can_close());
    rig.cmd(Command::CloseNote).unwrap();
    assert_eq!((rig.sit().note, rig.sit().doing), (Some(want), Doing::Trying));
    assert!(rig.has_job(Purpose::Retry) || rig.core.next_due().is_some(), "the tries go on");

    rig.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
    let dos = rig.take();
    assert!(to_speakers(&dos) && asks_no_player(&dos), "{dos:?}");
    assert!(!rig.has_job(Purpose::Retry), "the tries are over");
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.note.clone(), s.amber), (this_pc(), None, false), "a person's switch needs no note");
}

#[test]
fn the_speaker_responds_within_the_tries() {
    let mut rig = Rig::playing_on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.wait(4_000);
    rig.end(Purpose::Retry, Ended::Ready);
    // Past the moment the tries would have ended.
    rig.wait(7_000);
    let dos = rig.take();
    assert!(!to_speakers(&dos) && asks_no_player(&dos), "it plays on: {dos:?}");
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.amber), (Spot::Speaker(id(K)), Doing::Playing, false));
    assert_eq!((s.note.clone(), popup(&rig)), (None, None));
    assert_eq!(row(&rig, K), RowState::Plain);
}

#[test]
fn the_speaker_doesnt_respond_within_the_tries() {
    let mut rig = Rig::playing_on_kitchen();
    let at = rig.now.local;
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.wait(9_750);
    assert_eq!(rig.sit().spot, Spot::Speaker(id(K)), "about ten seconds of tries");
    rig.take();
    rig.wait(500);
    let dos = rig.take();
    assert!(to_speakers(&dos), "then the sound goes to this PC's own output: {dos:?}");
    assert!(asks_no_player(&dos), "and carries on there");
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.amber), (this_pc(), Doing::Playing, true));
    let want =
        Note { cause: Cause::StoppedResponding { speaker: id(K), vpn: None }, at: Some(at), available_again: false, ahead: None };
    assert_eq!(s.note, Some(want.clone()), "the time is when it stopped responding");
    assert_eq!(button(&rig), Some(NoteButton::TryAgain(id(K))));
    assert_eq!(popup(&rig), Some(Popup::Note(want)));
    assert_eq!(row(&rig, K), RowState::NotResponding);
}

#[test]
fn the_speaker_can_play_again() {
    let mut rig = Rig::playing_on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.wait(30_000);
    assert_eq!(popup(&rig), None, "set-up: the pop-up of the move has gone");
    rig.take();

    rig.speaker(K, SpeakerSignal::Responding);
    let dos = rig.take();
    assert!(!to_ours(&dos) && !dos.iter().any(|d| matches!(d, Do::Connect(_))), "the app does nothing: {dos:?}");
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.amber), (this_pc(), Doing::Playing, false));
    let note = s.note.clone().expect("the same note");
    assert!(matches!(note.cause, Cause::StoppedResponding { .. }) && note.available_again, "{note:?}");
    assert!(!note.failure());
    assert_eq!(note.button(), Some(NoteButton::SwitchTo(id(K))));
    assert_eq!(popup(&rig), None, "nothing by the clock");
    assert_eq!(row(&rig, K), RowState::Plain);

    // The way back is the person's: a switch like any other.
    rig.cmd(switch(K)).unwrap();
    assert_eq!(rig.sit().spot, this_pc(), "until Kitchen is ready the sound stays");
    rig.end(Purpose::Switch, Ended::Ready);
    assert_eq!((rig.sit().spot, rig.sit().note), (Spot::Speaker(id(K)), None));
}

#[test]
fn someone_else_starts_playing_on_the_speaker() {
    let mut rig = Rig::playing_on_kitchen();
    let at = rig.now.local;
    rig.speaker(K, SpeakerSignal::PlayingElse { who: None });
    let dos = rig.take();
    assert!(to_speakers(&dos) && !connects(&dos, Purpose::Retry), "at once, without tries: {dos:?}");
    assert!(asks_no_player(&dos));
    rig.wait(500);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing));
    let want = Note { cause: Cause::PlayingElse { speaker: id(K), who: None }, at: Some(at), available_again: false, ahead: None };
    assert_eq!(s.note, Some(want.clone()));
    assert!(want.failure());
    assert_eq!(button(&rig), Some(NoteButton::SwitchTo(id(K))));
    assert_eq!(popup(&rig), Some(Popup::Note(want)));
    assert_eq!(row(&rig, K), RowState::PlayingElse);

    // Its button switches without a question: the note said what the
    // switch stops.
    rig.open_panel();
    rig.cmd(switch(K)).unwrap();
    let s = rig.sit();
    assert_eq!((s.confirm_in_panel.clone(), s.popups.confirmation.clone()), (None, None));
    assert!(!rig.job(Purpose::Switch).look_first, "it doesn't look first: it takes the speaker");
    assert!(s.note.is_some(), "the sound and the note stay until Kitchen is ready");
    rig.end(Purpose::Switch, Ended::Ready);
    assert_eq!((rig.sit().spot, rig.sit().note), (Spot::Speaker(id(K)), None));
    assert_eq!(row(&rig, K), RowState::Plain);
}

#[test]
fn the_speaker_asks_for_its_code_again() {
    let mut rig = Rig::playing_on_kitchen();
    let at = rig.now.local;
    rig.speaker(K, SpeakerSignal::AsksSecret(Secret::Code));
    let dos = rig.take();
    assert!(to_speakers(&dos) && asks_no_player(&dos), "{dos:?}");
    rig.wait(500);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing));
    let want =
        Note { cause: Cause::NeedsSecret { speaker: id(K), secret: Secret::Code }, at: Some(at), available_again: false, ahead: None };
    assert_eq!(s.note, Some(want.clone()));
    assert_eq!(button(&rig), Some(NoteButton::EnterSecret(id(K), Secret::Code)));
    assert_eq!(popup(&rig), Some(Popup::Note(want)), "the same by the clock, with the same button");
}

#[test]
fn this_pc_is_on_another_network_when_sound_starts() {
    let mut rig = kitchen_rested();
    to_the_office(&mut rig);
    rig.wait(61_000);
    rig.take();
    rig.play(SPOTIFY);
    rig.wait(4_000);
    let dos = rig.take();
    assert!(!dos.iter().any(|d| matches!(d, Do::Connect(_))), "the speaker can't be meant here: {dos:?}");
    assert!(asks_no_player(&dos));
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.amber), (this_pc(), Doing::Playing, false));
    assert_eq!((s.note.clone(), popup(&rig)), (None, None), "and nothing is said");

    // At home again the note says that Kitchen can play; the sound stays.
    back_home(&mut rig);
    let s = rig.sit();
    let note = s.note.clone().expect("a note at home");
    assert_eq!((note.cause.clone(), note.available_again), (Cause::OtherNetwork { speaker: id(K) }, true));
    assert!(!note.failure());
    assert_eq!(note.button(), Some(NoteButton::SwitchTo(id(K))));
    assert_eq!(popup(&rig), None);
    assert_eq!(s.spot, this_pc());

    // Sound that starts before the app can know where it is gets the short
    // try. Once the minute is over, the note takes the truer cause and
    // says nothing either.
    let mut rig = kitchen_rested();
    to_the_office(&mut rig);
    rig.play(SPOTIFY);
    rig.end(Purpose::SoundStarted, Ended::NotFound);
    assert!(matches!(cause(&rig), Some(Cause::NotRespondingAtStart { why: Away::NotFound, .. })));
    rig.wait(61_000);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.note.clone(), popup(&rig), s.amber), (this_pc(), None, None, false));
    back_home(&mut rig);
    assert_eq!(cause(&rig), Some(Cause::OtherNetwork { speaker: id(K) }));
}

#[test]
fn the_places_speaker_goes_away_while_nothing_plays() {
    let mut rig = Rig::on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    let dos = rig.take();
    assert!(!to_speakers(&dos) && !connects(&dos, Purpose::Retry), "nothing yet: {dos:?}");
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.away), (Spot::Speaker(id(K)), Doing::NothingPlaying, Some(Away::NotResponding)));
    assert_eq!(row(&rig, K), RowState::NotResponding);
    // The note says what happens when sound starts, and offers This PC.
    let ahead = Note {
        cause: Cause::NotRespondingAtStart { speaker: id(K), why: Away::NotResponding },
        at: None,
        available_again: false,
        ahead: Some(Ahead::WhenSoundStarts),
    };
    assert_eq!(s.note, Some(ahead.clone()));
    assert_eq!(button(&rig), Some(NoteButton::SwitchToThisPc));
    assert!(ahead.can_close(), "this one can stand for hours: it has a ×");
    assert_eq!((popup(&rig), s.amber), (None, false), "nothing was playing: nothing by the clock");

    // A short sound is heard on this PC at once and moves nothing.
    rig.sound("Ding");
    rig.wait(750);
    rig.silence("Ding");
    rig.wait(3_000);
    let dos = rig.take();
    assert!(dos.contains(&Do::PassThrough(Some(out("speakers")))), "{dos:?}");
    assert!(!to_speakers(&dos) && !dos.iter().any(|d| matches!(d, Do::ReplayShort { .. })), "{dos:?}");
    assert_eq!((rig.sit().spot, rig.sit().note), (Spot::Speaker(id(K)), Some(ahead)), "Kitchen stays the place");

    // Sound that starts now plays on this PC, without a try: the answer is
    // known. From then on This PC is the place.
    rig.play(SPOTIFY);
    let at = rig.now.local;
    let dos = rig.take();
    assert!(to_speakers(&dos) && !dos.iter().any(|d| matches!(d, Do::Connect(_))), "{dos:?}");
    assert!(asks_no_player(&dos));
    rig.wait(500);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing));
    let want = Note {
        cause: Cause::NotRespondingAtStart { speaker: id(K), why: Away::NotResponding },
        at: Some(at),
        available_again: false,
        ahead: None,
    };
    assert_eq!(s.note, Some(want.clone()));
    assert_eq!(popup(&rig), Some(Popup::Note(want)));

    // It comes back while still nothing played: the place never left it,
    // and the note has nothing left to say.
    let mut rig = Rig::on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.speaker(K, SpeakerSignal::Responding);
    assert_eq!((rig.sit().spot, rig.sit().away, rig.sit().note), (Spot::Speaker(id(K)), None, None));
    rig.play(SPOTIFY);
    assert!(connects(&rig.take(), Purpose::SoundStarted), "and the next sound plays there");

    // The note's button: This PC is the place, as a click on its row makes
    // it, and nothing more needs saying.
    let mut rig = Rig::on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
    assert_eq!((rig.sit().spot, rig.sit().note), (this_pc(), None));
}

#[test]
fn a_person_clicks_a_speakers_row() {
    let mut rig = playing_on_this_pc();
    rig.cmd(switch(K)).unwrap();
    let dos = rig.take();
    assert!(connects(&dos, Purpose::Switch), "{dos:?}");
    assert!(!to_ours(&dos), "the sound stays where it is until Kitchen is ready: {dos:?}");
    assert!(asks_no_player(&dos));
    rig.wait(1_000);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing), "line 1 doesn't change");
    assert_eq!(row(&rig, K), RowState::Connecting);
    assert_eq!(s.picking.as_ref().map(|p| (p.speaker.clone(), p.can_cancel)), Some((id(K), true)), "with Cancel");
    assert_eq!((s.note.clone(), popup(&rig)), (None, None));

    // A newer switch takes the place of one that connects.
    rig.cmd(switch(B)).unwrap();
    assert!(rig.take().iter().any(|d| matches!(d, Do::Supersede(_))));
    assert_eq!((row(&rig, K), row(&rig, B)), (RowState::Plain, RowState::Connecting));
    assert_eq!(rig.sit().picking.map(|p| p.speaker), Some(id(B)));
}

#[test]
fn the_person_cancels_while_the_clicked_speaker_connects() {
    // *Cancel* drops the attempt: the sound stays where it is, the row
    // loses *Connecting…*, and what was there before the click is back.
    let mut rig = playing_on_this_pc();
    let before = rig.sit();
    rig.cmd(switch(K)).unwrap();
    rig.wait(1_000);
    rig.take();
    rig.cmd(Command::CancelSwitch).unwrap();
    let dos = rig.take();
    assert!(dos.iter().any(|d| matches!(d, Do::Supersede(_))), "Kitchen's connection is no longer wanted: {dos:?}");
    assert!(dos.iter().all(|d| !matches!(d, Do::Windows(_) | Do::Connect(_))) && asks_no_player(&dos), "{dos:?}");
    assert!(rig.jobs.is_empty());
    assert_eq!(rig.sit(), before, "as before the click");
    assert_eq!((row(&rig, K), popup(&rig)), (RowState::Plain, None));
    // Kitchen answering afterwards changes nothing.
    rig.wait(5_000);
    assert_eq!((rig.sit().spot, rig.sit().note), (this_pc(), None));

    // The row of the place does the same: This PC's …
    let mut rig = playing_on_this_pc();
    rig.cmd(switch(K)).unwrap();
    rig.take();
    rig.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
    assert!(rig.take().iter().any(|d| matches!(d, Do::Supersede(_))));
    assert_eq!((rig.sit().picking, row(&rig, K), rig.sit().spot), (None, RowState::Plain, this_pc()));
    assert!(rig.jobs.is_empty());
    // … and a speaker's, while it plays on.
    let mut rig = Rig::playing_on_kitchen();
    rig.cmd(switch(B)).unwrap();
    rig.take();
    rig.cmd(switch(K)).unwrap();
    let dos = rig.take();
    assert!(dos.iter().any(|d| matches!(d, Do::Supersede(_))), "{dos:?}");
    assert!(!silenced(&dos, K) && !dos.iter().any(|d| matches!(d, Do::Connect(_))), "Kitchen plays on: {dos:?}");
    assert_eq!((rig.sit().picking, row(&rig, B), rig.sit().spot), (None, RowState::Plain, Spot::Speaker(id(K))));
    assert_eq!(rig.sit().doing, Doing::Playing);

    // A note that stood before the click stands after the cancel.
    let mut rig = Rig::playing_on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.wait(10_250);
    let note = rig.sit().note.expect("set-up: the note");
    rig.cmd(Command::TryAgain(id(K))).unwrap();
    rig.cmd(Command::CancelSwitch).unwrap();
    assert_eq!((rig.sit().note, rig.sit().picking, rig.sit().spot), (Some(note), None, this_pc()));

    // With nothing connecting it does nothing; the place's own connecting
    // and the tries are not a person's switch.
    let mut rig = Rig::playing_on_kitchen();
    rig.speaker(K, SpeakerSignal::NotResponding);
    rig.wait(500);
    rig.take();
    rig.cmd(Command::CancelSwitch).unwrap();
    assert!(rig.take().iter().all(|d| !matches!(d, Do::Supersede(_) | Do::Windows(_))));
    assert_eq!((rig.sit().spot, rig.sit().doing), (Spot::Speaker(id(K)), Doing::Trying));
}

#[test]
fn a_switch_that_connects_can_be_left_without_a_cancel_of_its_own() {
    // The other value of the fixed choice: no *Cancel*. The row of the
    // place still drops the attempt.
    let mut rig = playing_on_this_pc();
    rig.core.cancel_while_connecting(false);
    rig.cmd(switch(K)).unwrap();
    rig.take();
    assert_eq!(rig.sit().picking.map(|p| p.can_cancel), Some(false), "not offered");
    rig.cmd(Command::CancelSwitch).unwrap();
    assert!(rig.take().iter().all(|d| !matches!(d, Do::Supersede(_))));
    assert_eq!((rig.sit().picking.map(|p| p.speaker), row(&rig, K)), (Some(id(K)), RowState::Connecting));
    rig.cmd(Command::Switch { place: Place::ThisPc, way: None }).unwrap();
    assert!(rig.take().iter().any(|d| matches!(d, Do::Supersede(_))));
    assert_eq!((rig.sit().picking, row(&rig, K)), (None, RowState::Plain));
}

#[test]
fn the_clicked_speaker_gets_ready() {
    let mut rig = playing_on_this_pc();
    rig.cmd(switch(K)).unwrap();
    rig.take();
    rig.end(Purpose::Switch, Ended::Ready);
    let dos = rig.take();
    assert!(to_ours(&dos), "the sound moves: {dos:?}");
    assert!(asks_no_player(&dos));
    rig.wait(500);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing, s.picking.clone()), (Spot::Speaker(id(K)), Doing::Playing, None));
    assert_eq!((s.note.clone(), popup(&rig)), (None, None));
}

#[test]
fn the_clicked_speaker_doesnt_get_ready() {
    let mut rig = playing_on_this_pc();
    rig.open_panel();
    rig.cmd(switch(K)).unwrap();
    rig.take();
    rig.end(Purpose::Switch, Ended::NotResponding);
    let dos = rig.take();
    assert!(!to_ours(&dos) && asks_no_player(&dos), "nothing changes: {dos:?}");
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing));
    assert_eq!(row(&rig, K), RowState::NotResponding, "its row says why");
    // The note answers the click: the sound is still where it was, and the
    // button tries the same switch again.
    let want = Note {
        cause: Cause::DidntGetReady { speaker: id(K), why: PickWhy::NotResponding },
        at: None,
        available_again: false,
        ahead: None,
    };
    assert_eq!(s.note, Some(want.clone()));
    assert!(want.failure());
    assert_eq!(button(&rig), Some(NoteButton::TryAgain(id(K))));
    assert_eq!((popup(&rig), s.amber), (None, false), "from the panel nothing shows by the clock");
    // It is an answer, not a standing note: it goes when the panel closes.
    rig.close_panel();
    assert_eq!((rig.sit().note, popup(&rig)), (None, None));
    assert_eq!(rig.core.kept().note, None);
    assert_eq!(row(&rig, K), RowState::NotResponding, "the row goes on saying it");

    // A switch from outside the panel says so by the clock, and the note
    // is there when the panel is opened.
    let mut rig = playing_on_this_pc();
    rig.cmd_from(Surface::Tray, switch(K)).unwrap();
    rig.end(Purpose::Switch, Ended::NotResponding);
    assert_eq!(popup(&rig), Some(Popup::DidntGetReady { speaker: id(K), why: PickWhy::NotResponding }));
    rig.open_panel();
    assert_eq!((rig.sit().spot, rig.sit().note), (this_pc(), Some(want)));
}

#[test]
fn headphones_are_connected() {
    // Windows moves the sound to them, or the app does where Windows
    // keeps to the app's device.
    for windows_switches in [true, false] {
        let mut rig = Rig::playing_on_kitchen();
        rig.plug_headphones(windows_switches);
        rig.wait(2_500);
        let dos = rig.take();
        assert!(silenced(&dos, K), "Kitchen stops playing: {dos:?}");
        assert!(asks_no_player(&dos));
        if !windows_switches {
            assert!(windows_to(&dos, &WinTarget::Output(out("headphones"))), "{dos:?}");
        }
        assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
        let s = rig.sit();
        assert_eq!((s.spot.clone(), s.doing), (Spot::ThisPc(Some(out("headphones"))), Doing::Playing));
        assert_eq!(rig.core.kept().place, None, "This PC is the place, as a click on its row would make it");
        // The note says where the sound came from, and offers the way back.
        let want = Note {
            cause: Cause::HeadphonesConnected { before: id(K), name: "Headphones".into() },
            at: None,
            available_again: false,
            ahead: None,
        };
        assert_eq!(s.note, Some(want.clone()));
        assert!(!want.failure());
        assert_eq!(button(&rig), Some(NoteButton::SwitchTo(id(K))));
        assert_eq!((popup(&rig), s.amber), (None, false), "nothing by the clock");
    }

    // This PC was the place already: only its output changes, and nothing
    // is said.
    let mut rig = playing_on_this_pc();
    rig.plug_headphones(true);
    rig.wait(2_500);
    assert_eq!((rig.sit().spot, rig.sit().note), (Spot::ThisPc(Some(out("headphones"))), None));
}

#[test]
fn headphones_go_and_a_speaker_was_the_place_before_them() {
    let mut rig = Rig::playing_on_kitchen();
    rig.plug_headphones(true);
    rig.wait(30_000);
    rig.take();
    rig.unplug_headphones();
    let at = rig.now.local;
    let dos = rig.take();
    assert!(!to_ours(&dos) && !dos.iter().any(|d| matches!(d, Do::Connect(_))), "nothing goes back to Kitchen: {dos:?}");
    assert!(asks_no_player(&dos));
    rig.wait(500);
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing), "the sound stays on this PC");
    let want = Note {
        cause: Cause::HeadphonesGone { before: id(K), name: "Headphones".into() },
        at: Some(at),
        available_again: false,
        ahead: None,
    };
    assert_eq!(s.note, Some(want.clone()));
    assert!(!want.failure());
    assert_eq!(button(&rig), Some(NoteButton::SwitchTo(id(K))));
    assert_eq!(popup(&rig), Some(Popup::Note(want)), "by the clock while sound plays");

    // While nothing plays: the note, and nothing by the clock.
    let mut rig = Rig::on_kitchen();
    rig.plug_headphones(true);
    rig.wait(30_000);
    rig.unplug_headphones();
    assert!(matches!(cause(&rig), Some(Cause::HeadphonesGone { .. })));
    assert_eq!((popup(&rig), rig.sit().spot), (None, this_pc()));

    // This PC was the place before them: the app did nothing and says
    // nothing.
    let mut rig = playing_on_this_pc();
    rig.plug_headphones(true);
    rig.wait(30_000);
    rig.unplug_headphones();
    assert_eq!((rig.sit().note, popup(&rig), rig.sit().spot), (None, None, this_pc()));
}

#[test]
fn pause_is_pressed_on_the_speaker_itself() {
    let mut rig = Rig::playing_on_kitchen();
    rig.speaker(K, SpeakerSignal::PausedAtSpeaker);
    let dos = rig.take();
    assert!(paused(&dos, SPOTIFY), "the pause is passed on to the player: {dos:?}");
    assert!(!to_speakers(&dos), "and nothing more: {dos:?}");
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Paused);
    rig.wait(6_000);
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (Spot::Speaker(id(K)), Doing::NothingPlaying));
    assert_eq!((s.note.clone(), popup(&rig), s.standing.clone()), (None, None, Vec::new()));

    // The player starts again by the usual routes, and plays on Kitchen.
    rig.take();
    rig.play(SPOTIFY);
    rig.wait(1_000);
    assert!(asks_no_player(&rig.take()));
    assert_eq!((rig.sit().spot, rig.sit().doing), (Spot::Speaker(id(K)), Doing::Playing));
}

#[test]
fn the_app_starts_while_sound_plays_on_this_pc() {
    let kept = Kept { place: Some(id(K)), named: Some(id(K)), first_run_done: true, ..Kept::default() };
    let mut w = world();
    w.players.push(PlayerWorld {
        id: PlayerId::new(SPOTIFY),
        app: AppKey::new(SPOTIFY),
        name: Some(SPOTIFY.into()),
        status: PlayerStatus::Playing,
    });
    let mut rig = Rig::starting(kept, w, StartKind::Normal, false);
    rig.sources.push(Source { app: AppKey::new(SPOTIFY), player: Some(PlayerId::new(SPOTIFY)), output: None });
    rig.wait(3_500);
    let dos = rig.take();
    assert!(!to_ours(&dos), "the sound stays where it plays: {dos:?}");
    assert!(asks_no_player(&dos));
    let s = rig.sit();
    assert_eq!((s.spot.clone(), s.doing), (this_pc(), Doing::Playing), "This PC is the place");
    let note = s.note.clone().expect("the note");
    assert_eq!(note.cause, Cause::PlayingAtStart { speaker: id(K) });
    assert!(!note.failure());
    assert_eq!(note.button(), Some(NoteButton::SwitchTo(id(K))));
    assert_eq!(popup(&rig), Some(Popup::Note(note.clone())), "the same by the clock");

    // Once: when it has gone, only the panel's note is left, until a
    // switch or its ×.
    rig.wait(20_000);
    assert_eq!((popup(&rig), rig.sit().note), (None, Some(note)));
}

#[test]
fn the_app_quits() {
    let mut rig = Rig::playing_on_kitchen();
    rig.cmd_from(Surface::Tray, Command::Quit).unwrap();
    let dos = rig.take();
    assert!(to_speakers(&dos) && silenced(&dos, K), "the sound goes on from this PC's own output: {dos:?}");
    assert!(asks_no_player(&dos), "nothing is paused");
    assert_eq!(rig.status(SPOTIFY), PlayerStatus::Playing);
    assert_eq!((rig.sit().note, popup(&rig), rig.core.kept().note), (None, None, None), "and nothing is said");
}
