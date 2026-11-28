//! Examples of the situation-based surfaces, including their shared word checks.

use super::by_clock;
use super::command::{Command, DelayStep, Open, Refusal, VolumeOwner};
use super::facts::*;
use super::panel::*;
use super::scene::*;
use super::situation::*;
use super::state::{PcOutputId, Place, Secret, SpeakerId};
use super::text::{Lang, LocalTime};

use super::samples::*;

fn check_act(act: &Act, check: &impl Fn(&str)) {
    check(&act.label);
    if let Some(spoken) = &act.spoken {
        check(spoken);
    }
}
fn check_question(q: &QuestionView, check: &impl Fn(&str)) {
    check(&q.title);
    check(&q.sentence);
    check_act(&q.yes, check);
    check_act(&q.no, check);
}

// Walk the actual display and accessibility fields, including folded rows.
fn view(s: &Scene) -> Panel {
    let p = panel(s);
    let twins: Vec<_> = s
        .speakers
        .iter()
        .filter(|sp| {
            s.speakers
                .iter()
                .any(|other| other.id != sp.id && other.name == sp.name)
        })
        .filter_map(|sp| sp.address.as_ref().map(|a| format!("{} ({a})", sp.name)))
        .collect();
    let check = |words: &str| {
        check_words_allowing("scene", "surface", words, &twins);
        let lower = words.to_lowercase();
        assert!(!lower.contains("resume"), "{words}");
        if lower.contains("pause") {
            let paused = s.situation.standing.iter().find_map(|v| match v {
                Standing::PausedAtSpeaker { speaker } => {
                    Some(format!("{} was paused", s.speaker_name(speaker)))
                }
                _ => None,
            });
            assert!(paused.is_some() && (Some(words) == paused.as_deref() || words == "Paused on the speaker" ||
                words == format!("Stream To Speaker – {} · Paused on the speaker", p.place.name) ||
                words == "It was paused with its own buttons or app, so this PC’s sound isn’t heard."), "{words}");
        }
    };
    let optional = |word: &Option<String>| {
        if let Some(w) = word {
            check(w);
        }
    };
    let action = |act: &Option<Act>| {
        if let Some(a) = act {
            check_act(a, &check);
        }
    };
    let figured = |f: &Figured| {
        check(&[f.before.as_str(), &f.figure, &f.after].concat());
    };
    check(&p.place.name);
    optional(&p.place.output);
    check(&p.place.state.text);
    action(&p.place.state.link);
    if let Some(v) = &p.place.volume {
        check(&v.number);
        check(&v.spoken);
        check(&v.mute_spoken);
    }
    let pc = &p.list.this_pc;
    check(&pc.name);
    check(&pc.output);
    optional(&pc.tooltip);
    action(&pc.arrow);
    for o in &pc.outputs {
        check(&o.name);
        check(&o.tooltip);
    }
    if let Some(f) = &p.list.more {
        check(&f.label);
    }
    if let Some(line) = &p.list.search {
        check(&line.text);
        action(&line.link);
    }
    for row in p
        .list
        .rows
        .iter()
        .chain(p.list.more.iter().flat_map(|f| &f.rows))
    {
        check(&row.name);
        optional(&row.text);
        action(&row.link);
        optional(&row.tooltip);
        check_act(&row.gear, &check);
        check(&row.spoken);
    }
    match &p.about {
        About::Empty => (),
        About::Line { text, link } => {
            check(text);
            action(link);
        }
        About::Question(q) => check_question(q, &check),
        About::Note(n) => {
            check(&n.title);
            check(&n.sentence);
            action(&n.button);
            action(&n.close);
        }
        About::Delay(d) => {
            match &d.head {
                DelayHead::One(f) => figured(f),
                DelayHead::Ways { label, lines } => {
                    check(label);
                    for l in lines {
                        check(&l.name);
                        figured(&l.gives);
                        action(&l.connecting);
                        check(&l.spoken);
                    }
                }
            }
            for row in &d.rows {
                check(&row.question);
                check(&row.verb);
                check_act(&row.small, &check);
                check_act(&row.large, &check);
            }
            optional(&d.line);
        }
    }
    for act in &p.links {
        check_act(act, &check);
    }
    if let Some(shown) = by_clock::popup(s) {
        let v = shown.what;
        check(&v.title);
        optional(&v.sentence);
        action(&v.button);
        check_act(&v.close, &check);
    }
    if let Some(shown) = by_clock::confirmation(s) {
        check_question(&shown.what, &check);
    }
    let tray = by_clock::tray(s);
    check(&tray.tooltip);
    for entry in tray.menu {
        check(&entry.label);
    }
    let w = by_clock::window_words(s);
    check(&w.strip);
    check(&w.title);
    check(&w.sentence);
    check_act(&w.quit, &check);
    p
}

fn moment(s: &Scene, name: &str, state: &str, sign: Sign) -> About {
    let p = view(s);
    assert_eq!(p.place.name, name);
    assert_eq!(
        p.place.output,
        if name == "This PC" {
            s.outputs.first().map(|o| s.output_label(o))
        } else {
            None
        }
    );
    assert_eq!(
        (p.place.state.text.as_str(), p.place.state.sign),
        (state, sign)
    );
    p.about
}
fn assert_button(button: Option<&Act>, expected: Option<(&str, Command)>) {
    assert_eq!(
        button.map(|a| (a.label.as_str(), &a.command)),
        expected.as_ref().map(|(s, c)| (*s, c))
    );
}
fn assert_note(
    about: About,
    mark: Mark,
    title: &str,
    sentence: &str,
    button: Option<(&str, Command)>,
    close: bool,
) {
    let About::Note(n) = about else {
        panic!("expected a note: {about:?}")
    };
    assert_eq!(
        (n.mark, n.title.as_str(), n.sentence.as_str()),
        (mark, title, sentence)
    );
    assert_button(n.button.as_ref(), button);
    assert_eq!(
        n.close.map(|a| (a.label, a.command)),
        close.then(|| ("Close".to_string(), Command::CloseNote))
    );
}
fn delay(about: About) -> DelayControls {
    let About::Delay(d) = about else {
        panic!("expected delay: {about:?}")
    };
    d
}
fn first_delay(doing: Doing, state: &str, sign: Sign, dimmed: bool) {
    let mut s = scene();
    s.situation.doing = doing;
    let d = delay(moment(&s, "Kitchen", state, sign));
    assert_eq!(d.speaker, kitchen());
    assert_eq!(d.dimmed, dimmed);
    assert_eq!(d.line, None);
    assert_eq!(
        d.head,
        DelayHead::One(Figured {
            before: "Kitchen is ".into(),
            figure: "about 150 ms".into(),
            after: " behind the picture.".into()
        })
    );
    for (r, remove) in d.rows.iter().zip([true, false]) {
        assert_eq!(r.enabled, !dimmed);
        let (question, verb) = if remove {
            ("Sound behind the picture?", "Remove")
        } else {
            ("Sound breaking up?", "Add")
        };
        assert_eq!((r.question.as_str(), r.verb.as_str()), (question, verb));
        for (button, ms) in [(&r.small, 25), (&r.large, 100)] {
            assert_eq!(button.label, format!("{ms} ms"));
            assert_eq!(button.spoken.as_ref().unwrap(), &format!("{verb} {ms} ms"));
            let step = if remove {
                DelayStep::Remove(ms)
            } else {
                DelayStep::Add(ms)
            };
            assert_eq!(
                button.command,
                Command::ChangeDelay {
                    speaker: kitchen(),
                    step
                }
            );
        }
    }
}

#[test]
fn the_tray_tooltip_says_why_the_icon_is_amber_over_this_pc() {
    let s = named("pc-kitchen-stopped-responding");
    let tray = by_clock::tray(&s);
    assert!(tray.amber && !tray.accent);
    assert_eq!(tray.tooltip, "Stream To Speaker – This PC · Playing · Kitchen stopped responding");
    // No failure stands: the place and what it does, nothing more.
    let s = named("pc-kitchen-available-again");
    let tray = by_clock::tray(&s);
    assert!(!tray.amber);
    assert_eq!(tray.tooltip, "Stream To Speaker – This PC · Playing");
    // A speaker as the place says its trouble in its own state.
    let s = named("kitchen-not-responding-trying");
    assert_eq!(by_clock::tray(&s).tooltip, "Stream To Speaker – Kitchen · Not responding. Trying again…");
}

#[test]
fn sound_plays_on_kitchen() {
    first_delay(Doing::Playing, "Playing", Sign::Bars, false);
}
#[test]
fn nothing_plays_on_kitchen() {
    first_delay(Doing::NothingPlaying, "Nothing playing", Sign::Still, false);
}
#[test]
fn sound_starts_and_connects() {
    first_delay(Doing::Connecting, "Connecting…", Sign::Spinner, true);
}

fn seconds_ahead(doing: Doing, cause: Cause) {
    let mut s = scene();
    s.situation.doing = doing;
    s.situation.note = Some(Note {
        ahead: Some(Ahead::InAFewSeconds),
        ..note(cause)
    });
    assert_note(
        moment(
            &s,
            "Kitchen",
            "Not responding. Trying again…",
            Sign::SpinnerWarn,
        ),
        Mark::Warn,
        "Kitchen isn’t responding",
        "The sound moves to this PC in a few seconds.",
        Some(("Switch to this PC now", to_pc())),
        false,
    );
}
#[test]
fn start_has_waited_two_seconds() {
    seconds_ahead(
        Doing::Connecting,
        Cause::NotRespondingAtStart {
            speaker: kitchen(),
            why: Away::NotResponding,
        },
    );
}
#[test]
fn speaker_stops_during_play() {
    seconds_ahead(Doing::Trying, stopped());
}

fn moved_note(n: Note, mark: Mark, title: &str, sentence: &str, button: Option<(&str, Command)>) {
    let mut s = pc();
    s.situation.note = Some(n);
    assert_note(
        moment(&s, "This PC", "Playing", Sign::Bars),
        mark,
        title,
        sentence,
        button,
        true,
    );
}
#[test]
fn moved_after_tries() {
    moved_note(
        note(stopped()),
        Mark::Warn,
        "Kitchen stopped responding",
        "The app moved the sound to this PC at 21:14.",
        Some(("Try Kitchen again", Command::TryAgain(kitchen()))),
    );
}
#[test]
fn speaker_can_play_again() {
    moved_note(
        Note {
            available_again: true,
            ..note(stopped())
        },
        Mark::Info,
        "Kitchen is available again",
        "The sound stays on this PC until you switch.",
        Some(("Switch to Kitchen", switch_to(kitchen()))),
    );
}
#[test]
fn someone_else_plays() {
    moved_note(
        note(Cause::PlayingElse {
            speaker: kitchen(),
            who: None,
        }),
        Mark::Warn,
        "Kitchen is playing something else",
        "Switching back stops what is playing there.",
        Some(("Switch to Kitchen", switch_to(kitchen()))),
    );
}
#[test]
fn asks_for_its_code_again() {
    moved_note(
        note(Cause::NeedsSecret {
            speaker: kitchen(),
            secret: Secret::Code,
        }),
        Mark::Warn,
        "Kitchen asks for its code again",
        "The app moved the sound to this PC at 21:14.",
        Some(("Enter the code…", Command::Open(Open::Pair(kitchen())))),
    );
}
#[test]
fn speaker_away_while_quiet() {
    let mut s = scene();
    s.situation.doing = Doing::NothingPlaying;
    s.situation.away = Some(Away::NotFound);
    s.situation.note = Some(Note {
        ahead: Some(Ahead::WhenSoundStarts),
        ..note(Cause::NotRespondingAtStart {
            speaker: kitchen(),
            why: Away::NotFound,
        })
    });
    assert_note(
        moment(&s, "Kitchen", "Not found", Sign::Warn),
        Mark::Warn,
        "Kitchen can’t be found",
        "Sound that starts now plays on this PC.",
        Some(("Switch to this PC", to_pc())),
        true,
    );
}
fn assert_connecting(s: &Scene) {
    let About::Line { text, link } = moment(s, "This PC", "Playing", Sign::Bars) else {
        panic!("connecting line")
    };
    assert_eq!(
        text,
        "Connecting to Kitchen. The sound stays on this PC and moves when Kitchen is ready."
    );
    assert_button(link.as_ref(), Some(("Cancel", Command::CancelSwitch)));
    assert_eq!(
        link.unwrap().spoken.as_deref(),
        Some("Cancel connecting to Kitchen")
    );
}
#[test]
fn clicked_a_row() {
    let mut s = pc();
    s.situation.picking = Some(pick(kitchen()));
    assert_connecting(&s);
}
#[test]
fn another_speakers_note_stands() {
    let mut s = pc();
    let bed = id("bed");
    s.speakers.push(seen(bed.clone(), "Bedroom"));
    s.situation.picking = Some(pick(kitchen()));
    s.situation.note = Some(note(Cause::StoppedResponding {
        speaker: bed.clone(),
        vpn: None,
    }));
    assert_note(
        moment(&s, "This PC", "Playing", Sign::Bars),
        Mark::Warn,
        "Bedroom stopped responding",
        "The app moved the sound to this PC at 21:14.",
        Some(("Try Bedroom again", Command::TryAgain(bed))),
        true,
    );
}
#[test]
fn clicked_the_notes_own_button() {
    let mut s = pc();
    s.situation.picking = Some(Picking {
        by: SwitchBy::TryAgain,
        ..pick(kitchen())
    });
    s.situation.note = Some(note(stopped()));
    assert_connecting(&s);
}
#[test]
fn switch_didnt_get_ready() {
    moved_note(
        note(Cause::DidntGetReady {
            speaker: kitchen(),
            why: PickWhy::NotResponding,
        }),
        Mark::Warn,
        "Kitchen didn’t respond",
        "The sound is still on this PC.",
        Some(("Try Kitchen again", Command::TryAgain(kitchen()))),
    );
}
#[test]
fn headphones_connected() {
    let mut s = pc();
    s.outputs = vec![output("Headphones", OutputKind::Headphones)];
    s.situation.pc_output = Some(s.outputs[0].id.clone());
    on_pc(&mut s);
    s.situation.note = Some(note(Cause::HeadphonesConnected {
        before: kitchen(),
        name: "Headphones".into(),
    }));
    assert_note(
        moment(&s, "This PC", "Playing", Sign::Bars),
        Mark::Info,
        "Headphones connected",
        "The sound moved here from Kitchen. When they disconnect, it plays on this PC’s speakers.",
        Some(("Switch to Kitchen", switch_to(kitchen()))),
        true,
    );
}
#[test]
fn headphones_disconnected() {
    for (kind, name, sentence) in [
        (OutputKind::Speakers, "Speakers", "The sound moved to this PC’s speakers at 21:14."),
        (OutputKind::Display, "Desk monitor", "The sound moved to Desk monitor at 21:14."),
        (OutputKind::Other, "USB audio", "The sound moved to USB audio at 21:14."),
    ] {
        let mut s = pc();
        s.outputs = vec![output(name, kind)];
        s.situation.pc_output = Some(s.outputs[0].id.clone());
        on_pc(&mut s);
        s.situation.note = Some(note(Cause::HeadphonesGone { before: kitchen(), name: "Headphones".into() }));
        assert_eq!(view(&s).place.output.as_deref(), Some(name));
        assert_note(
            moment(&s, "This PC", "Playing", Sign::Bars),
            Mark::Info,
            "Headphones disconnected",
            sentence,
            Some(("Switch to Kitchen", switch_to(kitchen()))),
            true,
        );
    }
}
#[test]
fn sound_stayed_at_start() {
    moved_note(
        note(Cause::PlayingAtStart { speaker: kitchen() }),
        Mark::Info,
        "The sound stayed on this PC",
        "It was playing here when the app started.",
        Some(("Switch to Kitchen", switch_to(kitchen()))),
    );
}
#[test]
fn pc_without_an_output() {
    let mut s = pc();
    s.outputs.clear();
    s.situation.pc_output = None;
    on_pc(&mut s);
    s.situation.doing = Doing::NothingToPlayOn;
    assert_eq!(
        moment(
            &s,
            "This PC",
            "This PC has nothing to play on right now.",
            Sign::Warn
        ),
        About::Line {
            text: "Click a speaker to play this PC’s sound on it.".into(),
            link: None
        }
    );
    let v = view(&s).place.volume.unwrap();
    assert!(v.dimmed);
    assert!(v.owner.is_none());
}

#[test]
fn every_cause_and_form_has_the_rules_button_mark_and_close() {
    use Cause::*;
    let k = kitchen();
    let mut causes = vec![
        stopped(),
        NetworkLost {
            speaker: k.clone(),
            kind: ConnKind::WiFi,
        },
        NetworkLost {
            speaker: k.clone(),
            kind: ConnKind::Wired,
        },
        NeedsSecret {
            speaker: k.clone(),
            secret: Secret::Code,
        },
        NeedsSecret {
            speaker: k.clone(),
            secret: Secret::Password,
        },
        PlayingElse {
            speaker: k.clone(),
            who: None,
        },
        PlayingElse {
            speaker: k.clone(),
            who: Some("LAPTOP-7Q2K".into()),
        },
        Disconnected { speaker: k.clone() },
        GroupedInto {
            speaker: k.clone(),
            with: "Living Room".into(),
        },
        OtherNetwork { speaker: k.clone() },
        Removed { speaker: k.clone() },
        PlayingAtStart { speaker: k.clone() },
        HeadphonesGone {
            before: k.clone(),
            name: "Headphones".into(),
        },
        HeadphonesConnected {
            before: k.clone(),
            name: "Display".into(),
        },
        SwitchedAfterQuiet { from: k.clone() },
    ];
    for name in [None, Some("Corp VPN".into())] {
        causes.push(StoppedResponding {
            speaker: k.clone(),
            vpn: Some(NetJoined {
                name,
                at: LocalTime::new(21, 13),
            }),
        });
    }
    for why in [Away::NotResponding, Away::NotFound, Away::NoNetwork] {
        causes.push(NotRespondingAtStart {
            speaker: k.clone(),
            why,
        });
    }
    for why in [
        PickWhy::NotResponding,
        PickWhy::NotFound,
        PickWhy::NoNetwork,
        PickWhy::NeedsSecret(Secret::Code),
        PickWhy::NeedsSecret(Secret::Password),
        PickWhy::WrongSecret(Secret::Code),
        PickWhy::WrongSecret(Secret::Password),
        PickWhy::CouldntStart,
        PickWhy::Refused,
    ] {
        causes.push(DidntGetReady {
            speaker: k.clone(),
            why,
        });
    }
    for cause in causes {
        for (ahead, available_again) in [
            (None, false),
            (None, true),
            (Some(Ahead::InAFewSeconds), false),
            (Some(Ahead::WhenSoundStarts), false),
        ] {
            for at in [None, Some(LocalTime::new(21, 14))] {
                let n = Note {
                    cause: cause.clone(),
                    ahead,
                    available_again,
                    at,
                };
                let mut s = if ahead.is_none() { pc() } else { scene() };
                s.situation.doing = match ahead {
                    Some(Ahead::InAFewSeconds) => Doing::Trying,
                    Some(Ahead::WhenSoundStarts) => Doing::NothingPlaying,
                    None => Doing::Playing,
                };
                s.situation.note = Some(n.clone());
                if !n.shows() {
                    assert!(!matches!(view(&s).about, About::Note(_)));
                    continue;
                }
                if ahead.is_none() {
                    s.situation.popups.popup = Some(shown(Popup::Note(n.clone())));
                }
                let About::Note(v) = view(&s).about else {
                    panic!("{n:?}")
                };
                assert_eq!(v.mark, if n.failure() { Mark::Warn } else { Mark::Info });
                assert_eq!(
                    v.close.as_ref().map(|a| &a.command),
                    n.can_close().then_some(&Command::CloseNote)
                );
                let expected = n.button().map(|b| match b {
                    NoteButton::TryAgain(id) => ("Try Kitchen again", Command::TryAgain(id)),
                    NoteButton::SwitchTo(id) => ("Switch to Kitchen", switch_to(id)),
                    NoteButton::SwitchToThisPc => (
                        if ahead == Some(Ahead::InAFewSeconds) {
                            "Switch to this PC now"
                        } else {
                            "Switch to this PC"
                        },
                        to_pc(),
                    ),
                    NoteButton::EnterSecret(id, secret) => (
                        if secret == Secret::Code {
                            "Enter the code…"
                        } else {
                            "Enter the password…"
                        },
                        Command::Open(Open::Pair(id)),
                    ),
                });
                assert_button(v.button.as_ref(), expected);
                if available_again {
                    assert_eq!(v.title, "Kitchen is available again");
                    assert_eq!(v.sentence, "The sound stays on this PC until you switch.");
                }
                if let Some(a) = ahead {
                    assert_eq!(
                        v.sentence,
                        if a == Ahead::InAFewSeconds {
                            "The sound moves to this PC in a few seconds."
                        } else {
                            "Sound that starts now plays on this PC."
                        }
                    );
                }
                if let Some(p) = by_clock::popup(&s) {
                    let own = available_again
                        || matches!(
                            cause,
                            HeadphonesGone { .. }
                                | HeadphonesConnected { .. }
                                | PlayingAtStart { .. }
                                | SwitchedAfterQuiet { .. }
                                | DidntGetReady { .. }
                        );
                    assert_eq!(p.what.title, v.title);
                    assert_eq!(
                        p.what.sentence.as_deref(),
                        Some(if own {
                            v.sentence.as_str()
                        } else {
                            "The sound is on this PC."
                        })
                    );
                    assert_eq!(p.what.button, v.button);
                    assert_eq!(p.what.close.command, Command::ClosePopup);
                }
            }
        }
    }
}

#[test]
fn status_lines_follow_precedence_and_link_to_the_right_page() {
    let k = kitchen();
    let app = Standing::AppElsewhere {
        app: "Chrome".into(),
        output: PcOutputId::new("o"),
    };
    for (doing, standing, away, sign, text, link) in [
        (
            Doing::Playing,
            vec![
                app.clone(),
                Standing::RoomJoined {
                    room: "Bedroom".into(),
                },
            ],
            None,
            Sign::Bars,
            "Playing · now also in Bedroom",
            None,
        ),
        (
            Doing::Playing,
            vec![app.clone()],
            None,
            Sign::Bars,
            "Playing, except Chrome",
            Some(("Why?", Open::CantHear(Some(k.clone())))),
        ),
        (
            Doing::NothingPlaying,
            vec![app],
            None,
            Sign::Still,
            "Chrome plays on this PC",
            Some(("Why?", Open::CantHear(Some(k.clone())))),
        ),
        (
            Doing::Playing,
            vec![Standing::OnlySomeApps {
                none_playing: false,
            }],
            None,
            Sign::Bars,
            "Playing · only chosen apps",
            None,
        ),
        (
            Doing::NothingPlaying,
            vec![Standing::OnlySomeApps { none_playing: true }],
            None,
            Sign::Still,
            "Only chosen apps play here",
            Some(("Choose apps…", Open::VolumeMixer)),
        ),
        (
            Doing::Playing,
            vec![
                Standing::PausedAtSpeaker { speaker: k.clone() },
                Standing::CantReadSound { speaker: k.clone() },
            ],
            None,
            Sign::Warn,
            "Can’t read this PC’s sound",
            None,
        ),
        (
            Doing::Playing,
            vec![
                Standing::MusicAssistantNotStarted,
                Standing::PausedAtSpeaker { speaker: k.clone() },
            ],
            Some(Away::NotFound),
            Sign::Still,
            "Paused on the speaker",
            None,
        ),
        (
            Doing::Playing,
            vec![Standing::MusicAssistantNotStarted],
            Some(Away::NotFound),
            Sign::Still,
            "Not playing yet",
            None,
        ),
        (
            Doing::Playing,
            vec![Standing::MusicAssistantNotConnected],
            None,
            Sign::Warn,
            "Not connected",
            None,
        ),
        (
            Doing::NothingPlaying,
            vec![],
            Some(Away::NotResponding),
            Sign::Warn,
            "Not responding",
            None,
        ),
        (
            Doing::NothingPlaying,
            vec![],
            Some(Away::NoNetwork),
            Sign::Warn,
            "No network connection",
            None,
        ),
    ] {
        let mut s = scene();
        s.situation.doing = doing;
        s.situation.standing = standing;
        s.situation.away = away;
        let l = view(&s).place.state;
        assert_eq!((l.sign, l.text.as_str()), (sign, text));
        assert_button(
            l.link.as_ref(),
            link.map(|(label, page)| (label, Command::Open(page))),
        );
    }
    for doing in [Doing::Trying, Doing::Connecting] {
        for (cause, text) in [
            (
                Cause::NetworkLost {
                    speaker: k.clone(),
                    kind: ConnKind::WiFi,
                },
                "No network connection. Trying again…",
            ),
            (
                Cause::NotRespondingAtStart {
                    speaker: k.clone(),
                    why: Away::NoNetwork,
                },
                "Waiting for this PC’s network…",
            ),
        ] {
            let mut s = scene();
            s.situation.doing = doing;
            s.situation.note = Some(Note {
                ahead: Some(Ahead::InAFewSeconds),
                ..note(cause)
            });
            let l = view(&s).place.state;
            assert_eq!((l.sign, l.text.as_str()), (Sign::SpinnerWarn, text));
            assert!(l.link.is_none());
        }
    }
}

#[test]
fn lasting_notes_and_repairs() {
    for (state, mark, title, body, button) in [
        (RepairNow::Unavailable, Mark::Warn, "The app’s sound output is missing", "Turn Stream To Speaker back on in Windows’ sound settings, or install the app again.", Some(("Open Windows’ sound settings", Command::Open(Open::SoundSettings)))),
        (RepairNow::NotAsked, Mark::Warn, "The app’s sound output is missing", "The sound stays on this PC. Repair puts it back; it needs administrator approval.", Some(("Repair", Command::RepairOutput))),
        (RepairNow::TurnedOff, Mark::Warn, "Stream To Speaker is turned off in Windows", "Repair turns it back on.", Some(("Repair", Command::TurnOnOutput))),
        (RepairNow::TurnOnFailed, Mark::Warn, "Stream To Speaker is turned off in Windows", "Windows didn’t turn it back on. Turn it on in Windows’ sound settings.", Some(("Open Windows’ sound settings", Command::Open(Open::SoundSettings)))),
        (RepairNow::Running, Mark::Busy, "Repairing the app’s sound output…", "The sound stays on this PC meanwhile.", None),
        (RepairNow::DidntWork, Mark::Warn, "Repair didn’t work: the app’s sound output is still missing", "Restarting Windows may help; if not, report the problem.", Some(("Report a problem", Command::Open(Open::ReportProblem)))),
        (RepairNow::NotApproved, Mark::Warn, "Repair needs administrator approval, and it wasn’t given", "The sound stays on this PC.", Some(("Repair", Command::RepairOutput))),
        (RepairNow::Worked, Mark::Info, "The app’s sound output is back", "You can switch to a speaker again.", None),
    ] {
        let mut s = pc(); s.repair = state;
        if state != RepairNow::Worked { s.situation.standing.push(Standing::OutputMissing); }
        assert_note(view(&s).about, mark, title, body, button, false);
    }
    for (condition, mark, title, body, button) in [
        (Standing::WindowsDidntSwitch { speaker: kitchen() }, Mark::Warn, "Windows didn’t switch to Kitchen", "Try again, or choose the Stream To Speaker output in Windows’ sound settings.", ("Open Windows’ sound settings", Command::Open(Open::SoundSettings))),
        (Standing::MusicAssistantNotStarted, Mark::Warn, "Music Assistant hasn’t started playing", "Start this PC’s sound in Music Assistant, or set Music Assistant to start it by itself.", ("How to start it", Command::Open(Open::MusicAssistantHowTo))),
        (Standing::MusicAssistantNotConnected, Mark::Warn, "Music Assistant isn’t connected", "This PC’s sound isn’t heard until it connects.", ("How to connect it", Command::Open(Open::MusicAssistantSettings))),
        (Standing::PausedAtSpeaker { speaker: kitchen() }, Mark::Warn, "Kitchen was paused", "It was paused with its own buttons or app, so this PC’s sound isn’t heard.", ("Switch to this PC", to_pc())),
        (Standing::CantReadSound { speaker: kitchen() }, Mark::Warn, "Stream To Speaker can’t read this PC’s sound", "It keeps trying. Kitchen is silent meanwhile.", ("Switch to this PC", to_pc())),
        (Standing::CallOn { speaker: kitchen() }, Mark::Info, "A microphone is in use", "If this is a call, you will hear the other person late on Kitchen.", ("Switch to this PC", to_pc())),
    ] {
        let mut s = scene(); s.situation.standing.push(condition); assert_note(view(&s).about, mark, title, body, Some(button), false);
    }
    // The app's own trouble is said before what the speaker does.
    let mut s = scene();
    s.situation.standing = vec![Standing::PausedAtSpeaker { speaker: kitchen() }, Standing::CantReadSound { speaker: kitchen() }];
    assert_note(view(&s).about, Mark::Warn, "Stream To Speaker can’t read this PC’s sound", "It keeps trying. Kitchen is silent meanwhile.", Some(("Switch to this PC", to_pc())), false);
}

#[test]
fn part_three_chooses_one_thing_in_order() {
    let mut s = scene();
    s.situation.standing = vec![
        Standing::CallOn { speaker: kitchen() },
        Standing::PausedAtSpeaker { speaker: kitchen() },
        Standing::MusicAssistantNotStarted,
        Standing::WindowsDidntSwitch { speaker: kitchen() },
        Standing::OutputMissing,
    ];
    s.situation.picking = Some(pick(id("other")));
    s.situation.note = Some(note(stopped()));
    s.situation.confirm_in_panel = Some(Confirm::MayBeInUse {
        speaker: kitchen(),
        way: WayChoice::Native,
    });
    assert!(matches!(view(&s).about, About::Question(_)));
    s.situation.confirm_in_panel = None;
    let title = |s: &Scene| match view(s).about {
        About::Note(v) => v.title,
        v => panic!("{v:?}"),
    };
    assert_eq!(title(&s), "The app’s sound output is missing");
    s.situation.standing.pop();
    assert_eq!(title(&s), "Kitchen stopped responding");
    s.situation.note = None;
    assert!(matches!(view(&s).about, About::Line { link: Some(_), .. }));
    s.situation.picking = None;
    for expected in [
        "Windows didn’t switch to Kitchen",
        "Music Assistant hasn’t started playing",
        "Kitchen was paused",
        "A microphone is in use",
    ] {
        assert_eq!(title(&s), expected);
        s.situation.standing.pop();
    }
    assert!(matches!(view(&s).about, About::Delay(_)));
    s.speakers[0].ways.clear();
    assert_eq!(view(&s).about, About::Empty);
    on_pc(&mut s);
    assert!(matches!(view(&s).about, About::Line { link: None, .. }));
    s.situation.speakers[0].state = RowState::NotFound;
    assert_eq!(view(&s).about, About::Empty);
}

fn way_lines(s: &Scene) -> Vec<WayLine> {
    let DelayHead::Ways { label, lines } = delay(view(s).about).head else {
        panic!("expected ways")
    };
    assert_eq!(
        label,
        format!(
            "How this PC sends sound to {}",
            s.speaker_name(&s.speakers[0].id)
        )
    );
    lines
}
#[test]
fn figures_ways_and_a_press() {
    use WayChoice::*;
    let mut s = scene();
    for (own_ms, change_ms, measured, expected) in [
        (149, 0, false, "about 149 ms"),
        (1975, 0, false, "about 1.98 s"),
        (2000, 0, false, "about 2 s"),
        (2000, 0, true, "2 s"),
        (150, 0, true, "150 ms"),
        (1, -25, true, "0 ms"),
    ] {
        s.speakers[0].ways[0].delay = SpeakerDelay {
            own_ms,
            measured,
            change_ms,
        };
        let DelayHead::One(f) = delay(view(&s).about).head else {
            panic!()
        };
        assert_eq!(f.figure, expected);
    }
    s.speakers[0].ways = vec![
        way(Native, 150),
        way(AirPlayShort, 500),
        way(AirPlaySteady, 2000),
    ];
    let lines = way_lines(&s);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].spoken, "Sonos · about 150 ms behind the picture");
    assert!(lines[0].chosen && lines[0].command.is_none());
    assert_eq!(
        lines[1].spoken,
        "AirPlay · about 2 s behind, but breaks up less"
    );
    assert_eq!(
        lines[1].command,
        Some(Command::ChooseWay {
            speaker: kitchen(),
            way: AirPlaySteady
        })
    );
    let d = delay(view(&s).about);
    let Command::ChangeDelay { step, speaker } = d.rows[0].small.command.clone() else {
        panic!()
    };
    assert_eq!(speaker, kitchen());
    assert_eq!(step, DelayStep::Remove(25));
    let current = &mut s.speakers[0].ways[0];
    current.delay.change_ms =
        super::facts::press(current.range, current.delay.change_ms, step).after_ms;
    assert_eq!(way_lines(&s)[0].gives.figure, "about 125 ms");
    s.speakers[0].airplay = Some(AirPlayShort);
    assert_eq!(way_lines(&s)[1].way, AirPlayShort);
    s.speakers[0].kind = SpeakerKind::Other {
        maker: Some("Denon".into()),
    };
    assert_eq!(way_lines(&s)[0].name, "Denon");
    s.speakers[0].kind = SpeakerKind::Other { maker: None };
    assert_eq!(way_lines(&s)[0].name, "Directly");
    s.situation.picking = Some(Picking {
        way: AirPlayShort,
        by: SwitchBy::Way,
        ..pick(kitchen())
    });
    let lines = way_lines(&s);
    assert!(lines.iter().all(|l| l.command.is_none()));
    assert_eq!(lines[1].spoken, "AirPlay · Connecting…");
    assert_button(
        lines[1].connecting.as_ref(),
        Some(("Cancel", Command::CancelSwitch)),
    );
    let d = delay(view(&s).about);
    assert!(d.dimmed && d.rows.iter().all(|r| !r.enabled));
    s.situation.picking = None;
    s.speakers[0].ways.remove(0);
    s.speakers[0].way = AirPlaySteady;
    let lines = way_lines(&s);
    assert_eq!(lines[0].name, "Less delay");
    assert_eq!(lines[1].name, "Steadier");
    assert!(lines[1].chosen);
}

#[test]
fn delay_limits_fixed_progress_and_dimmed_states() {
    let mut s = scene();
    for (change, remove, add, line) in [
        (
            -100,
            false,
            true,
            "Kitchen can’t play with less delay than this.",
        ),
        (
            200,
            true,
            false,
            "Kitchen can’t play with more delay than this.",
        ),
    ] {
        s.speakers[0].ways[0].delay.change_ms = change;
        let d = delay(view(&s).about);
        assert_eq!(
            (d.rows[0].enabled, d.rows[1].enabled, d.line.as_deref()),
            (remove, add, Some(line))
        );
    }
    s.speakers[0].ways[0].applies = Applies::Reconnect;
    s.speakers[0].ways[0].delay.change_ms = 0;
    assert_eq!(
        delay(view(&s).about).line.as_deref(),
        Some("Changing the delay stops Kitchen’s sound for a few seconds.")
    );
    s.speakers[0].name = "Den TV".into();
    s.speakers[0].ways.push(way(WayChoice::AirPlaySteady, 2000));
    s.speakers[0].way = WayChoice::AirPlaySteady;
    s.speakers[0].ways[1].applies = Applies::Fixed;
    let d = delay(view(&s).about);
    assert!(d.rows.iter().all(|r| !r.enabled));
    assert_eq!(
        d.line.as_deref(),
        Some("To change the delay, choose Sonos above.")
    );
    let refusal = |s: &Scene, id: &SpeakerId| {
        let snap = super::view::view(s.clone(), None, false, 1);
        super::view::refusal_text(s.lang, &Refusal::FixedOnSteady { speaker: id.clone() }, &snap)
    };
    assert!(refusal(&s, &kitchen()).contains("Choose “Sonos”"));
    let homepod = super::samples::named("homepod-steady-only");
    let fixed = delay(view(&homepod).about);
    assert!(fixed.rows.iter().all(|r| !r.enabled));
    assert_eq!(fixed.line.as_deref(), Some("HomePod’s delay can’t be changed."));
    // Both Short and Sonos can change the delay. Only the first should be suggested.
    let mut three = s.clone();
    three.speakers[0].ways.insert(0, way(WayChoice::AirPlayShort, 125));
    assert!(refusal(&three, &kitchen()).contains("Choose “Less delay”"));
    let d = delay(view(&three).about);
    assert_eq!(d.line.as_deref(), Some("To change the delay, choose Less delay above."));
    assert!(way_lines(&three).iter().any(|l| l.name == "Less delay" && l.command == Some(Command::ChooseWay {
        speaker: kitchen(), way: WayChoice::AirPlayShort,
    })));
    for (progress, expected) in [
        (Progress::Live { to_go_ms: -75 }, "Removing… 75 ms to go"),
        (Progress::Live { to_go_ms: 75 }, "Adding… 75 ms to go"),
        (Progress::ReconnectPending, "Applying the new delay…"),
        (Progress::Reconnecting, "Applying the new delay…"),
    ] {
        s.progress = progress;
        assert_eq!(delay(view(&s).about).line.as_deref(), Some(expected));
    }
    for (doing, away) in [
        (Doing::Connecting, None),
        (Doing::Trying, None),
        (Doing::NothingPlaying, Some(Away::NotFound)),
    ] {
        s.situation.doing = doing;
        s.situation.away = away;
        let d = delay(view(&s).about);
        assert!(d.dimmed && d.rows.iter().all(|r| !r.enabled));
        assert!(way_lines(&s).iter().all(|l| l.command.is_none()));
    }
}

#[test]
fn music_assistant_that_is_not_connected_says_so_and_opens_its_own_page() {
    let p = view(&named("pc-music-assistant-not-connected"));
    let r = p.list.rows.iter().find(|r| r.place == Place::MusicAssistant).unwrap();
    assert_eq!(
        (r.text.as_deref(), r.spoken.as_str(), r.grey, &r.command),
        (Some("Not connected"), "Music Assistant, not connected", true, &None)
    );
    assert_button(r.link.as_ref(), Some(("Why?", Command::Open(Open::MusicAssistantSettings))));
    assert_eq!(r.link.as_ref().unwrap().spoken.as_deref(), Some("Why? Opens Music Assistant’s settings"));
    assert_button(Some(&r.gear), Some(("Music Assistant’s settings", Command::Open(Open::MusicAssistantSettings))));
    // While it is the place and can't play, the line under its name says
    // the same.
    let mut s = named("music-assistant-playing");
    s.situation.doing = Doing::NothingPlaying;
    s.situation.away = Some(Away::NotFound);
    assert_eq!(view(&s).place.state.text, "Not connected");
    // A speaker keeps its own words.
    let mut s = named("kitchen-nothing-playing");
    s.situation.away = Some(Away::NotFound);
    assert_eq!(view(&s).place.state.text, "Not found");
    // Chosen with no server connected, the app is sending all the same:
    // the line, the row and the note say why nothing is heard.
    let p = view(&named("music-assistant-not-connected"));
    assert_eq!((p.place.state.sign, p.place.state.text.as_str()), (Sign::Warn, "Not connected"));
    let r = p.list.rows.iter().find(|r| r.place == Place::MusicAssistant).unwrap();
    assert_eq!(r.text.as_deref(), Some("Not connected"));
    assert_button(r.link.as_ref(), Some(("Why?", Command::Open(Open::MusicAssistantSettings))));
    match p.about {
        About::Note(n) => assert_eq!((n.title.as_str(), n.sentence.as_str()), ("Music Assistant isn’t connected", "This PC’s sound isn’t heard until it connects.")),
        v => panic!("{v:?}"),
    }
}

#[test]
fn rows_show_the_state_and_its_actions() {
    let mut s = pc();
    for (state, word, spoken, mark, why, grey) in [
        (RowState::Plain, None, "Kitchen", None, false, false),
        (
            RowState::Connecting,
            Some("Connecting…"),
            "Kitchen, connecting…",
            Some(Mark::Busy),
            false,
            false,
        ),
        (
            RowState::NotResponding,
            Some("Not responding"),
            "Kitchen, not responding",
            Some(Mark::Warn),
            true,
            false,
        ),
        (
            RowState::NotFound,
            Some("Not found"),
            "Kitchen, not found",
            None,
            true,
            true,
        ),
        (
            RowState::PlayingElse,
            Some("Playing something else"),
            "Kitchen, playing something else",
            None,
            false,
            false,
        ),
        (
            RowState::MayBeInUse,
            Some("May be in use"),
            "Kitchen, may be in use",
            None,
            false,
            false,
        ),
        (
            RowState::RepairFirst,
            Some("Repair first"),
            "Kitchen, repair first",
            None,
            false,
            true,
        ),
    ] {
        s.situation.speakers[0].state = state;
        let r = &view(&s).list.rows[0];
        assert_eq!(
            (r.text.as_deref(), r.spoken.as_str(), r.mark, r.grey),
            (word, spoken, mark, grey)
        );
        assert_eq!(r.command, (!grey).then(|| switch_to(kitchen())));
        assert_eq!(r.tooltip.as_deref(), (!grey).then_some("Switch to Kitchen"));
        assert_button(
            r.link.as_ref(),
            why.then_some(("Why?", Command::Open(Open::SpeakerMissing))),
        );
        if why {
            assert_eq!(
                r.link.as_ref().unwrap().spoken.as_deref(),
                Some("Why? Opens “Speaker missing?” for Kitchen")
            );
        }
        assert_button(
            Some(&r.gear),
            Some((
                "Kitchen’s settings: tone, delay, name",
                Command::Open(Open::SpeakerSettings(kitchen())),
            )),
        );
    }
    s.situation.speakers[0].state = RowState::Plain;
    s.speakers[0].ways.push(way(WayChoice::AirPlaySteady, 2000));
    assert_eq!(
        view(&s).list.rows[0].tooltip.as_deref(),
        Some("Switch to Kitchen by Sonos")
    );
    s.speakers[0].way = WayChoice::AirPlaySteady;
    assert_eq!(
        view(&s).list.rows[0].tooltip.as_deref(),
        Some("Switch to Kitchen by AirPlay")
    );
    s.situation.spot = Spot::Speaker(kitchen());
    assert!(view(&s).list.rows[0].command.is_none());
    s.situation.picking = Some(pick(id("other")));
    let r = &view(&s).list.rows[0];
    assert!(r.marked && r.tooltip.is_none());
    assert_eq!(r.command, Some(Command::CancelSwitch));
    s.situation.picking.as_mut().unwrap().can_cancel = false;
    assert!(view(&s).list.rows[0].command.is_none());
}

#[test]
fn names_folds_and_search() {
    let mut s = pc();
    let mut twin = seen(id("twin"), "Kitchen");
    twin.address = Some("192.168.1.8".into());
    s.speakers[0].address = Some("192.168.1.7".into());
    s.speakers.push(twin);
    let rows = view(&s).list.rows;
    assert_eq!(rows[0].name, "Kitchen (192.168.1.7)");
    assert_eq!(rows[1].name, "Kitchen (192.168.1.8)");
    assert_eq!(by_clock::tray(&s).menu[4].label, rows[1].name);
    s.speakers.pop();
    assert_eq!(view(&s).list.rows[0].name, "Kitchen");
    for i in 1..UNFOLDED_ROWS {
        let name = format!("Room {i}");
        let mut sp = seen(id(&name), &name);
        sp.used_here = false;
        s.speakers.push(sp);
    }
    let list = view(&s).list;
    assert_eq!(list.rows.len(), 1);
    assert_eq!(list.more.unwrap().label, "More speakers (7)");
    s.speakers[0].used_here = false;
    s.speakers.truncate(6);
    let list = view(&s).list;
    assert!(list.rows.is_empty());
    assert_eq!(
        list.more.unwrap().label,
        "Speakers found on this network (6)"
    );
    s.network = Some(ConnKind::Wired);
    assert_eq!(
        view(&s).list.more.unwrap().label,
        "Speakers found on this network (6)"
    );
    s.network = Some(ConnKind::WiFi);
    assert_eq!(
        view(&s).list.more.unwrap().label,
        "Speakers found on this Wi-Fi (6)"
    );
    s.situation.spot = Spot::Speaker(kitchen());
    s.situation.picking = Some(pick(s.speakers[1].id.clone()));
    let list = view(&s).list;
    assert_eq!(list.rows.len(), 2);
    assert_eq!(list.more.unwrap().rows.len(), 4);
    s.speakers.clear();
    s.situation.picking = None;
    on_pc(&mut s);
    s.search.phase = SearchPhase::Looking;
    let search = view(&s).list.search.unwrap();
    assert!(search.spinner && search.link.is_none());
    assert_eq!(search.text, "Looking for speakers…");
    s.search.phase = SearchPhase::NoneFound;
    let search = view(&s).list.search.unwrap();
    assert!(!search.spinner);
    assert_eq!(search.text, "No speakers found on the network.");
    assert_button(
        search.link.as_ref(),
        Some(("Why?", Command::Open(Open::SpeakerMissing))),
    );
    s.speakers.push(seen(kitchen(), "Kitchen"));
    assert!(view(&s).list.search.is_none());
}

#[test]
fn an_output_goes_by_its_own_name_until_another_output_shares_it() {
    let mut s = scene();
    s.outputs.push(output("Headphones", OutputKind::Headphones));
    on_pc(&mut s);
    s.outputs_open = true;
    let names = |s: &Scene| view(s).list.this_pc.outputs.iter().map(|o| o.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&s), ["Speakers", "Headphones"]);
    assert_eq!(view(&s).place.output.as_deref(), Some("Speakers"));
    assert_eq!(view(&s).list.this_pc.output, "Speakers");
    // A second "Speakers": Windows' full names tell the two apart, and
    // the other output keeps its own.
    let mut usb = output("USB", OutputKind::Speakers);
    usb.name = "Speakers".into();
    usb.friendly = "Speakers (USB Audio)".into();
    s.outputs.push(usb);
    assert_eq!(names(&s), ["Speakers (Realtek Audio)", "Headphones", "Speakers (USB Audio)"]);
    assert_eq!(view(&s).place.output.as_deref(), Some("Speakers (Realtek Audio)"));
    assert_eq!(view(&s).list.this_pc.output, "Speakers (Realtek Audio)");
    // The full name is what hovering shows.
    assert_eq!(view(&s).list.this_pc.outputs[1].tooltip, "Headphones (Realtek Audio)");
    // An output Windows gives no name of its own goes by the full one.
    s.outputs[1].name.clear();
    assert_eq!(names(&s)[1], "Headphones (Realtek Audio)");
}

#[test]
fn outputs_and_volume_belong_to_the_place() {
    let mut s = scene();
    let p = view(&s);
    assert_eq!(
        (
            p.list.this_pc.output.as_str(),
            p.list.this_pc.arrow,
            p.list.this_pc.marked
        ),
        ("Speakers", None, false)
    );
    assert_eq!(p.list.this_pc.command, Some(to_pc()));
    assert_eq!(p.list.this_pc.tooltip.as_deref(), Some("Switch to this PC"));
    let v = p.place.volume.unwrap();
    assert_eq!(v.owner, Some(VolumeOwner::Speaker(kitchen())));
    assert_eq!(v.spoken, "Kitchen volume");
    assert_eq!(v.mute_spoken, "Mute Kitchen");
    assert_eq!(v.number, "35");
    s.outputs.push(output("Headphones", OutputKind::Headphones));
    on_pc(&mut s);
    assert_eq!(
        view(&s).list.this_pc.arrow.unwrap().command,
        Command::ShowPcOutputs(true)
    );
    s.outputs_open = true;
    s.volume.pc = Some(Level {
        value: 42,
        muted: true,
        limit: Some(80),
    });
    let p = view(&s);
    let row = p.list.this_pc;
    assert!(row.marked && row.command.is_none());
    assert_button(
        row.arrow.as_ref(),
        Some(("This PC’s outputs", Command::ShowPcOutputs(false))),
    );
    assert_eq!(row.outputs.len(), 2);
    assert!(row.outputs[0].marked && !row.outputs[1].marked);
    for (row, output) in row.outputs.iter().zip(&s.outputs) {
        assert_eq!(row.name, output.name);
        assert_eq!(row.tooltip, output.friendly);
        assert_eq!(row.command, Command::SwitchOutput(output.id.clone()));
    }
    let v = p.place.volume.unwrap();
    assert_eq!(
        v.owner,
        Some(VolumeOwner::PcOutput(s.outputs[0].id.clone()))
    );
    assert_eq!(
        (v.level, v.muted, v.limit, v.number.as_str()),
        (42, true, Some(80), "42")
    );
    assert_eq!(v.mute_spoken, "Unmute This PC");
    let ma = SpeakerId::music_assistant();
    s.speakers.push(seen(ma.clone(), "ignored"));
    s.situation.spot = Spot::Speaker(ma.clone());
    let p = view(&s);
    assert_eq!(p.place.name, "Music Assistant");
    assert_eq!(
        p.place.volume.unwrap().owner,
        Some(VolumeOwner::MusicAssistant)
    );
    assert_eq!(
        p.list.rows[1].gear.command,
        Command::Open(Open::MusicAssistantSettings)
    );
    on_pc(&mut s);
    assert_eq!(
        view(&s).list.rows[1].command,
        Some(Command::Switch {
            place: Place::MusicAssistant,
            way: None
        })
    );
    s.situation.pc_output = None;
    on_pc(&mut s);
    s.outputs.clear();
    s.volume.pc = None;
    let p = view(&s);
    assert!(p.place.volume.is_none() && p.list.this_pc.arrow.is_none());
    assert_eq!(p.list.this_pc.output, "No output connected");
    assert_eq!(
        p.links.map(|a| (a.label, a.command)),
        [
            ("Can’t hear it?".into(), Command::Open(Open::CantHear(None))),
            (
                "Speaker missing?".into(),
                Command::Open(Open::SpeakerMissing)
            ),
            ("Settings".into(), Command::Open(Open::Settings))
        ]
    );
}

#[test]
fn no_speaker_chosen_names_the_list_in_the_panel_and_the_icon_in_the_popup() {
    let mut scene = named("pc-plain");
    scene.situation.popups.popup = Some(shown(Popup::NoSpeakerChosen));
    let popup = by_clock::popup(&scene).unwrap().what;
    scene.situation.standing.push(Standing::NoSpeakerChosen);
    let About::Note(note) = view(&scene).about else { panic!("expected the panel note") };
    assert_eq!(note.title, popup.title);
    assert_eq!(note.sentence, "The sound is still on this PC. Choose a speaker in the list.");
    assert_eq!(popup.sentence.as_deref(), Some("The sound is still on this PC. Click the app’s icon by the clock to choose a speaker."));
    assert_eq!(note.close.unwrap().command, Command::CloseNote);
}

#[test]
fn popup_words_and_actions() {
    let mut s = scene();
    assert!(by_clock::popup(&s).is_none());
    for (popup, title, sentence, button) in [
        (Popup::PlayingOn { speaker: kitchen() }, "Playing on Kitchen", None, Some(("Switch to this PC", to_pc()))),
        (Popup::AlsoPlayingIn { room: "Bedroom".into() }, "Now also playing in Bedroom", Some("Bedroom was grouped with Kitchen in the Sonos app."), None),
        (Popup::DidntGetReady { speaker: kitchen(), why: PickWhy::Refused }, "Kitchen refused the connection", Some("The sound is still on Kitchen."), None),
        (Popup::NoSpeakerChosen, "No speaker chosen", Some("The sound is still on Kitchen. Click the app’s icon by the clock to choose a speaker."), None),
        (Popup::CallOn { speaker: kitchen() }, "A microphone is in use", Some("If this is a call, you will hear the other person late on Kitchen."), Some(("Switch to this PC", to_pc()))),
    ] {
        s.situation.popups.popup = Some(shown(popup)); view(&s);
        let p = by_clock::popup(&s).unwrap(); assert_eq!((p.seq,p.since_ms), (7,2000));
        assert_eq!((p.what.title.as_str(), p.what.sentence.as_deref()), (title, sentence)); assert_button(p.what.button.as_ref(), button); assert_eq!(p.what.close.command, Command::ClosePopup);
    }
}
#[test]
fn confirmations_share_words_and_answers() {
    let mut s = pc();
    let k = kitchen();
    let way = WayChoice::Native;
    assert!(by_clock::confirmation(&s).is_none());
    for (confirm, title, body, yes) in [
        (Confirm::ChimeBusy { speaker: k.clone(), who: None }, "Kitchen is playing something else", "A test chime stops it, and it may not start again by itself.", "Play a test chime"),
        (Confirm::ChimeBusy { speaker: k.clone(), who: Some("LAPTOP-7Q2K".into()) }, "Kitchen is playing LAPTOP-7Q2K’s sound", "A test chime stops it, and it may not start again by itself.", "Play a test chime"),
        (Confirm::Busy { speaker: k.clone(), way, who: None }, "Kitchen is playing something else", "Switching to Kitchen stops what it is playing.", "Switch to Kitchen"),
        (Confirm::Busy { speaker: k.clone(), way, who: Some("LAPTOP-7Q2K".into()) }, "Kitchen is playing LAPTOP-7Q2K’s sound", "Switching to Kitchen stops what it is playing.", "Switch to Kitchen"),
        (Confirm::MayBeInUse { speaker: k.clone(), way }, "Kitchen may be in use", "The app can’t tell whether another device is playing on Kitchen. Switching to Kitchen stops anything playing on it.", "Switch to Kitchen"),
        (Confirm::ScreenReader { speaker: k.clone(), way }, "Your screen reader’s voice would play on Kitchen too", "It would be late there. To keep it on this PC, choose this PC’s output in the screen reader’s own settings.", "Switch to Kitchen"),
    ] {
        let chime = matches!(confirm, Confirm::ChimeBusy { .. }); s.situation.confirm_in_panel = Some(confirm.clone()); s.situation.popups.confirmation = Some(shown(confirm));
        let About::Question(q) = view(&s).about else { panic!() };
        assert_eq!((q.title.as_str(),q.sentence.as_str()), (title,body)); assert_button(Some(&q.yes), Some((yes,Command::Confirm { yes:true })));
        assert_button(Some(&q.no), Some(("Cancel",Command::Confirm { yes:false })));
        let clock = by_clock::confirmation(&s).unwrap(); assert_eq!((clock.seq, clock.since_ms), (7,2000));
        assert_eq!(clock.what, QuestionView { sentence: if chime { body.into() } else { format!("{body} Until you switch, the sound stays on this PC.") }, ..q });
    }
    s.situation.confirm_in_panel = Some(Confirm::ScreenReader { speaker: k, way });
    s.situation.popups.confirmation = None;
    for (routing, yes) in [(RoutingQuestion::StartSwitch, "Switch to Kitchen"), (RoutingQuestion::TakePlayback, "Play this PC’s sound on Kitchen")] {
        s.situation.confirm_under_routing = Some(routing);
        let About::Question(q) = view(&s).about else { panic!() };
        assert_button(Some(&q.yes), Some((yes, Command::Confirm { yes: true })));
        assert_button(Some(&q.no), Some(("Cancel", Command::Confirm { yes: false })));
    }
}

#[test]
fn tray_and_window_words() {
    let mut s = scene();
    s.speakers[0].last_used = Some(15);
    s.situation.amber = true;
    for i in 0..10 {
        let name = format!("Room {i}");
        let mut sp = seen(id(&name), &name);
        sp.used_here = i % 2 == 0;
        sp.last_used = Some(i);
        let state = match i {
            6 => RowState::NotFound,
            4 => RowState::RepairFirst,
            _ => RowState::Plain,
        };
        s.situation.speakers.push(SpeakerNow {
            can_chime: true,
            id: sp.id.clone(),
            is_place: false,
            way: sp.way,
            state,
        });
        s.speakers.push(sp);
    }
    view(&s);
    let tray = by_clock::tray(&s);
    assert_eq!(tray.tooltip, "Stream To Speaker – Kitchen · Playing");
    assert!(tray.accent && tray.amber);
    let labels: Vec<_> = tray.menu.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Stream To Speaker",
            "Open",
            "This PC",
            "Kitchen",
            "Room 8",
            "Room 2",
            "Room 0",
            "Room 9",
            "Room 7",
            "Room 5",
            "Room 3",
            "More speakers…",
            "Settings",
            "Quit, and play the sound on this PC"
        ]
    );
    assert_eq!(tray.menu.iter().filter(|e| e.checked).count(), 1);
    assert!(tray.menu[3].checked);
    assert_eq!(tray.menu[0].command, None);
    assert_eq!(tray.menu[1].command, Some(Command::Open(Open::Panel)));
    assert_eq!(tray.menu[2].command, Some(to_pc()));
    assert_eq!(tray.menu[3].command, None);
    assert_eq!(tray.menu[4].command, Some(switch_to(id("Room 8"))));
    assert_eq!(tray.menu[11].command, Some(Command::Open(Open::Panel)));
    assert_eq!(tray.menu[12].command, Some(Command::Open(Open::Settings)));
    assert_eq!(tray.menu[13].command, Some(Command::Quit));
    let w = by_clock::window_words(&s);
    assert_eq!(w.strip, "Closing this window doesn’t stop the sound. This icon by the clock opens the same controls.");
    assert_eq!(w.title, "The app is still running");
    assert_eq!(
        w.sentence,
        "Its icon is by the clock, or under the ^ arrow there."
    );
    assert_button(
        Some(&w.quit),
        Some(("Quit, and play the sound on this PC", Command::Quit)),
    );
    on_pc(&mut s);
    s.speakers.truncate(1);
    s.situation.amber = false;
    view(&s);
    let tray = by_clock::tray(&s);
    assert!(!tray.accent && !tray.amber);
    assert_eq!(tray.tooltip, "Stream To Speaker – This PC · Playing");
    assert!(tray.menu[2].checked && tray.menu[2].command.is_none());
    assert_eq!(tray.menu.last().unwrap().label, "Quit");
    assert_button(
        Some(&by_clock::window_words(&s).quit),
        Some(("Quit the app", Command::Quit)),
    );
}

/// Exercise the same scenes on the thread recording the sentence keys.
#[test]
fn notes_follow_the_apps_sent_to_the_speaker() {
    let mut s = pc();
    s.situation.only_some_apps = true;
    let mut connecting = named("pc-connecting-to-kitchen");
    connecting.situation.only_some_apps = true;
    assert!(matches!(panel(&connecting).about, About::Line { text, .. } if text == "Connecting to Kitchen…"));
    let mut n = note(stopped());
    for (ahead, words) in [
        (Some(Ahead::InAFewSeconds), "The sound from your chosen apps moves to this PC in a few seconds."),
        (Some(Ahead::WhenSoundStarts), "Your chosen apps will play on this PC when they make sound."),
        (None, "The sound from your chosen apps moved to this PC at 21:14."),
    ] {
        n.ahead = ahead;
        assert_eq!(super::notes::note(&s, &n).sentence, words);
    }
    s.situation.popups.popup = Some(shown(Popup::Note(n.clone())));
    assert_eq!(by_clock::popup(&s).unwrap().what.sentence.as_deref(), Some("Your chosen apps now play on this PC."));
    s.situation.popups.popup = Some(shown(Popup::DidntGetReady { speaker: kitchen(), why: PickWhy::Refused }));
    assert_eq!(by_clock::popup(&s).unwrap().what.sentence.as_deref(), Some("The apps you choose play on this PC."));
    n.available_again = true;
    assert_eq!(super::notes::note(&s, &n).sentence, "The apps you choose play on this PC.");
}

pub(super) fn check_all_scenes() {
    notes_follow_the_apps_sent_to_the_speaker();
    sound_plays_on_kitchen();
    nothing_plays_on_kitchen();
    sound_starts_and_connects();
    start_has_waited_two_seconds();
    clicked_a_row();
    speaker_away_while_quiet();
    headphones_connected();
    headphones_disconnected();
    pc_without_an_output();
    every_cause_and_form_has_the_rules_button_mark_and_close();
    status_lines_follow_precedence_and_link_to_the_right_page();
    lasting_notes_and_repairs();
    figures_ways_and_a_press();
    delay_limits_fixed_progress_and_dimmed_states();
    rows_show_the_state_and_its_actions();
    names_folds_and_search();
    outputs_and_volume_belong_to_the_place();
    no_speaker_chosen_names_the_list_in_the_panel_and_the_icon_in_the_popup();
    popup_words_and_actions();
    confirmations_share_words_and_answers();
    tray_and_window_words();
}

#[test]
fn a_move_names_its_time_only_when_the_core_supplies_one() {
    let mut s = pc();
    let vpn = |name| Cause::StoppedResponding {
        speaker: kitchen(),
        vpn: Some(NetJoined {
            name,
            at: LocalTime::new(21, 13),
        }),
    };
    for (cause, title, body) in [
        (stopped(), "Kitchen stopped responding", "The app moved the sound to this PC at 08:02."),
        (Cause::WayFailed { speaker: kitchen(), why: PickWhy::Refused }, "Kitchen refused the connection", "The app moved the sound to this PC at 08:02."),
        (vpn(Some("Corp VPN".into())), "Kitchen stopped responding", "The app moved the sound to this PC at 08:02, just after this PC connected to “Corp VPN”."),
        (vpn(None), "Kitchen stopped responding", "The app moved the sound to this PC at 08:02, just after this PC made a new network connection."),
        (Cause::GroupedInto { speaker: kitchen(), with: "Living Room".into() }, "Kitchen was grouped with Living Room", "The app moved the sound to this PC at 08:02. Kitchen can play it again once it leaves that group."),
        (Cause::SwitchedAfterQuiet { from: kitchen() }, "Nothing played for 30 minutes", "The app moved the sound from Kitchen to this PC at 08:02."),
        (Cause::HeadphonesGone { before: kitchen(), name: "Headphones".into() }, "Headphones disconnected", "The sound moved to this PC’s speakers at 08:02."),
        (Cause::Removed { speaker: kitchen() }, "You removed Kitchen", "The app moved the sound to this PC."),
    ] {
        for at in [Some(LocalTime::new(8,2)), None] {
            s.situation.note = Some(Note { at, ..note(cause.clone()) });
            let About::Note(v) = view(&s).about else { panic!() };
            assert_eq!(v.title,title);
            assert_eq!(v.sentence, if at.is_some() { body.into() } else { body.replace(" at 08:02", "") });
        }
    }
    s.lang = Lang::Test;
    s.situation.note = Some(Note {
        at: Some(LocalTime::new(8, 2)),
        ..note(stopped())
    });
    let About::Note(v) = view(&s).about else {
        panic!()
    };
    assert_eq!(v.sentence, "The app moved the sound to this PC at 8.02.");
}

#[test]
fn a_speaker_on_its_own_way_can_be_switched_to_the_airplay_way_it_offers() {
    let mut s = seen(kitchen(), "Kitchen");
    assert_eq!(s.airplay_way(), None, "it offers no AirPlay way");
    s.ways.push(way(WayChoice::AirPlayShort, 300));
    assert_eq!(s.airplay_way(), Some(WayChoice::AirPlayShort));
    // The steadier one of the two.
    s.ways.push(way(WayChoice::AirPlaySteady, 2_000));
    assert_eq!(s.airplay_way(), Some(WayChoice::AirPlaySteady));
    // The one it last used by AirPlay, while it is offered.
    s.airplay = Some(WayChoice::AirPlayShort);
    assert_eq!(s.airplay_way(), Some(WayChoice::AirPlayShort));
    s.ways.retain(|w| w.way != WayChoice::AirPlayShort);
    assert_eq!(s.airplay_way(), Some(WayChoice::AirPlaySteady));
    // On AirPlay already: there is nothing to switch to.
    s.way = WayChoice::AirPlaySteady;
    assert_eq!(s.airplay_way(), None);
}

#[test]
fn a_stale_repair_result_and_a_missing_name() {
    let mut s = pc();
    s.repair = RepairNow::Worked;
    s.situation.standing.push(Standing::OutputMissing);
    assert_note(
        view(&s).about,
        Mark::Warn,
        "The app’s sound output is missing",
        "The sound stays on this PC. Repair puts it back; it needs administrator approval.",
        Some(("Repair", Command::RepairOutput)),
        false,
    );
    s.repair = RepairNow::NotAsked;
    s.situation.standing.clear();
    s.speakers.clear();
    s.situation.note = Some(note(Cause::Removed { speaker: kitchen() }));
    assert_note(
        view(&s).about,
        Mark::Info,
        "You removed Speaker",
        "The app moved the sound to this PC.",
        None,
        true,
    );
}

const NEVER: &[&str] = &[
    "streaming enabled",
    "standing by",
    "resync",
    "rescan",
    "latency",
    "timing",
    "trim",
    "drain",
    "in sync",
    "packet",
    "listener",
    "ppm",
    "upnp",
    "raop",
    "et=4",
    "coordinator",
    "0.0.0.0",
    "earlier",
    "airplay 2",
    "buffer",
    "error",
];

fn has_address(s: &str) -> bool {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.')).any(|w| {
        let parts: Vec<&str> = w.split('.').collect();
        parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3)
    })
}

/// `twins`: the names that may carry an address, because two entries share
/// them.
pub(super) fn check_words_allowing(what: &str, kind: &str, text: &str, twins: &[String]) {
    assert!(!text.trim().is_empty() || kind == "delay button", "{what}: empty {kind}");
    assert!(!text.contains('{') && !text.contains('}'), "{what}: unfilled placeholder in {kind}: {text}");
    let lower = text.to_lowercase();
    for w in NEVER {
        assert!(!lower.contains(w), "{what}: “{w}” in {kind}: {text}");
    }
    assert!(!lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == "pad" || w == "later"), "{what}: {text}");
    if has_address(text) {
        assert!(twins.iter().any(|t| text.contains(t.as_str())), "{what}: an address in {kind}: {text}");
    }
}
