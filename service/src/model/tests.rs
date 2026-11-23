//! Tests across the whole model: the tables of states and words cell by
//! cell, and a catalogue that renders every state on every surface.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::command::*;
use super::delay::*;
use super::delay::PanelDelay;
use super::facts::*;
use super::places::{Row, RowStatus};
use super::port::Port;
use super::scripted::{Names, Scenario, ScriptedEngine};
use super::snapshot::{build, Button, ButtonStyle, Snapshot, Third, TitleIcon, Tone};
use super::state::*;
use super::surfaces::{self, MenuEntry};
use super::text::{Key, Lang, LocalTime, USED};

const EN: Lang = Lang::English;

fn kitchen() -> SpeakerId {
    SpeakerId::new("kitchen")
}

fn snap(s: Scenario) -> Arc<Snapshot> {
    ScriptedEngine::new(s).snapshot()
}

/// A scenario's facts, changed by `f`, built into a snapshot.
fn snap_with(s: Scenario, f: impl FnOnce(&mut Facts)) -> Snapshot {
    let mut facts = ScriptedEngine::new(s).facts();
    f(&mut facts);
    build(&facts, 1)
}

/// A card's buttons as text: "Label*" for the primary, "Label~" for a link.
fn labels(buttons: &[Button]) -> Vec<String> {
    buttons
        .iter()
        .map(|b| match b.style {
            ButtonStyle::Primary => format!("{}*", b.label),
            ButtonStyle::Link => format!("{}~", b.label),
            _ => b.label.clone(),
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Vol {
    None,
    Pc(u8),
    Speaker(u8),
    Ma(u8),
}

fn vol(s: &Snapshot) -> Vol {
    match &s.now.volume {
        None => Vol::None,
        Some(v) => match v.owner {
            VolumeOwner::PcOutput(_) => Vol::Pc(v.level),
            VolumeOwner::Speaker(_) => Vol::Speaker(v.level),
            VolumeOwner::MusicAssistant => Vol::Ma(v.level),
        },
    }
}

fn set_state(state: NowState) -> impl FnOnce(&mut Facts) {
    move |f| f.state = state
}

/// Every state of the Now card, in exact words: title, line 2, actions,
/// volume and delay rows.
#[test]
fn now_card_in_every_state() {
    let t = |h, m| LocalTime::new(h, m);
    let k = kitchen();
    let rows: Vec<(&str, Snapshot, &str, &str, Vec<&str>, Vol, bool)> = vec![
        (
            "on this PC",
            (*snap(Scenario::OnThisPc)).clone(),
            "Sound is on this PC",
            "Choose a speaker below to play everything there.",
            vec![],
            Vol::Pc(40),
            false,
        ),
        (
            "connecting from this PC",
            (*snap(Scenario::ConnectingFromPc)).clone(),
            "Connecting to Kitchen…",
            "Your sound stays on this PC until Kitchen is ready.",
            vec!["Cancel"],
            Vol::Pc(40),
            false,
        ),
        (
            "switching",
            snap_with(
                Scenario::Connecting,
                set_state(NowState::Connecting {
                    target: SpeakerId::new("living"),
                    from: SoundAt::Speaker(k.clone()),
                    stage: ConnectStage::Asking,
                }),
            ),
            "Connecting to Living Room + Dining Room…",
            "Your sound stays on Kitchen until Living Room + Dining Room is ready.",
            vec!["Cancel"],
            Vol::Speaker(32),
            false,
        ),
        (
            "playing",
            (*snap(Scenario::Playing)).clone(),
            "Playing on Kitchen",
            "Spotify is playing",
            vec![],
            Vol::Speaker(32),
            true,
        ),
        (
            "playing, several apps",
            snap_with(
                Scenario::Playing,
                set_state(NowState::Playing { speaker: k.clone(), apps: Apps::of(&["Spotify", "Edge"]) }),
            ),
            "Playing on Kitchen",
            "Spotify and Edge are playing",
            vec![],
            Vol::Speaker(32),
            true,
        ),
        (
            "playing, app unknown",
            snap_with(Scenario::Playing, set_state(NowState::Playing { speaker: k.clone(), apps: Apps::default() })),
            "Playing on Kitchen",
            "Sound is playing",
            vec![],
            Vol::Speaker(32),
            true,
        ),
        (
            "playing, muted",
            (*snap(Scenario::Muted)).clone(),
            "Playing on Kitchen",
            "Spotify is playing · Kitchen is muted",
            vec![],
            Vol::Speaker(32),
            true,
        ),
        (
            "applying a delay change by reconnecting",
            snap_with(
                Scenario::ApplyingDelay,
                set_state(NowState::ApplyingDelay { speaker: SpeakerId::new("office"), from_ms: 0, to_ms: -100 }),
            ),
            "Playing on Office",
            "Reconnecting to remove 100 ms: a few seconds of silence.",
            vec![],
            Vol::Speaker(30),
            true,
        ),
        (
            "ready",
            (*snap(Scenario::Ready)).clone(),
            "Kitchen is ready",
            "Nothing on this PC is playing right now.",
            vec![],
            Vol::Speaker(32),
            true,
        ),
        (
            "ready, an app plays elsewhere",
            (*snap(Scenario::ReadyElsewhere)).clone(),
            "Kitchen is ready",
            "Chrome is playing on Speakers (Realtek Audio), not here. Some apps move to a new output only after you restart them.",
            vec!["Open Volume mixer"],
            Vol::Speaker(32),
            true,
        ),
        (
            "not taking the sound",
            (*snap(Scenario::NotTaking)).clone(),
            "Kitchen isn’t taking the sound",
            "This PC is sending sound, but Kitchen stopped asking for it at 21:14.",
            vec!["Reconnect*", "What else can I try?"],
            Vol::Speaker(32),
            true,
        ),
        (
            "not responding",
            (*snap(Scenario::NotResponding)).clone(),
            "Kitchen isn’t responding",
            "Trying again (2 of 3). If Kitchen doesn’t respond within 15 seconds, your sound comes back to this PC.",
            vec!["Play on this PC now"],
            Vol::None,
            false,
        ),
        (
            "not responding, staying silent",
            (*snap(Scenario::StaySilent)).clone(),
            "Kitchen isn’t responding",
            "Trying again every 30 seconds. Nothing plays on this PC until Kitchen is back.",
            vec!["Play on this PC"],
            Vol::None,
            false,
        ),
        (
            "fell back",
            (*snap(Scenario::FellBack)).clone(),
            "Sound is back on this PC",
            "Kitchen stopped responding at 21:14, so your sound came back to Speakers (Realtek Audio).",
            vec!["Try Kitchen again*", "Stay on this PC"],
            Vol::Pc(40),
            false,
        ),
        (
            "speaker back after a dropout",
            (*snap(Scenario::SpeakerBack)).clone(),
            "Kitchen is back",
            "Your sound stayed on this PC because something played here after Kitchen stopped responding.",
            vec!["Play on Kitchen*", "Stay on this PC"],
            Vol::Pc(40),
            false,
        ),
        (
            "speaker back at sign-in",
            snap_with(
                Scenario::SpeakerBack,
                set_state(NowState::SpeakerBack { speaker: k.clone(), because: StayedBecause::PlayingWhenFound }),
            ),
            "Kitchen is back",
            "Your sound stayed on this PC because something was playing here when Kitchen was found.",
            vec!["Play on Kitchen*", "Stay on this PC"],
            Vol::Pc(40),
            false,
        ),
        (
            "taken",
            (*snap(Scenario::Taken)).clone(),
            "Sound is back on this PC",
            "Kitchen started playing something else at 21:40, so your sound came back to Speakers (Realtek Audio).",
            vec!["Play on Kitchen again*", "Stay on this PC"],
            Vol::Pc(40),
            false,
        ),
        (
            "needs pairing",
            (*snap(Scenario::NeedsPairing)).clone(),
            "Den TV needs pairing",
            "Den TV wants a one-time code before it can play your sound. Your sound stays on this PC until then.",
            vec!["Pair…*", "Stay on this PC"],
            Vol::Pc(40),
            false,
        ),
        (
            "looking for the speaker",
            (*snap(Scenario::LookingForSpeaker)).clone(),
            "Sound is on this PC",
            "Looking for Kitchen…",
            vec![],
            Vol::Pc(40),
            false,
        ),
        (
            "can't find it",
            (*snap(Scenario::CantFind)).clone(),
            "Can’t find Kitchen right now",
            "Your sound stays on this PC. It goes to Kitchen when Kitchen is found, unless something plays here first.",
            vec!["Stay on this PC", "Speaker missing?"],
            Vol::Pc(40),
            false,
        ),
        (
            "can't find it, something played",
            snap_with(
                Scenario::CantFind,
                set_state(NowState::LookingFor { speaker: k.clone(), looking: Looking::NotFound(Return::WillAsk) }),
            ),
            "Can’t find Kitchen right now",
            "Your sound stays on this PC. When Kitchen is found, you’ll be asked.",
            vec!["Stay on this PC", "Speaker missing?"],
            Vol::Pc(40),
            false,
        ),
        (
            "Windows switched",
            (*snap(Scenario::WindowsSwitched)).clone(),
            "Sound is on Headphones",
            "Windows switched its output to Headphones (WH-1000XM4), so Stream To Speaker let go of Kitchen at 21:50.",
            vec!["Play on Kitchen"],
            Vol::Pc(24),
            false,
        ),
        (
            "choose a speaker",
            (*snap(Scenario::ChooseSpeaker)).clone(),
            "Where should Stream To Speaker play?",
            "Pick a speaker. If you don’t, your sound goes back to Speakers (Realtek Audio).",
            vec![],
            Vol::None,
            false,
        ),
        (
            "Windows refused",
            (*snap(Scenario::WindowsRefused)).clone(),
            "Windows didn’t switch to Kitchen",
            "Windows kept Speakers (Realtek Audio) as its output. In Sound settings, choose “Kitchen (Stream To Speaker)”.",
            vec!["Open Sound settings*"],
            Vol::Speaker(32),
            true,
        ),
        (
            "Music Assistant playing",
            (*snap(Scenario::MaPlaying)).clone(),
            "Playing through Music Assistant",
            "Edge is playing · Music Assistant plays it in Living Room.",
            vec![],
            Vol::Ma(70),
            false,
        ),
        (
            "Music Assistant playing, room unknown",
            snap_with(
                Scenario::MaPlaying,
                set_state(NowState::MaPlaying { apps: Apps::of(&["Edge"]), room: None }),
            ),
            "Playing through Music Assistant",
            "Edge is playing · Music Assistant chooses the speaker.",
            vec![],
            Vol::Ma(70),
            false,
        ),
        (
            "Music Assistant waiting",
            (*snap(Scenario::MaWaiting)).clone(),
            "Waiting for Music Assistant to play it",
            "Music Assistant has this PC’s sound but hasn’t started playing it. Start it there, or set it to start by itself.",
            vec!["How?", "Play on this PC"],
            Vol::None,
            false,
        ),
        (
            "Music Assistant away",
            (*snap(Scenario::MaAway)).clone(),
            "Music Assistant isn’t connected",
            "Your sound is on this PC until Music Assistant is back. It goes back to Music Assistant when it connects, unless something plays here first.",
            vec!["Stay on this PC", "Music Assistant settings"],
            Vol::Pc(40),
            false,
        ),
        (
            "Music Assistant away, something played",
            snap_with(Scenario::MaAway, set_state(NowState::MaAway { returns: Return::WillAsk })),
            "Music Assistant isn’t connected",
            "Your sound is on this PC until Music Assistant is back. When Music Assistant is back, you’ll be asked.",
            vec!["Stay on this PC", "Music Assistant settings"],
            Vol::Pc(40),
            false,
        ),
        (
            "output missing",
            (*snap(Scenario::OutputMissing)).clone(),
            "The Stream To Speaker sound output is missing",
            "Windows doesn’t have the output this app plays through, so your sound stays on this PC. Repair puts it back; it needs administrator approval.",
            vec!["Repair*"],
            Vol::Pc(40),
            false,
        ),
        (
            "only apps sent there, playing",
            (*snap(Scenario::SentApps)).clone(),
            "Playing Spotify on Kitchen",
            "Only apps you send here play on Kitchen; everything else stays on this PC.",
            vec!["Choose apps…~"],
            Vol::Speaker(32),
            true,
        ),
        (
            "only apps sent there, none sent",
            (*snap(Scenario::NoAppSent)).clone(),
            "Kitchen is ready",
            "No app is sending sound here. In Windows’ Volume mixer, set an app’s output to “Kitchen (Stream To Speaker)”.",
            vec!["Open Volume mixer"],
            Vol::Speaker(32),
            true,
        ),
    ];
    for (what, s, title, line2, actions, volume, delay) in rows {
        assert_eq!(s.now.title, title, "{what}: title");
        assert_eq!(s.now.line2, line2, "{what}: line 2");
        assert_eq!(labels(&s.now.actions), actions, "{what}: actions");
        assert_eq!(vol(&s), volume, "{what}: volume");
        assert_eq!(s.now.delay.is_some(), delay, "{what}: delay control");
    }

    // The problem icons before the title.
    let icon = |s: Scenario| snap(s).now.icon;
    assert_eq!(icon(Scenario::Playing), TitleIcon::None);
    assert_eq!(icon(Scenario::NotTaking), TitleIcon::Warning);
    assert_eq!(icon(Scenario::FellBack), TitleIcon::Warning);
    assert_eq!(icon(Scenario::SpeakerBack), TitleIcon::None);
    assert_eq!(icon(Scenario::LookingForSpeaker), TitleIcon::None);
    assert_eq!(icon(Scenario::CantFind), TitleIcon::Warning);
    assert_eq!(icon(Scenario::OutputMissing), TitleIcon::Error);

    // The meter: only while this PC sends real sound.
    assert!(snap(Scenario::Playing).now.meter);
    assert!(snap(Scenario::MaPlaying).now.meter);
    assert!(snap(Scenario::SentApps).now.meter);
    assert!(!snap(Scenario::Ready).now.meter);
    assert!(!snap(Scenario::ApplyingDelay).now.meter, "no meter while reconnecting");

    // Muted reads "Muted"; Music Assistant's slider says what it sets.
    assert_eq!(snap(Scenario::Muted).now.volume.as_ref().unwrap().number, "Muted");
    let ma = snap(Scenario::MaPlaying);
    let v = ma.now.volume.as_ref().unwrap();
    assert_eq!(v.tooltip.as_deref(), Some("Level sent to Music Assistant"));
    assert_eq!(v.caption.as_deref(), Some("Each speaker’s own volume is set in Music Assistant."));

    // The line under the fell-back buttons promises a return only while true.
    assert_eq!(snap(Scenario::FellBack).now.note.as_deref(), Some("When Kitchen is back, you’ll be asked."));
    let by_itself = snap_with(
        Scenario::FellBack,
        set_state(NowState::FellBack {
            speaker: k.clone(),
            at: t(21, 14),
            cause: FellBackCause::StoppedResponding,
            returns: Return::ByItself,
        }),
    );
    assert_eq!(
        by_itself.now.note.as_deref(),
        Some("It goes back to Kitchen by itself when Kitchen is back, unless something plays here first.")
    );

    // The reconnect's other two forms.
    let applying = |from_ms, to_ms| {
        snap_with(
            Scenario::ApplyingDelay,
            set_state(NowState::ApplyingDelay { speaker: SpeakerId::new("office"), from_ms, to_ms }),
        )
        .now
        .line2
    };
    assert_eq!(applying(-50, 0), "Reconnecting to go back to Office’s own delay: a few seconds of silence.");
    assert_eq!(applying(-100, -50), "Reconnecting to add 50 ms: a few seconds of silence.");

    // "Only apps I send there": This PC's row says what it really does.
    let sent = snap(Scenario::SentApps);
    assert_eq!(sent.list.this_pc.title, "Stop playing on a speaker");
    assert_eq!(sent.list.this_pc.tooltip, "Apps you send here go quiet until you pick a speaker");

    // A missing output dims every speaker.
    let missing = snap(Scenario::OutputMissing);
    assert!(missing.list.speakers.iter().all(|r| r.dimmed && r.tooltip == "Repair first" && r.command.is_none()));
}

/// One state, every surface: panel title, tray icon, tooltip, Windows'
/// device name, notification, and the phone's and `status`'s line.
#[test]
fn one_state_every_surface() {
    use NotificationKind as N;
    let cases: Vec<(&str, Snapshot, &str, Arcs, Badge, &str, Option<&str>, Option<N>, &str)> = vec![
        (
            "on this PC",
            (*snap(Scenario::OnThisPc)).clone(),
            "Sound is on this PC",
            Arcs::Ink,
            Badge::None,
            "Sound is on this PC",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Sound is on this PC",
        ),
        (
            "connecting",
            (*snap(Scenario::ConnectingFromPc)).clone(),
            "Connecting to Kitchen…",
            Arcs::GreenFaint,
            Badge::None,
            "Connecting to Kitchen",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Connecting to Kitchen…",
        ),
        (
            "playing",
            (*snap(Scenario::Playing)).clone(),
            "Playing on Kitchen",
            Arcs::Green,
            Badge::None,
            "Playing on Kitchen",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Playing on Kitchen",
        ),
        (
            "applying a delay change",
            snap_with(
                Scenario::ApplyingDelay,
                set_state(NowState::ApplyingDelay { speaker: SpeakerId::new("office"), from_ms: 0, to_ms: -100 }),
            ),
            "Playing on Office",
            Arcs::Green,
            Badge::None,
            "Playing on Office (changing its delay)",
            Some("Office (Stream To Speaker)"),
            None,
            "Playing on Office (reconnecting to remove 100 ms)",
        ),
        (
            "ready",
            (*snap(Scenario::Ready)).clone(),
            "Kitchen is ready",
            Arcs::Green,
            Badge::None,
            "Kitchen is ready",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Kitchen is ready",
        ),
        (
            "not taking",
            (*snap(Scenario::NotTaking)).clone(),
            "Kitchen isn’t taking the sound",
            Arcs::Green,
            Badge::Attention,
            "Kitchen isn’t taking the sound",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Kitchen isn’t taking the sound",
        ),
        (
            "reconnecting",
            (*snap(Scenario::NotResponding)).clone(),
            "Kitchen isn’t responding",
            Arcs::Green,
            Badge::Attention,
            "Kitchen isn’t responding",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Kitchen isn’t responding",
        ),
        (
            "fell back",
            (*snap(Scenario::FellBack)).clone(),
            "Sound is back on this PC",
            Arcs::Ink,
            Badge::Attention,
            "Kitchen stopped responding. Sound is on this PC",
            Some("Kitchen (Stream To Speaker)"),
            Some(N::StoppedResponding),
            "Sound is on this PC (Kitchen stopped responding)",
        ),
        (
            "speaker back",
            (*snap(Scenario::SpeakerBack)).clone(),
            "Kitchen is back",
            Arcs::Ink,
            Badge::Attention,
            "Kitchen is back. Sound is on this PC",
            Some("Kitchen (Stream To Speaker)"),
            Some(N::SpeakerBack),
            "Sound is on this PC (Kitchen is back)",
        ),
        (
            "taken",
            (*snap(Scenario::Taken)).clone(),
            "Sound is back on this PC",
            Arcs::Ink,
            Badge::Attention,
            "Kitchen is playing something else. Sound is on this PC",
            Some("Kitchen (Stream To Speaker)"),
            Some(N::Taken),
            "Sound is on this PC (Kitchen is playing something else)",
        ),
        (
            "needs pairing",
            (*snap(Scenario::NeedsPairing)).clone(),
            "Den TV needs pairing",
            Arcs::Ink,
            Badge::Attention,
            "Den TV needs pairing",
            Some("Den TV (Stream To Speaker)"),
            Some(N::NeedsSecret(Secret::Code)),
            "Den TV needs pairing — pair it on the PC",
        ),
        (
            "looking",
            (*snap(Scenario::LookingForSpeaker)).clone(),
            "Sound is on this PC",
            Arcs::Ink,
            Badge::None,
            "Looking for Kitchen",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Looking for Kitchen…",
        ),
        (
            "can't find",
            (*snap(Scenario::CantFind)).clone(),
            "Can’t find Kitchen right now",
            Arcs::Ink,
            Badge::Attention,
            "Can’t find Kitchen",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Can’t find Kitchen",
        ),
        (
            "Windows switched",
            (*snap(Scenario::WindowsSwitched)).clone(),
            "Sound is on Headphones",
            Arcs::Ink,
            Badge::None,
            "Sound is on Headphones",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Sound is on Headphones",
        ),
        (
            "choose a speaker",
            (*snap(Scenario::ChooseSpeaker)).clone(),
            "Where should Stream To Speaker play?",
            Arcs::Ink,
            Badge::None,
            "Choose a speaker",
            Some("Stream To Speaker"),
            None,
            "Choose a speaker on the PC",
        ),
        (
            "Windows refused",
            (*snap(Scenario::WindowsRefused)).clone(),
            "Windows didn’t switch to Kitchen",
            Arcs::Green,
            Badge::Attention,
            "Windows didn’t switch to Kitchen",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Windows didn’t switch to Kitchen",
        ),
        (
            "Music Assistant playing",
            (*snap(Scenario::MaPlaying)).clone(),
            "Playing through Music Assistant",
            Arcs::Green,
            Badge::None,
            "Playing through Music Assistant",
            Some("Music Assistant (Stream To Speaker)"),
            None,
            "Playing through Music Assistant",
        ),
        (
            "Music Assistant waiting",
            (*snap(Scenario::MaWaiting)).clone(),
            "Waiting for Music Assistant to play it",
            Arcs::GreenFaint,
            Badge::None,
            "Waiting for Music Assistant",
            Some("Music Assistant (Stream To Speaker)"),
            None,
            "Waiting for Music Assistant",
        ),
        (
            "Music Assistant away",
            (*snap(Scenario::MaAway)).clone(),
            "Music Assistant isn’t connected",
            Arcs::Ink,
            Badge::Attention,
            "Music Assistant isn’t connected",
            Some("Music Assistant (Stream To Speaker)"),
            None,
            "Music Assistant isn’t connected",
        ),
        (
            "output missing",
            (*snap(Scenario::OutputMissing)).clone(),
            "The Stream To Speaker sound output is missing",
            Arcs::Ink,
            Badge::Failure,
            "Sound output missing",
            None,
            Some(N::OutputMissing),
            "Sound output missing",
        ),
        (
            "only some apps",
            (*snap(Scenario::SentApps)).clone(),
            "Playing Spotify on Kitchen",
            Arcs::Green,
            Badge::None,
            "Playing Spotify on Kitchen",
            Some("Kitchen (Stream To Speaker)"),
            None,
            "Playing Spotify on Kitchen",
        ),
    ];
    for (what, s, title, arcs, badge, tip, device, note, status) in cases {
        assert_eq!(s.now.title, title, "{what}: panel title");
        assert_eq!((s.tray.arcs, s.tray.badge), (arcs, badge), "{what}: tray icon");
        assert_eq!(s.tray.tooltip, format!("Stream To Speaker – {tip}"), "{what}: tooltip");
        assert_eq!(s.device_name.as_deref(), device, "{what}: device name");
        assert_eq!(s.notification.as_ref().map(|n| n.kind), note, "{what}: notification");
        assert_eq!(s.status, status, "{what}: status");
        assert_eq!(s.tray.menu.first(), Some(&MenuEntry::Label(tip.to_string())), "{what}: menu's state line");
    }
    assert_eq!(surfaces::device_name_not_running(EN), "Stream To Speaker (not running)");

    // After a fallback, a takeover or a speaker coming back, the badge lasts
    // until the panel has been opened once; the card stays.
    for s in [Scenario::FellBack, Scenario::Taken, Scenario::SpeakerBack] {
        let seen = snap_with(s, |f| f.seen = true);
        assert_eq!(seen.tray.badge, Badge::None, "{s:?}");
        assert_eq!(seen.now.title, snap(s).now.title, "{s:?}");
    }
    let e = ScriptedEngine::new(Scenario::FellBack);
    e.send(Surface::Panel, Command::PanelShown(true));
    e.settle();
    assert_eq!(e.snapshot().tray.badge, Badge::None);
}

fn row<'a>(s: &'a Snapshot, title: &str) -> &'a Row {
    s.list
        .rows()
        .find(|r| r.title == title)
        .unwrap_or_else(|| panic!("no row {title}: {:?}", s.list.rows().map(|r| &r.title).collect::<Vec<_>>()))
}

/// What a row's second line says, by situation.
#[test]
fn rows_by_situation() {
    let k = kitchen();
    // Normal: the kind and the delay.
    let all = snap_with(Scenario::OnThisPc, |f| {
        for s in &mut f.speakers {
            s.hidden = false;
        }
        f.speakers.push(SpeakerFacts {
            id: SpeakerId::new("den"),
            name: "Den TV".into(),
            kind: SpeakerKind::AppleTv,
            address: "192.168.1.45".into(),
            delay: Some(SpeakerDelay { own_ms: 2000, measured: false, change_ms: 0 }),
            presence: Presence::Found,
            used_here: true,
            hidden: false,
            busy: false,
            failure: None,
            question: None,
            twin: None,
            last_used: None,
        });
        f.speakers.push(SpeakerFacts {
            id: SpeakerId::new("desk"),
            name: "Desk Speaker".into(),
            kind: SpeakerKind::Sendspin,
            delay: Some(SpeakerDelay { own_ms: 500, measured: false, change_ms: 0 }),
            ..f.speakers[0].clone()
        });
        f.speakers[0].delay = Some(SpeakerDelay { own_ms: 200, measured: false, change_ms: 0 });
    });
    for (title, line) in [
        ("Kitchen", "Sonos · about 0.2 s delay"),
        ("Living Room + Dining Room", "Sonos group · about 0.2 s delay"),
        ("Bedroom", "HomePod pair · about 0.2 s delay"),
        ("Office", "AirPlay · about 2 s delay"),
        ("Den TV", "Apple TV · about 2 s delay"),
        ("Desk Speaker", "Sendspin speaker · about 0.5 s delay"),
    ] {
        assert_eq!(row(&all, title).tooltip, line);
    }
    // Every delay figure includes what was removed.
    assert_eq!(row(&snap(Scenario::OnThisPc), "Kitchen").tooltip, "Sonos · about 0.15 s delay");
    // Measured.
    let measured = snap_with(Scenario::OnThisPc, |f| {
        f.speakers[0].delay = Some(SpeakerDelay { own_ms: 210, measured: true, change_ms: 0 })
    });
    assert_eq!(row(&measured, "Kitchen").tooltip, "Sonos · 0.21 s delay, measured");

    // Connecting: the stages.
    for (stage, line) in [
        (ConnectStage::Finding, "Finding Kitchen…"),
        (ConnectStage::Asking, "Asking Kitchen to play…"),
        (ConnectStage::Waiting, "Waiting for Kitchen to start…"),
        (ConnectStage::Pairing, "Pairing — finish in the window that opened"),
    ] {
        let s = snap_with(
            Scenario::OnThisPc,
            set_state(NowState::Connecting { target: k.clone(), from: SoundAt::ThisPc, stage }),
        );
        let r = row(&s, "Kitchen");
        assert_eq!(r.tooltip, line);
        assert!(r.status == Some(RowStatus::Spinner) && r.command.is_none());
        assert_eq!(s.top.corner.as_ref().map(|b| &b.command), Some(&Command::CancelConnect));
    }

    // A click that failed: red, with Try again, until acted on.
    for (failure, line) in [
        (ConnectFailure::NoAnswer, "Couldn’t connect — no answer."),
        (ConnectFailure::WrongPassword, "Couldn’t connect — wrong password."),
    ] {
        let s = snap_with(Scenario::OnThisPc, |f| f.speakers[0].failure = Some(failure));
        let r = row(&s, "Kitchen");
        assert_eq!((r.tooltip.as_str(), r.tone), (line, Tone::Error));
        let e = r.error.as_ref().expect("a second line");
        assert_eq!((e.text.as_str(), e.retry.label.as_str()), (line, "Try again"));
    }

    let line = |s: Scenario, title: &str| {
        let snapshot = snap(s);
        let r = row(&snapshot, title);
        (r.tooltip.clone(), r.tone, r.dimmed)
    };
    assert_eq!(
        line(Scenario::NotResponding, "Kitchen"),
        ("Isn’t responding — trying again (2 of 3)".into(), Tone::Warning, false)
    );
    assert_eq!(line(Scenario::StaySilent, "Kitchen").0, "Isn’t responding — trying every 30 s");
    assert_eq!(line(Scenario::NotTaking, "Kitchen"), ("Connected · not taking the sound".into(), Tone::Warning, false));
    assert_eq!(line(Scenario::WindowsSwitched, "Kitchen"), ("Let go at 21:50".into(), Tone::Plain, false));
    assert_eq!(line(Scenario::FellBack, "Kitchen"), ("Stopped responding at 21:14".into(), Tone::Warning, false));
    assert_eq!(line(Scenario::Taken, "Kitchen"), ("Playing something else since 21:40".into(), Tone::Warning, false));
    assert_eq!(line(Scenario::NeedsPairing, "Den TV"), ("Needs pairing".into(), Tone::Warning, false));
    assert_eq!(line(Scenario::BusyQuestion, "Kitchen").0, "Sonos · playing something else");
    assert_eq!(line(Scenario::OnThisPc, "Garage"), ("Not found right now".into(), Tone::Plain, true));
    let cat = snap(Scenario::RowCatalogue);
    assert!(cat.list.rows().all(|r| r.title != "Bathroom"), "a speaker the app can't play to isn't listed");
    assert_eq!(row(&cat, "Bedroom").tooltip, "Waiting for the second HomePod");
    assert_eq!(row(&cat, "Office").tooltip, "AirPlay · 1.74 s delay, measured");
    assert_eq!(row(&cat, "Kitchen (192.168.1.20)").tooltip, "Sonos · two devices answer as Kitchen");
    assert_eq!(row(&cat, "Kitchen (192.168.1.31)").tone, Tone::Warning);
    assert_eq!(row(&cat, "Den TV").tooltip, "Couldn’t connect — no answer.");

    // The busy question, in place.
    let busy = snap(Scenario::BusyQuestion);
    let q = row(&busy, "Kitchen").question.as_ref().expect("the question");
    assert_eq!(q.title, "Kitchen is playing something else right now.");
    assert_eq!(q.line, "Play this PC’s sound there instead? What’s playing on Kitchen will stop.");
    assert_eq!(labels(&[q.confirm.clone(), q.cancel.clone()]), ["Play on Kitchen*", "Cancel"]);
    let other_pc = snap_with(Scenario::BusyQuestion, |f| {
        f.speakers[0].question = Some(TakeOver { other_pc: Some("LAPTOP-ANNA".into()) })
    });
    assert_eq!(
        row(&other_pc, "Kitchen").question.as_ref().unwrap().title,
        "Kitchen is playing LAPTOP-ANNA’s sound right now."
    );

    // Music Assistant's row.
    assert_eq!(line(Scenario::OnThisPc, "Music Assistant").0, "Chooses the speaker in Music Assistant");
    assert_eq!(line(Scenario::MaPlaying, "Music Assistant").0, "Playing · Music Assistant chooses the speaker");
    assert_eq!(line(Scenario::MaWaiting, "Music Assistant").0, "Offered · not playing yet");
    assert_eq!(line(Scenario::MaAway, "Music Assistant"), ("Not connected · last seen 21:12".into(), Tone::Warning, false));
    assert_eq!(snap(Scenario::OnThisPc).list.music_assistant.as_ref().unwrap().heading, "Through Music Assistant");

    // Order: This PC first, then by name, remembered absent ones last.
    let playing = snap(Scenario::Playing);
    let titles: Vec<&str> = playing.list.speakers.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["Bedroom", "Kitchen", "Living Room + Dining Room", "Office", "Garage"]);
    assert_eq!(playing.list.this_pc.title, "This PC");
    assert_eq!(playing.list.this_pc.tooltip, "Speakers (Realtek Audio)");
    assert_eq!(playing.list.heading, "Play on");

    // The current row: accent, no click, a menu with Reconnect.
    let r = row(&playing, "Kitchen");
    assert!(r.current && r.command.is_none());
    let menu: Vec<&str> = r.menu.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(menu, ["Speaker settings", "Play a test sound", "Reconnect", "Hide from this list"]);
    let other: Vec<&str> = row(&playing, "Bedroom").menu.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(other, ["Speaker settings", "Play a test sound", "Hide from this list"]);
    assert_eq!(row(&playing, "Bedroom").command, Some(Command::Play(Place::Speaker(SpeakerId::new("bedroom")))));

    // Screen readers hear the row whole.
    assert_eq!(r.spoken, "Kitchen. Sonos, about 0.15 second delay. Playing.");

    // A crowded network folds the speakers never used from here.
    let crowded = snap(Scenario::CrowdedNetwork);
    let more = crowded.list.more.as_ref().expect("folded");
    assert_eq!(more.title, "More speakers (23)");
    assert_eq!(more.tooltip, "Never used from this PC");
    assert_eq!(crowded.list.speakers.len(), 2);
    assert_eq!(snap(Scenario::RowCatalogue).list.more.as_ref().unwrap().title, "More speakers (12)");

    // Searching.
    let none = snap(Scenario::NothingFound);
    let s = none.list.searching.as_ref().unwrap();
    assert_eq!(s.title, "Looking for speakers on Wi-Fi “HomeNet”…");
    assert_eq!(s.tooltip, "Every 10 seconds while this is open");
    let nf = s.none_found.as_ref().unwrap();
    assert_eq!(nf.title, "No speakers found yet.");
    assert_eq!(nf.causes, ["Windows treats “HomeNet” as a public network, so speakers can’t answer."]);
    assert_eq!(labels(&nf.buttons), ["Fix in Windows settings*", "Can’t find your speaker?", "Add by address"]);
}

/// The delay control, state by state.
#[test]
fn delay_control_state_by_state() {
    struct Want {
        figure: &'static str,
        change: Option<&'static str>,
        line: Option<(&'static str, Tone)>,
        action: Option<&'static str>,
        remove_enabled: bool,
        add_enabled: bool,
    }
    let w = |figure, change, line, action, remove_enabled, add_enabled| Want {
        figure,
        change,
        line,
        action,
        remove_enabled,
        add_enabled,
    };
    let cases: Vec<(Scenario, Want)> = vec![
        (
            Scenario::DelayOwn,
            w(
                "about 0.2 s",
                None,
                Some(("Sound behind the picture? Remove some delay. Sound breaking up? Add some.", Tone::Plain)),
                None,
                true,
                true,
            ),
        ),
        (Scenario::Playing, w("about 0.15 s", Some("50 ms removed"), None, None, true, true)),
        (
            Scenario::DelayRemoving,
            w("under 0.1 s", Some("125 ms removed"), Some(("Removing… 75 ms to go", Tone::Plain)), None, true, true),
        ),
        (
            Scenario::DelayAtLimit,
            w(
                "under 0.1 s",
                Some("200 ms removed"),
                Some(("Kitchen allows at most 200 ms removed.", Tone::Warning)),
                None,
                false,
                true,
            ),
        ),
        (
            Scenario::DelayAdding,
            w("about 0.3 s", Some("100 ms added"), Some(("Adding… 50 ms to go", Tone::Plain)), None, true, true),
        ),
        (
            Scenario::DelayGoingBack,
            w("about 0.2 s", None, Some(("Adding… 125 ms to go", Tone::Plain)), None, true, true),
        ),
        (
            Scenario::DelayReconnectPending,
            w(
                "about 0.25 s",
                Some("50 ms removed"),
                Some(("Office reconnects to apply this: a few seconds of silence.", Tone::Busy)),
                None,
                true,
                true,
            ),
        ),
        (
            Scenario::ApplyingDelay,
            w("about 0.25 s", Some("50 ms removed"), Some(("Reconnecting Office…", Tone::Busy)), None, true, true),
        ),
        (
            Scenario::DelayNotAccepted,
            w(
                "about 1.8 s",
                Some("200 ms removed"),
                Some(("Office didn’t accept 300 ms removed, so it’s back to 200 ms removed.", Tone::Warning)),
                None,
                true,
                true,
            ),
        ),
        (
            Scenario::DelaySteadyRemove,
            w(
                "about 1.9 s",
                Some("100 ms removed"),
                Some(("Office is about 2 s late on Steady. For video, use Short delay (about 0.3 s).", Tone::Info)),
                Some("Use Short delay"),
                true,
                true,
            ),
        ),
        (
            Scenario::DelaySwitchedToShort,
            w(
                "about 0.3 s",
                None,
                Some(("Short delay starts with nothing removed or added. Steady keeps its 300 ms removed.", Tone::Plain)),
                None,
                true,
                true,
            ),
        ),
        (
            Scenario::DelaySendspin,
            w(
                "about 0.5 s",
                None,
                Some(("Desk Speaker already plays with as little delay as it can.", Tone::Plain)),
                None,
                false,
                true,
            ),
        ),
        (
            Scenario::DelayDroppedOut,
            w(
                "under 0.1 s",
                Some("150 ms removed"),
                Some(("Kitchen dropped out with 200 ms removed, so 50 ms went back: 150 ms removed.", Tone::Warning)),
                None,
                true,
                true,
            ),
        ),
        (
            Scenario::DelayDropouts,
            w(
                "about 0.2 s",
                None,
                Some(("Kitchen dropped out 3 times in 10 minutes. Adding 100 ms may help.", Tone::Info)),
                Some("Add 100 ms"),
                true,
                true,
            ),
        ),
    ];
    for (scenario, want) in cases {
        let s = snap(scenario);
        let c = s.now.delay.as_ref().unwrap_or_else(|| panic!("{scenario:?}: no delay control"));
        assert_eq!(c.label, "Delay");
        assert_eq!(c.figure, want.figure, "{scenario:?}: figure");
        assert_eq!(c.change.as_deref(), want.change, "{scenario:?}: change");
        assert_eq!(c.reset.is_some(), want.change.is_some(), "{scenario:?}: ↺");
        let line = c.line.as_ref().map(|l| (l.text.as_str(), l.tone));
        assert_eq!(line, want.line, "{scenario:?}: line");
        let action = c.line.as_ref().and_then(|l| l.action.as_ref()).map(|b| b.label.as_str());
        assert_eq!(action, want.action, "{scenario:?}: action");
        let DelayBody::Buttons { remove_large, remove_small, add_large, add_small } = &c.body else {
            panic!("{scenario:?}: buttons")
        };
        assert_eq!(
            [&remove_large.label, &remove_small.label, &add_large.label, &add_small.label],
            ["Remove 100 ms", "Remove 25 ms", "Add 100 ms", "Add 25 ms"]
        );
        assert_eq!((remove_large.enabled, remove_small.enabled), (want.remove_enabled, want.remove_enabled));
        assert_eq!((add_large.enabled, add_small.enabled), (want.add_enabled, want.add_enabled));
    }

    // The head line, read aloud; the buttons' whole actions.
    let playing = snap(Scenario::Playing);
    let c = playing.now.delay.as_ref().unwrap();
    assert_eq!(c.spoken, "Delay on Kitchen: about 0.15 seconds, 50 milliseconds removed. Press Enter to type an amount.");
    assert_eq!(c.reset.as_ref().unwrap().label, "Back to Kitchen’s own delay");
    assert_eq!(c.spoken_total, "50 milliseconds removed");
    assert_eq!(c.spoken_done, "Done: 50 milliseconds removed");
    let DelayBody::Buttons { remove_large, .. } = &c.body else { unreachable!() };
    assert_eq!(remove_large.spoken.as_deref(), Some("Remove 100 milliseconds of delay"));
    assert_eq!(c.column_headings, ["Sound behind the picture?", "Sound breaking up?"]);
    let own = snap(Scenario::DelayOwn);
    assert_eq!(
        own.now.delay.as_ref().unwrap().spoken,
        "Delay on Kitchen: about 0.2 seconds, nothing removed or added. Press Enter to type an amount."
    );

    // An AirPlay 2 speaker on Steady: the head line stays, the buttons go.
    let fixed = snap(Scenario::DelayFixedOnSteady);
    let c = fixed.now.delay.as_ref().unwrap();
    assert_eq!((c.figure.as_str(), c.change.as_deref(), c.reset.is_some()), ("about 2 s", None, false));
    let DelayBody::Fixed { text, action } = &c.body else { panic!("fixed") };
    assert_eq!(
        text,
        "On Steady, Den TV keeps this delay itself, so none of it can be removed yet. For video, use Short delay (about 0.3 s)."
    );
    assert_eq!(action.label, "Use Short delay");
    assert_eq!(c.line, None);

    // Other limits.
    let steady_limit = snap_with(Scenario::DelaySteadyRemove, |f| {
        f.speakers.iter_mut().find(|s| s.id.as_str() == "office").unwrap().delay.as_mut().unwrap().change_ms = -1000;
        f.delay.as_mut().unwrap().note = None;
    });
    let l = steady_limit.now.delay.as_ref().unwrap().line.as_ref().unwrap();
    assert_eq!(l.text, "On Steady, Office allows at most 1000 ms removed. For less delay, use Short delay.");
    assert_eq!(l.action.as_ref().unwrap().label, "Use Short delay");
    let added = snap_with(Scenario::DelaySendspin, |f| {
        f.speakers.iter_mut().find(|s| s.id.as_str() == "desk").unwrap().delay.as_mut().unwrap().change_ms = 800;
    });
    assert_eq!(added.now.delay.as_ref().unwrap().line.as_ref().unwrap().text, "Desk Speaker allows at most 800 ms added.");

    // The Lab's step sizes: every label follows.
    let lab = snap_with(Scenario::Playing, |f| f.delay_steps = DelaySteps { small_ms: 50, large_ms: 200 });
    let DelayBody::Buttons { remove_large, add_small, .. } = &lab.now.delay.as_ref().unwrap().body else {
        unreachable!()
    };
    assert_eq!((remove_large.label.as_str(), add_small.label.as_str()), ("Remove 200 ms", "Add 50 ms"));
}

/// Any amount: the field, its two buttons and the line that says what
/// Enter does before anything changes.
#[test]
fn typed_amount() {
    let view = |change: i32| {
        let s = snap_with(Scenario::Playing, |f| f.speakers[0].delay.as_mut().unwrap().change_ms = change);
        s.delay.clone().unwrap()
    };
    let at50 = view(-50);
    let empty = amount_field(EN, &at50, None, Enter::Remove);
    assert_eq!((empty.remove.label.as_str(), empty.add.label.as_str()), ("Remove", "Add"));
    assert!(!empty.remove.enabled && !empty.add.enabled);
    assert_eq!(empty.line, "Type how many milliseconds, then choose Remove or Add.");
    assert_eq!((empty.label.as_str(), empty.unit.as_str()), ("Amount", "ms"));

    let f = amount_field(EN, &at50, Some(150), Enter::Remove);
    assert_eq!(labels(&[f.remove.clone(), f.add.clone()]), ["Remove 150 ms*", "Add 150 ms"]);
    assert_eq!(f.line, "Enter removes 150 ms: 200 ms removed in all.");
    assert_eq!(f.phone_lines, ["Remove 150 ms: 200 ms removed in all.", "Add 150 ms: 100 ms added in all."]);
    assert_eq!(f.remove.command, Command::ChangeDelay { speaker: kitchen(), step: DelayStep::Remove(150) });

    let cases: &[(i32, u32, Enter, &str, Tone)] = &[
        (-50, 40, Enter::Add, "Enter adds 40 ms: 10 ms removed in all.", Tone::Plain),
        (-50, 100, Enter::Add, "Enter adds 100 ms: 50 ms added in all.", Tone::Plain),
        (-50, 50, Enter::Add, "Enter adds 50 ms: back to Kitchen’s own delay.", Tone::Plain),
        (-150, 100, Enter::Remove, "Kitchen allows at most 200 ms removed, so Enter removes 50 ms.", Tone::Warning),
        (850, 300, Enter::Add, "Kitchen allows at most 1000 ms added, so Enter adds 150 ms.", Tone::Warning),
        (-200, 25, Enter::Remove, "Kitchen allows at most 200 ms removed, so Enter changes nothing.", Tone::Warning),
    ];
    for &(change, typed, enter, line, tone) in cases {
        let f = amount_field(EN, &view(change), Some(typed), enter);
        assert_eq!((f.line.as_str(), f.tone), (line, tone), "{change} {typed} {enter:?}");
    }
    let f = amount_field(EN, &view(-150), Some(100), Enter::Remove);
    assert_eq!(f.phone_lines[0], "Remove 100 ms: 200 ms removed in all, the most Kitchen allows.");
    let f = amount_field(EN, &view(-200), Some(25), Enter::Remove);
    assert!(!f.remove.enabled, "nothing left to remove");
    assert_eq!(f.phone_lines[0], "Kitchen allows at most 200 ms removed.");
}

fn delay_outcome(scenario: Scenario, change: i32, step: DelayStep) -> Outcome {
    let e = ScriptedEngine::new(scenario);
    let mut f = e.facts();
    if let Some(id) = f.delay.as_ref().map(|d| d.speaker.clone()) {
        f.speakers.iter_mut().find(|s| s.id == id).unwrap().delay.as_mut().unwrap().change_ms = change;
    }
    let speaker = f.delay.as_ref().map(|d| d.speaker.clone()).unwrap_or_else(kitchen);
    e.set_facts(f);
    let ticket = e.send(Surface::Shortcut, Command::ChangeDelay { speaker, step });
    assert!(ticket.try_outcome().is_none(), "sending does no work on the caller's thread");
    e.advance(0);
    ticket.try_outcome().expect("handled")
}

/// The confirmation of a delay shortcut, by the clock.
#[test]
fn shortcut_confirmation() {
    let c = |s, change, step| {
        let x = confirmation(EN, &delay_outcome(s, change, step));
        (x.over, x.big, x.small, x.warning)
    };
    let s = |a: &str, b: &str, c: &str, w: bool| (a.to_string(), b.to_string(), c.to_string(), w);
    assert_eq!(c(Scenario::Playing, -50, DelayStep::Remove(25)), s("Kitchen", "75 ms removed", "Removed 25 ms", false));
    assert_eq!(c(Scenario::Playing, -25, DelayStep::Add(25)), s("Kitchen", "Kitchen’s own delay", "Added 25 ms", false));
    assert_eq!(c(Scenario::Playing, -75, DelayStep::Add(100)), s("Kitchen", "25 ms added", "Added 100 ms", false));
    assert_eq!(
        c(Scenario::DelaySteadyRemove, -200, DelayStep::Remove(100)),
        s("Office", "300 ms removed", "Reconnecting to apply: a few seconds of silence", false)
    );
    assert_eq!(
        c(Scenario::Playing, -150, DelayStep::Remove(100)),
        s("Kitchen", "200 ms removed", "Removed 50 ms: the most Kitchen allows", true)
    );
    assert_eq!(
        c(Scenario::Playing, -200, DelayStep::Remove(25)),
        s("Kitchen", "Can’t remove more", "Kitchen allows at most 200 ms removed", true)
    );
    assert_eq!(
        c(Scenario::Playing, 1000, DelayStep::Add(25)),
        s("Kitchen", "Can’t add more", "Kitchen allows at most 1000 ms added", true)
    );
    assert_eq!(
        c(Scenario::DelayFixedOnSteady, 0, DelayStep::Remove(25)),
        s("Den TV", "No delay changes on Steady", "Use Short delay on Den TV’s page", false)
    );
    assert_eq!(
        c(Scenario::OnThisPc, 0, DelayStep::Remove(25)),
        s("Stream To Speaker", "Sound is on this PC", "No speaker delay to change", false)
    );
}

/// The command line's words.
#[test]
fn command_line() {
    let playing = snap(Scenario::Playing);
    assert_eq!(playing.cli_status, "Playing on Kitchen · Sonos · delay about 0.15 s, 50 ms removed · volume 32");
    assert_eq!(
        cli_delay_info(EN, &playing).text,
        "Kitchen: delay about 0.15 s, 50 ms removed. You can remove up to 200 ms and add up to 1000 ms."
    );
    let reply = |change, step| cli_reply(EN, &delay_outcome(Scenario::Playing, change, step));
    let r = reply(-50, DelayStep::Remove(25));
    assert_eq!((r.text.as_str(), r.exit), ("Kitchen: removed 25 ms — 75 ms removed in all. Removing…", 0));
    assert_eq!(reply(-75, DelayStep::Add(100)).text, "Kitchen: added 100 ms — 25 ms added in all. Adding…");
    assert_eq!(
        reply(-150, DelayStep::Remove(100)).text,
        "Kitchen: removed 50 ms — 200 ms removed in all, the most Kitchen allows."
    );
    assert_eq!(reply(0, DelayStep::SetTotal(-150)).text, "Kitchen: set to 150 ms removed. Removing…");
    assert_eq!(reply(-50, DelayStep::Reset).text, "Kitchen: back to its own delay.");
    let r = reply(-200, DelayStep::Remove(25));
    assert_eq!((r.text.as_str(), r.exit), ("Kitchen: can’t remove more — Kitchen allows at most 200 ms removed.", 3));

    // Refused: no speaker delay here.
    let r = cli_reply(EN, &delay_outcome(Scenario::OnThisPc, 0, DelayStep::Remove(25)));
    assert_eq!((r.text.as_str(), r.exit), ("Sound is on this PC — no speaker delay to change.", 3));
    let r = cli_delay_info(EN, &snap(Scenario::OnThisPc));
    assert_eq!((r.text.as_str(), r.exit), ("Sound is on this PC — no speaker delay to change.", 3));

    // play pc.
    let e = ScriptedEngine::new(Scenario::Playing);
    let ticket = e.send(Surface::CommandLine, Command::Play(Place::ThisPc));
    e.advance(0);
    let r = cli_reply(EN, &ticket.try_outcome().unwrap());
    assert_eq!((r.text.as_str(), r.exit), ("Sound is on this PC.", 0));

    // The headless picker.
    assert_eq!(
        surfaces::picker_header(EN, "0.2.0", "Wi-Fi (192.168.1.23)"),
        "Stream To Speaker 0.2.0 — looking for speakers on Wi-Fi (192.168.1.23)…"
    );
    assert_eq!(surfaces::picker_prompt(EN, 3), "Play on [1-3], Enter = this PC, r = search again: ");
}

/// The tray menu: the state, the places, Open, Settings, Quit.
#[test]
fn tray_menu() {
    fn flat(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                MenuEntry::Label(l) => format!("({l})"),
                MenuEntry::Separator => "—".into(),
                MenuEntry::Item(i) => format!("{}{}", if i.checked { "✓ " } else { "" }, i.label),
            })
            .collect()
    }
    assert_eq!(
        flat(&snap(Scenario::Playing).tray.menu),
        [
            "(Playing on Kitchen)",
            "—",
            "This PC",
            "Bedroom",
            "✓ Kitchen",
            "Living Room + Dining Room",
            "Office",
            "Music Assistant",
            "—",
            "Open",
            "Settings",
            "—",
            "Quit (sound returns to this PC)",
        ]
    );
    let quit = |s: Scenario| flat(&snap(s).tray.menu).last().cloned().unwrap();
    assert_eq!(quit(Scenario::OnThisPc), "Quit Stream To Speaker");
    assert_eq!(quit(Scenario::FellBack), "Quit Stream To Speaker");
    assert_eq!(quit(Scenario::NotResponding), "Quit (sound returns to this PC)");
    let crowded = flat(&snap(Scenario::CrowdedNetwork).tray.menu);
    assert!(crowded.contains(&"More speakers…".to_string()));
}

#[derive(Debug, PartialEq)]
enum Third3 {
    None,
    Text(String),
    Delay(String),
}

fn third(s: &Snapshot) -> Third3 {
    match &s.top.third {
        None => Third3::None,
        Some(Third::Text(t)) => Third3::Text(t.clone()),
        Some(Third::Delay(d)) => Third3::Delay(format!("{}  {}", d.label, d.value)),
    }
}

/// The top of the panel, by state: the sentence, a detail and buttons only
/// in a state the person must know about, the volume row and a third line
/// that depends on the place.
#[test]
fn panel_top_by_state() {
    let txt = |s: &str| Third3::Text(s.to_string());
    let delay = |s: &str| Third3::Delay(s.to_string());
    let pc = || txt("Speakers (Realtek Audio)");
    let cases: Vec<(&str, Snapshot, &str, Option<&str>, Vec<&str>, Third3)> = vec![
        ("on this PC", (*snap(Scenario::OnThisPc)).clone(), "Sound is on this PC", None, vec![], pc()),
        (
            "on this PC, before any speaker",
            (*snap(Scenario::ChooseSpeaker)).clone(),
            "Where should Stream To Speaker play?",
            Some("Pick a speaker. If you don’t, your sound goes back to Speakers (Realtek Audio)."),
            vec![],
            Third3::None,
        ),
        (
            "connecting",
            (*snap(Scenario::Connecting)).clone(),
            "Connecting to Bedroom…",
            None,
            vec![],
            txt("Your sound moves there when it is ready."),
        ),
        ("playing", (*snap(Scenario::Playing)).clone(), "Playing on Kitchen", None, vec![], delay("Delay  about 0.15 s")),
        (
            "applying a delay change",
            (*snap(Scenario::ApplyingDelay)).clone(),
            "Playing on Office",
            None,
            vec![],
            delay("Delay  reconnecting to remove 50 ms…"),
        ),
        ("ready", (*snap(Scenario::Ready)).clone(), "Kitchen is ready", None, vec![], delay("Delay  about 0.15 s")),
        (
            "Music Assistant",
            (*snap(Scenario::MaPlaying)).clone(),
            "Playing through Music Assistant",
            None,
            vec![],
            txt("Music Assistant plays it in Living Room."),
        ),
        (
            "not taking: no second button",
            (*snap(Scenario::NotTaking)).clone(),
            "Kitchen isn’t taking the sound",
            Some("This PC is sending sound, but Kitchen stopped asking for it at 21:14."),
            vec!["Reconnect*"],
            delay("Delay  about 0.15 s"),
        ),
        (
            "not responding",
            (*snap(Scenario::NotResponding)).clone(),
            "Kitchen isn’t responding",
            Some("Trying again (2 of 3). If Kitchen doesn’t respond within 15 seconds, your sound comes back to this PC."),
            vec!["Play on this PC now"],
            Third3::None,
        ),
        (
            "fell back",
            (*snap(Scenario::FellBack)).clone(),
            "Sound is back on this PC",
            Some("Kitchen stopped responding at 21:14, so your sound came back to Speakers (Realtek Audio)."),
            vec!["Try Kitchen again*", "Stay on this PC"],
            pc(),
        ),
        (
            "can't find: Speaker missing? is in the footer",
            (*snap(Scenario::CantFind)).clone(),
            "Can’t find Kitchen right now",
            Some("Your sound stays on this PC. It goes to Kitchen when Kitchen is found, unless something plays here first."),
            vec!["Stay on this PC"],
            pc(),
        ),
        (
            "Windows refused",
            (*snap(Scenario::WindowsRefused)).clone(),
            "Windows didn’t switch to Kitchen",
            Some("Windows kept Speakers (Realtek Audio) as its output. In Sound settings, choose “Kitchen (Stream To Speaker)”."),
            vec!["Open Sound settings*"],
            delay("Delay  about 0.15 s"),
        ),
        (
            "Windows switched",
            (*snap(Scenario::WindowsSwitched)).clone(),
            "Sound is on Headphones",
            Some("Windows switched its output to Headphones (WH-1000XM4), so Stream To Speaker let go of Kitchen at 21:50."),
            vec!["Play on Kitchen"],
            txt("Headphones (WH-1000XM4)"),
        ),
        (
            "output missing",
            (*snap(Scenario::OutputMissing)).clone(),
            "The Stream To Speaker sound output is missing",
            Some("Windows doesn’t have the output this app plays through, so your sound stays on this PC. Repair puts it back; it needs administrator approval."),
            vec!["Repair*"],
            pc(),
        ),
        (
            "Music Assistant waiting",
            (*snap(Scenario::MaWaiting)).clone(),
            "Waiting for Music Assistant to play it",
            Some("Music Assistant has this PC’s sound but hasn’t started playing it. Start it there, or set it to start by itself."),
            vec!["How?", "Play on this PC"],
            Third3::None,
        ),
    ];
    for (what, s, sentence, detail, actions, want) in cases {
        assert_eq!(s.top.sentence, sentence, "{what}: sentence");
        assert_eq!(s.top.detail.as_deref(), detail, "{what}: detail");
        assert_eq!(labels(&s.top.actions), actions, "{what}: buttons");
        assert_eq!(third(&s), want, "{what}: third line");
    }
    let first = snap_with(Scenario::OnThisPc, |f| f.remembered = Remembered::Nothing);
    assert_eq!(third(&first), txt("Pick a speaker to play this PC’s sound there."));

    // Connecting: Cancel at the sentence's right end; the volume of where
    // the sound is now.
    let c = snap(Scenario::Connecting);
    assert_eq!(c.top.corner.as_ref().map(|b| b.label.as_str()), Some("Cancel"));
    assert_eq!(c.top.volume.as_ref().map(|v| v.level), Some(32));
    assert!(!c.top.meter);

    // Muted shows in the volume row only.
    let m = snap(Scenario::Muted);
    assert_eq!((m.top.detail.as_deref(), m.top.volume.as_ref().unwrap().number.as_str()), (None, "Muted"));
    assert!(m.top.meter);

    // Music Assistant's slider says what it sets in its tooltip.
    let ma = snap(Scenario::MaPlaying);
    assert_eq!(ma.top.volume.as_ref().unwrap().tooltip.as_deref(), Some("Level sent to Music Assistant"));

    // The fell-back line under the buttons stays.
    assert_eq!(snap(Scenario::FellBack).top.note.as_deref(), Some("When Kitchen is back, you’ll be asked."));
}

fn panel_delay_of(s: &Snapshot) -> &PanelDelay {
    match &s.top.third {
        Some(Third::Delay(d)) => d,
        other => panic!("no delay line: {other:?}"),
    }
}

/// Delay in the panel, always open: the line, the four buttons, what
/// matters now, and the bottom line with the change and *Reset* at the left
/// and *Other amount…* at the right.
#[test]
fn panel_delay() {
    let line = |s: Scenario| {
        let snapshot = snap(s);
        let d = panel_delay_of(&snapshot).clone();
        (d.note.map(|l| l.text), d.change, d.reset.map(|r| r.label))
    };
    let some = |s: &str| Some(s.to_string());
    assert_eq!(line(Scenario::Playing), (None, some("50 ms removed"), some("Reset")));
    assert_eq!(line(Scenario::DelayOwn), (None, None, None), "no symptom hint in the panel");
    assert_eq!(line(Scenario::DelayRemoving), (some("Removing… 75 ms to go"), some("125 ms removed"), some("Reset")));
    assert_eq!(
        line(Scenario::DelayAtLimit),
        (some("Kitchen allows at most 200 ms removed."), some("200 ms removed"), some("Reset"))
    );
    assert_eq!(line(Scenario::DelayAdding), (some("Adding… 50 ms to go"), some("100 ms added"), some("Reset")));
    assert_eq!(
        line(Scenario::DelaySendspin),
        (some("Desk Speaker already plays with as little delay as it can."), None, None)
    );
    assert_eq!(
        line(Scenario::ApplyingDelay),
        (some("Reconnecting to remove 50 ms: a few seconds of silence."), some("50 ms removed"), some("Reset"))
    );
    // *Other amount…* opens the amount field in place, as it reads before a
    // digit is typed; typing recomputes it from the snapshot's numbers.
    let playing = snap(Scenario::Playing);
    let d = panel_delay_of(&playing);
    assert_eq!(d.other_amount.as_deref(), Some("Other amount…"));
    let field = d.amount.as_ref().unwrap();
    let typed = crate::model::delay::amount_field(
        Lang::English,
        playing.delay.as_ref().unwrap(),
        Some(30),
        crate::model::delay::Enter::Remove,
    );
    assert_eq!(field.unit, typed.unit);
    assert_ne!(field.line, typed.line, "the field's line follows the typed amount");

    let playing = snap(Scenario::Playing);
    let d = panel_delay_of(&playing);
    assert_eq!(d.reset.as_ref().unwrap().spoken.as_deref(), Some("Back to Kitchen’s own delay"));
    assert_eq!(d.spoken, "Delay on Kitchen: about 0.15 seconds, 50 milliseconds removed. Press Enter to type an amount.");
    let DelayBody::Buttons { remove_large, remove_small, add_large, add_small } = &d.body else { panic!() };
    assert_eq!(
        [&remove_large.label, &add_large.label, &remove_small.label, &add_small.label],
        ["Remove 100 ms", "Add 100 ms", "Remove 25 ms", "Add 25 ms"]
    );

    // Where it can't be changed yet: the same line, then the sentence and
    // *Use Short delay*; no amount to type.
    let fixed = snap(Scenario::DelayFixedOnSteady);
    let d = panel_delay_of(&fixed);
    assert_eq!(d.value, "about 2 s");
    let DelayBody::Fixed { text, action } = &d.body else { panic!() };
    assert!(text.starts_with("On Steady, Den TV keeps this delay itself"));
    assert_eq!(action.label, "Use Short delay");
    assert_eq!(d.reset, None);
    assert_eq!((d.other_amount.as_ref(), d.amount.as_ref()), (None, None));

    // No delay line for This PC or Music Assistant.
    assert!(!matches!(snap(Scenario::OnThisPc).top.third, Some(Third::Delay(_))));
    assert!(!matches!(snap(Scenario::MaPlaying).top.third, Some(Third::Delay(_))));
}

/// A row is a name and, at its right end, a short status only when there
/// is one; what it used to say on a second line is its tooltip.
#[test]
fn rows_right_ends() {
    let status = |s: &Snapshot, title: &str| row(s, title).status.clone();
    let text = |t: &str| Some(RowStatus::Text(t.to_string()));
    let playing = snap(Scenario::Playing);
    assert_eq!(status(&playing, "Kitchen"), None);
    assert_eq!(status(&playing, "Bedroom"), None);
    assert_eq!(row(&playing, "Bedroom").tooltip, "HomePod pair · about 0.2 s delay");
    assert_eq!(status(&playing, "Garage"), text("Not found"));
    assert!(row(&playing, "Garage").dimmed);
    assert_eq!(status(&snap(Scenario::BusyQuestion), "Kitchen"), text("In use"));
    assert_eq!(status(&snap(Scenario::Connecting), "Bedroom"), Some(RowStatus::Spinner));
    assert_eq!(status(&snap(Scenario::NotResponding), "Kitchen"), Some(RowStatus::Attention));
    assert_eq!(status(&snap(Scenario::WindowsRefused), "Kitchen"), Some(RowStatus::Attention));
    let cat = snap(Scenario::RowCatalogue);
    assert_eq!(status(&cat, "Bedroom"), text("1 of 2 found"));
    assert_eq!(status(&cat, "Kitchen (192.168.1.20)"), Some(RowStatus::Attention));
    assert_eq!(status(&cat, "Den TV"), None, "a failed click grows a second line instead");
    assert!(row(&cat, "Den TV").error.is_some());

    // This PC is one row; no heading shows over the list in the panel, but
    // the phone's words stay.
    assert_eq!(playing.list.this_pc.title, "This PC");
    assert_eq!(playing.list.heading, "Play on");
    assert_eq!(playing.list.menu_name, "More");
}

/// The footer: Can't hear it? only while the place is a speaker or Music
/// Assistant; Speaker missing? and Settings always.
#[test]
fn footer() {
    let f = |s: Scenario| {
        let f = snap(s).footer.clone();
        (f.cant_hear.map(|b| b.label), f.speaker_missing.label, f.settings.label)
    };
    let both = |c: Option<&str>| (c.map(String::from), "Speaker missing?".to_string(), "Settings".to_string());
    assert_eq!(f(Scenario::OnThisPc), both(None));
    assert_eq!(f(Scenario::FellBack), both(None));
    assert_eq!(f(Scenario::Playing), both(Some("Can’t hear it?")));
    assert_eq!(f(Scenario::MaPlaying), both(Some("Can’t hear it?")));
    assert_eq!(
        snap(Scenario::Playing).footer.cant_hear.clone().unwrap().command,
        Command::Open(Open::CantHear(Some(kitchen())))
    );
}

/// First run's three moments, with less to read.
#[test]
fn first_run() {
    let pick = snap(Scenario::FirstRunPick);
    let w = pick.first_run.as_ref().unwrap();
    assert_eq!(
        (w.sentence.as_str(), w.detail.as_str(), w.looking_more.as_deref()),
        ("Where should this PC’s sound play?", "Speakers show up here as they’re found.", Some("Looking for more speakers…"))
    );
    assert_eq!(w.not_now.as_ref().unwrap().label, "Not now");
    assert_eq!(w.close.label, "Close");
    assert_eq!(pick.top.third, None, "first run's own words say it");

    let hear = snap(Scenario::FirstRunHear);
    let w = hear.first_run.as_ref().unwrap();
    assert_eq!(w.sentence, "Did you hear the chime on Kitchen?");
    assert_eq!(w.detail, "Kitchen now plays all of this PC’s sound. To bring it back, pick This PC.");
    assert_eq!(labels(&w.actions), ["I heard it*", "I didn’t hear it"]);
    assert_eq!(w.link.as_ref().unwrap().label, "Play it again");
    assert_eq!(hear.top.third, None, "no delay line until first run is over");
    assert_eq!(hear.top.volume.as_ref().unwrap().level, 25);

    let find = snap(Scenario::FirstRunFindIt);
    let w = find.first_run.as_ref().unwrap();
    assert_eq!(w.sentence, "Stream To Speaker stays by the clock");
    assert_eq!(w.detail, "Windows may tuck its icon under ^. Drag it onto the taskbar to keep it in view.");
    let sw = w.switch.as_ref().unwrap();
    assert_eq!((sw.label.as_str(), sw.on), ("Start when I sign in, and go back to Kitchen", true));
    assert_eq!(labels(&w.actions), ["Done*"]);

    // Walking it with the scripted engine.
    let e = ScriptedEngine::new(Scenario::FirstRunPick);
    e.send(Surface::Panel, Command::Play(Place::Speaker(kitchen())));
    e.settle();
    assert_eq!(e.snapshot().first_run.as_ref().unwrap().sentence, "Did you hear the chime on Kitchen?");
    e.send(Surface::Panel, Command::FirstRun(FirstRunAct::Heard));
    e.send(Surface::Panel, Command::FirstRun(FirstRunAct::StartAtSignIn(false)));
    e.settle();
    assert!(!e.snapshot().first_run.as_ref().unwrap().switch.as_ref().unwrap().on);
    e.send(Surface::Panel, Command::FirstRun(FirstRunAct::Done));
    e.settle();
    let s = e.snapshot();
    assert!(s.first_run.is_none());
    assert!(matches!(s.top.third, Some(Third::Delay(_))), "the delay line once first run is over");
}

/// Notifications: one button each, in the panel's words.
#[test]
fn notifications() {
    let n = |s: Scenario| {
        let n = snap(s).notification.clone().unwrap();
        (n.title, n.body, labels(&n.buttons))
    };
    let v = |a: &str, b: &str, c: &[&str]| (a.to_string(), b.to_string(), c.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(n(Scenario::FellBack), v("Kitchen stopped responding", "Your sound is back on this PC.", &["Try Kitchen again*"]));
    assert_eq!(n(Scenario::StaySilent), v("Kitchen stopped responding", "Nothing plays on this PC until it’s back.", &["Play on this PC*"]));
    assert_eq!(n(Scenario::Taken), v("Kitchen is playing something else", "Your sound is back on this PC.", &["Play on Kitchen again*"]));
    assert_eq!(n(Scenario::SpeakerBack), v("Kitchen is back", "Your sound is still on this PC.", &["Play on Kitchen*"]));
    assert_eq!(n(Scenario::NeedsPairing), v("Den TV needs pairing", "Your sound stays on this PC until you pair it.", &["Pair*"]));
    assert_eq!(n(Scenario::OutputMissing), v("Stream To Speaker can’t play", "Its sound output is missing from Windows.", &["Repair*"]));
    let restart = snap_with(Scenario::OnThisPc, |f| {
        f.notification = Some(NotificationFacts { seq: 9, kind: NotificationKind::RestartToFinish, speaker: None })
    });
    let r = restart.notification.unwrap();
    assert_eq!(r.title, "Restart to finish updating");
    assert_eq!(r.body, "The new sound output starts after Windows restarts. Stream To Speaker keeps working until then.");
    assert_eq!(labels(&r.buttons), ["Restart now*"]);
    assert_eq!(r.close.as_deref(), Some("Later"));
}

/// The notice line: one, in order; the donation never beside a problem.
#[test]
fn notice_line() {
    let notice = |s: Scenario| {
        snap(s).notice.clone().map(|n| (n.text, n.detail, labels(&n.actions), n.dismiss.is_some()))
    };
    let v = |a: &str, d: Option<&str>, b: &[&str], x: bool| {
        Some((a.to_string(), d.map(String::from), b.iter().map(|s| s.to_string()).collect::<Vec<_>>(), x))
    };
    assert_eq!(notice(Scenario::NoticeRestart), v("Restart Windows to finish updating.", None, &["Restart…~"], false));
    assert_eq!(
        notice(Scenario::NoticeAdapterOff),
        v("Not looking for speakers:", Some("the network adapter “Wi-Fi” is off."), &["Settings~"], false)
    );
    assert_eq!(notice(Scenario::NoticeRefused), v("Kitchen Sub asked for the sound and was refused.", None, &["Allow~"], false));
    assert_eq!(notice(Scenario::NoticeLab), v("Lab settings are on.", None, &["Turn off~"], false));
    assert_eq!(notice(Scenario::NoticeUpdate), v("Version 0.2.1 is ready to install.", None, &["What’s new~"], false));
    assert_eq!(notice(Scenario::NoticeUpdated), v("Updated to 0.2.1.", None, &["What’s new~"], true));
    assert_eq!(
        notice(Scenario::NoticeDonation),
        v("Stream To Speaker is free. Donations pay for signing its sound driver.", None, &["Donate~"], true)
    );
    assert_eq!(notice(Scenario::Playing), None);
    let both = snap_with(Scenario::NoticeDonation, |f| f.notices.lab_on = true);
    assert_eq!(both.notice.unwrap().kind, NoticeKind::LabOn, "precedence");
    let problem = snap_with(Scenario::NoticeDonation, |f| {
        f.state = NowState::NotResponding {
            speaker: kitchen(),
            dropout: Dropout::QuickTries { attempt: 1, of: 3, then: AfterTries::BackToPc },
        }
    });
    assert_eq!(problem.notice, None, "never while a problem shows");
}

/// The scripted engine walks through connecting when a place is chosen.
#[test]
fn scripted_engine_walks_through_connecting() {
    let e = ScriptedEngine::new(Scenario::OnThisPc);
    let changes = Arc::new(AtomicUsize::new(0));
    let c = changes.clone();
    let watch = e.watch(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    let k = kitchen();
    e.send(Surface::Panel, Command::Play(Place::Speaker(k.clone())));
    assert_eq!(e.snapshot().now.title, "Sound is on this PC", "nothing happens on the caller's thread");
    e.advance(0);
    assert_eq!(e.snapshot().now.title, "Connecting to Kitchen…");
    assert_eq!(row(&e.snapshot(), "Kitchen").tooltip, "Finding Kitchen…");
    e.advance(600);
    assert_eq!(row(&e.snapshot(), "Kitchen").tooltip, "Asking Kitchen to play…");
    e.advance(600);
    assert_eq!(row(&e.snapshot(), "Kitchen").tooltip, "Waiting for Kitchen to start…");
    e.advance(600);
    let s = e.snapshot();
    assert_eq!(s.now.title, "Playing on Kitchen");
    assert_eq!(s.now.volume.as_ref().unwrap().level, 32, "the speaker's own level");
    assert!(changes.load(Ordering::SeqCst) >= 4, "watchers hear every change");
    drop(watch);
    let before = changes.load(Ordering::SeqCst);
    e.send(Surface::Panel, Command::Play(Place::ThisPc));
    e.settle();
    assert_eq!(changes.load(Ordering::SeqCst), before, "a dropped watch is not called");
    assert_eq!(e.snapshot().now.title, "Sound is on this PC");

    // The latest click wins.
    e.send(Surface::Panel, Command::Play(Place::Speaker(k.clone())));
    e.advance(300);
    e.send(Surface::Tray, Command::Play(Place::Speaker(SpeakerId::new("living"))));
    e.settle();
    assert_eq!(e.snapshot().now.title, "Playing on Living Room + Dining Room");

    // Cancel keeps the sound where it was.
    e.send(Surface::Panel, Command::Play(Place::Speaker(SpeakerId::new("bedroom"))));
    e.advance(100);
    assert_eq!(e.snapshot().now.line2, "Your sound stays on Living Room + Dining Room until Bedroom is ready.");
    e.send(Surface::Panel, Command::CancelConnect);
    e.settle();
    assert_eq!(e.snapshot().now.title, "Playing on Living Room + Dining Room");

    // A speaker that isn't there fails on its row; the card is unchanged.
    e.send(Surface::Panel, Command::Play(Place::Speaker(SpeakerId::new("garage"))));
    e.settle();
    let s = e.snapshot();
    assert_eq!(s.now.title, "Playing on Living Room + Dining Room");
    assert_eq!(row(&s, "Garage").error.as_ref().unwrap().text, "Couldn’t connect — no answer.");

    // A busy speaker asks first.
    let e = ScriptedEngine::new(Scenario::OnThisPc);
    let mut f = e.facts();
    f.speakers[0].busy = true;
    e.set_facts(f);
    e.send(Surface::Panel, Command::Play(Place::Speaker(k.clone())));
    e.settle();
    assert!(row(&e.snapshot(), "Kitchen").question.is_some());
    assert_eq!(e.snapshot().now.title, "Sound is on this PC");
    e.send(Surface::Panel, Command::PlayTakingOver(k.clone()));
    e.settle();
    assert_eq!(e.snapshot().now.title, "Playing on Kitchen");

    // A code is asked for on the way.
    let e = ScriptedEngine::new(Scenario::DelayFixedOnSteady);
    e.send(Surface::Panel, Command::Play(Place::ThisPc));
    e.send(Surface::Panel, Command::Play(Place::Speaker(SpeakerId::new("den"))));
    e.advance(1300);
    assert_eq!(row(&e.snapshot(), "Den TV").tooltip, "Pairing — finish in the window that opened");

    // Music Assistant: waiting, then playing.
    let e = ScriptedEngine::new(Scenario::Playing);
    e.send(Surface::Panel, Command::Play(Place::MusicAssistant));
    e.advance(0);
    assert_eq!(e.snapshot().now.title, "Waiting for Music Assistant to play it");
    e.settle();
    assert_eq!(e.snapshot().now.title, "Playing through Music Assistant");
}

#[test]
fn scripted_engine_changes_delay_and_volume() {
    let e = ScriptedEngine::new(Scenario::Playing);
    let k = kitchen();
    let remove = |ms| Command::ChangeDelay { speaker: k.clone(), step: DelayStep::Remove(ms) };
    e.send(Surface::Panel, remove(100));
    e.advance(0);
    let s = e.snapshot();
    let c = s.now.delay.as_ref().unwrap();
    assert_eq!((c.figure.as_str(), c.change.as_deref()), ("under 0.1 s", Some("150 ms removed")));
    assert_eq!(c.line.as_ref().unwrap().text, "Removing… 100 ms to go");
    e.advance(5_000);
    let left = e.snapshot().delay.as_ref().unwrap().applying_ms_left();
    assert!(left > 0 && left < 100, "applies gradually: {left} ms left");
    e.settle();
    let s = e.snapshot();
    assert_eq!(s.now.delay.as_ref().unwrap().line, None, "done: the head line stands alone");
    assert_eq!(s.delay.as_ref().unwrap().applying_ms_left(), 0);

    // At the limit the column greys.
    e.send(Surface::Panel, remove(100));
    e.settle();
    let s = e.snapshot();
    assert_eq!(s.now.delay.as_ref().unwrap().change.as_deref(), Some("200 ms removed"));
    assert_eq!(s.now.delay.as_ref().unwrap().line.as_ref().unwrap().text, "Kitchen allows at most 200 ms removed.");

    // ↺ goes back to the speaker's own delay.
    e.send(Surface::Tray, Command::ChangeDelay { speaker: k.clone(), step: DelayStep::Reset });
    e.settle();
    assert_eq!(e.snapshot().now.delay.as_ref().unwrap().change, None);

    // Volume moves, within the limit, only for the place it was set for.
    let owner = VolumeOwner::Speaker(k.clone());
    e.send(Surface::Panel, Command::SetVolume { owner: owner.clone(), level: 45 });
    e.send(Surface::Panel, Command::SetVolume { owner: VolumeOwner::Speaker(SpeakerId::new("living")), level: 90 });
    e.settle();
    assert_eq!(e.snapshot().now.volume.as_ref().unwrap().number, "45");
    e.send(Surface::Panel, Command::SetMuted { owner, muted: true });
    e.settle();
    let s = e.snapshot();
    assert_eq!((s.now.volume.as_ref().unwrap().number.as_str(), s.now.line2.as_str()), ("Muted", "Spotify is playing · Kitchen is muted"));

    // An AirPlay change reconnects: presses gather, then a few seconds of
    // silence, then playing again.
    let e = ScriptedEngine::new(Scenario::DelaySwitchedToShort);
    let office = SpeakerId::new("office");
    let press = |ms| Command::ChangeDelay { speaker: office.clone(), step: DelayStep::Remove(ms) };
    e.send(Surface::Panel, press(25));
    e.advance(500);
    e.send(Surface::Panel, press(25));
    e.advance(0);
    let s = e.snapshot();
    assert_eq!(s.now.delay.as_ref().unwrap().line.as_ref().unwrap().text, "Office reconnects to apply this: a few seconds of silence.");
    e.advance(1_500);
    let s = e.snapshot();
    assert_eq!(s.now.line2, "Reconnecting to remove 50 ms: a few seconds of silence.");
    assert_eq!(s.status, "Playing on Office (reconnecting to remove 50 ms)");
    e.settle();
    let s = e.snapshot();
    assert_eq!((s.now.title.as_str(), s.now.line2.as_str()), ("Playing on Office", "Edge is playing"));
    assert_eq!(s.now.delay.as_ref().unwrap().change.as_deref(), Some("50 ms removed"));

    // Switching the delay choice keeps each choice's change.
    e.send(Surface::Panel, Command::UseDelayChoice { speaker: office.clone(), choice: DelayChoice::Steady });
    e.advance(0);
    let line = e.snapshot().now.delay.as_ref().unwrap().line.clone().unwrap();
    assert_eq!(line.text, "Office reconnects to apply this: a few seconds of silence.");
    e.settle();
    let s = e.snapshot();
    let c = s.now.delay.as_ref().unwrap();
    assert_eq!((c.figure.as_str(), c.change.as_deref()), ("about 1.7 s", Some("300 ms removed")));
    assert_eq!(c.line.as_ref().unwrap().text, "Steady starts with 300 ms removed. Short delay keeps its 50 ms removed.");
}

/// Every snapshot string from every surface.
fn all_text(s: &Snapshot) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |w: &str, t: &str| out.push((w.to_string(), t.to_string()));
    let volume = |v: &super::snapshot::VolumeRow, put: &mut dyn FnMut(&str, &str)| {
        put("volume", &v.number);
        put("volume", &v.spoken);
        put("volume", &v.mute.label);
        for t in v.tooltip.iter().chain(v.caption.iter()) {
            put("volume", t);
        }
    };
    let buttons = |body: &DelayBody, put: &mut dyn FnMut(&str, &str)| match body {
        DelayBody::Buttons { remove_large, remove_small, add_large, add_small } => {
            for b in [remove_large, remove_small, add_large, add_small] {
                put("delay button", &b.label);
                put("delay button", b.spoken.as_deref().unwrap_or(""));
            }
        }
        DelayBody::Fixed { text, action } => {
            put("delay", text);
            put("delay", &action.label);
        }
    };

    // The panel's top.
    let top = &s.top;
    put("sentence", &top.sentence);
    put("announce", &top.announce);
    for t in top.detail.iter().chain(top.note.iter()) {
        put("detail", t);
    }
    for b in top.actions.iter().chain(top.corner.iter()) {
        put("action", &b.label);
    }
    if let Some(v) = &top.volume {
        volume(v, &mut put);
    }
    match &top.third {
        Some(Third::Text(t)) => put("third", t),
        Some(Third::Delay(d)) => {
            for t in [&d.label, &d.value, &d.spoken, &d.spoken_total, &d.spoken_done] {
                put("panel delay", t);
            }
            buttons(&d.body, &mut put);
            if let Some(l) = &d.note {
                put("panel delay", &l.text);
                if let Some(a) = &l.action {
                    put("panel delay", &a.label);
                }
            }
            for t in d.change.iter().chain(d.other_amount.iter()) {
                put("panel delay", t);
            }
            if let Some(a) = &d.amount {
                for t in [&a.label, &a.unit, &a.spoken, &a.close, &a.remove.label, &a.add.label, &a.line] {
                    put("panel amount", t);
                }
            }
            if let Some(r) = &d.reset {
                put("panel delay", &r.label);
                put("panel delay", r.spoken.as_deref().unwrap_or_default());
            }
        }
        None => {}
    }
    if let Some(w) = &s.first_run {
        for t in [&w.sentence, &w.detail, &w.close.label] {
            put("first run", t);
        }
        for b in w.actions.iter().chain(w.link.iter()).chain(w.not_now.iter()) {
            put("first run", &b.label);
        }
        for t in w.looking_more.iter().chain(w.switch.iter().map(|s| &s.label)) {
            put("first run", t);
        }
    }

    // The card the phone remote shows.
    let n = &s.now;
    put("title", &n.title);
    put("line2", &n.line2);
    put("announce", &n.announce);
    for b in &n.actions {
        put("action", &b.label);
    }
    if let Some(t) = &n.note {
        put("note", t);
    }
    if let Some(v) = &n.volume {
        volume(v, &mut put);
    }
    if let Some(c) = &n.delay {
        for t in [&c.label, &c.figure, &c.spoken, &c.spoken_total, &c.spoken_done, &c.other_amount] {
            put("delay", t);
        }
        for t in c.column_headings.iter().chain(c.change.iter()) {
            put("delay", t);
        }
        if let Some(r) = &c.reset {
            put("delay", &r.label);
        }
        buttons(&c.body, &mut put);
        if let Some(l) = &c.line {
            put("delay line", &l.text);
            if let Some(a) = &l.action {
                put("delay line", &a.label);
            }
        }
    }

    // The list.
    let l = &s.list;
    put("list", &l.heading);
    put("list", &l.menu_name);
    for r in l.rows() {
        put("row title", &r.title);
        put("row", &r.tooltip);
        put("row spoken", &r.spoken);
        if let Some(RowStatus::Text(t)) = &r.status {
            put("row", t);
        }
        for m in &r.menu {
            put("row menu", &m.label);
        }
        if let Some(e) = &r.error {
            put("row", &e.text);
            put("row", &e.retry.label);
        }
        if let Some(q) = &r.question {
            for t in [&q.title, &q.line, &q.confirm.label, &q.cancel.label] {
                put("question", t);
            }
        }
    }
    if let Some(m) = &l.more {
        put("more", &m.title);
        put("more", &m.tooltip);
    }
    if let Some(m) = &l.music_assistant {
        put("ma", &m.heading);
    }
    if let Some(sr) = &l.searching {
        put("search", &sr.title);
        put("search", &sr.tooltip);
        if let Some(nf) = &sr.none_found {
            put("search", &nf.title);
            for c in &nf.causes {
                put("search", c);
            }
            for b in &nf.buttons {
                put("search", &b.label);
            }
        }
    }

    // The rest of the panel and the other surfaces.
    if let Some(no) = &s.notice {
        put("notice", &no.text);
        for t in no.detail.iter() {
            put("notice", t);
        }
        for b in no.actions.iter().chain(no.dismiss.iter()) {
            put("notice", &b.label);
        }
    }
    let f = &s.footer;
    for b in f.cant_hear.iter().chain([&f.speaker_missing, &f.settings]) {
        put("footer", &b.label);
    }
    put("tooltip", &s.tray.tooltip);
    for e in &s.tray.menu {
        match e {
            MenuEntry::Label(t) => put("tray", t),
            MenuEntry::Item(i) => put("tray", &i.label),
            MenuEntry::Separator => {}
        }
    }
    if let Some(d) = &s.device_name {
        put("device", d);
    }
    put("status", &s.status);
    put("status", &s.short_status);
    put("cli", &s.cli_status);
    if let Some(nt) = &s.notification {
        put("notification", &nt.title);
        put("notification", &nt.body);
        for b in &nt.buttons {
            put("notification", &b.label);
        }
    }
    out
}

/// Words the everyday surfaces never use (protocols, counters, the
/// implementation's words, and time words about the sound).
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

fn check_words(what: &str, kind: &str, text: &str) {
    check_words_allowing(what, kind, text, &[]);
}

/// `twins`: the names that may carry an address, because two entries share
/// them.
fn check_words_allowing(what: &str, kind: &str, text: &str, twins: &[String]) {
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

fn check_snapshot(what: &str, s: &Snapshot) {
    // An address appears only to tell two same-named entries apart.
    let twins: Vec<String> = s.list.rows().filter(|r| has_address(&r.title)).map(|r| r.title.clone()).collect();
    for t in &twins {
        let name = t.split(" (").next().unwrap();
        assert!(s.list.rows().filter(|o| o.title.starts_with(name)).count() >= 2, "{what}: {t}");
    }
    for (kind, text) in all_text(s) {
        check_words_allowing(what, &kind, &text, &twins);
    }
}

/// Renders every scenario on every surface, with usual, long and Chinese
/// names, plus every variant a state has; then checks that every sentence
/// in the table was used and no surface shows a forbidden word.
#[test]
fn every_state_has_words_on_every_surface_and_every_string_is_used() {
    USED.with(|u| u.borrow_mut().clear());
    let mut snapshots: Vec<(String, Snapshot)> = Vec::new();
    for &s in Scenario::ALL {
        for names in [Names::Usual, Names::Long, Names::Chinese] {
            let e = ScriptedEngine::with_names(s, names);
            snapshots.push((format!("{} {names:?}", s.name()), (*e.snapshot()).clone()));
        }
        assert_eq!(Scenario::from_name(s.name()), Some(s));
    }

    // Variants no scenario starts in.
    let k = kitchen();
    let t = LocalTime::new(21, 14);
    let variants: Vec<(&str, Scenario, Box<dyn FnOnce(&mut Facts)>)> = vec![
        ("refused pairing", Scenario::FellBack, Box::new(set_state(NowState::FellBack { speaker: k.clone(), at: t, cause: FellBackCause::RefusedPairing, returns: Return::ByItself }))),
        ("set never to return", Scenario::SpeakerBack, Box::new(set_state(NowState::SpeakerBack { speaker: k.clone(), because: StayedBecause::SetNotToReturn }))),
        ("playing when found", Scenario::SpeakerBack, Box::new(set_state(NowState::SpeakerBack { speaker: k.clone(), because: StayedBecause::PlayingWhenFound }))),
        ("password", Scenario::NeedsPairing, Box::new(|f: &mut Facts| {
            f.state = NowState::NeedsSecret { speaker: SpeakerId::new("den"), secret: Secret::Password };
            f.notification = Some(NotificationFacts { seq: 2, kind: NotificationKind::NeedsSecret(Secret::Password), speaker: Some(SpeakerId::new("den")) });
        })),
        ("silent quick tries", Scenario::NotResponding, Box::new(set_state(NowState::NotResponding { speaker: k.clone(), dropout: Dropout::QuickTries { attempt: 1, of: 3, then: AfterTries::StaySilent } }))),
        ("can't find, will ask", Scenario::CantFind, Box::new(set_state(NowState::LookingFor { speaker: k.clone(), looking: Looking::NotFound(Return::WillAsk) }))),
        ("ma away, will ask, unseen", Scenario::MaAway, Box::new(|f: &mut Facts| { f.state = NowState::MaAway { returns: Return::WillAsk }; f.music_assistant = Some(MaFacts { connected: false, last_seen: None }); })),
        ("fell back, returns", Scenario::FellBack, Box::new(set_state(NowState::FellBack { speaker: k.clone(), at: t, cause: FellBackCause::StoppedResponding, returns: Return::ByItself }))),
        ("on this PC, only sent apps", Scenario::OnThisPc, Box::new(|f: &mut Facts| f.mode = Mode::OnlySentApps { pass_through: false })),
        ("two apps", Scenario::Playing, Box::new(set_state(NowState::Playing { speaker: k.clone(), apps: Apps::of(&["Spotify", "Edge"]) }))),
        ("many apps", Scenario::SentApps, Box::new(set_state(NowState::PlayingSentApps { speaker: k.clone(), apps: Apps::of(&["Spotify", "Edge", "Chrome"]) }))),
        ("sent apps unknown", Scenario::SentApps, Box::new(set_state(NowState::PlayingSentApps { speaker: k.clone(), apps: Apps::default() }))),
        ("no app known", Scenario::MaPlaying, Box::new(set_state(NowState::MaPlaying { apps: Apps::default(), room: None }))),
        ("failures", Scenario::OnThisPc, Box::new(|f: &mut Facts| {
            f.speakers[0].failure = Some(ConnectFailure::WrongPassword);
            f.speakers[1].failure = Some(ConnectFailure::CouldntStart);
            f.speakers[2].failure = Some(ConnectFailure::CouldntReconnect);
            f.speakers[3].failure = Some(ConnectFailure::Other);
        })),
        ("other kinds", Scenario::OnThisPc, Box::new(|f: &mut Facts| {
            let base = f.speakers[0].clone();
            f.speakers.push(SpeakerFacts { id: SpeakerId::new("study"), name: "Study".into(), kind: SpeakerKind::HomePod, ..base.clone() });
            f.speakers.push(SpeakerFacts { id: SpeakerId::new("hall"), name: "Hall".into(), kind: SpeakerKind::Other { maker: None }, delay: None, ..base.clone() });
            f.speakers.push(SpeakerFacts { id: SpeakerId::new("den2"), name: "Denon".into(), kind: SpeakerKind::Other { maker: Some("Denon".into()) }, ..base });
        })),
        ("searching, several networks", Scenario::NothingFound, Box::new(|f: &mut Facts| {
            f.search = SearchFacts { phase: SearchPhase::NoneFound, network: None, causes: vec![SearchCause::FirewallBlocks] };
        })),
        ("searching on ethernet", Scenario::NothingFound, Box::new(|f: &mut Facts| {
            f.search = SearchFacts { phase: SearchPhase::Looking, network: Some(NetworkLabel { adapter: "Ethernet".into(), network: None }), causes: vec![] };
        })),
        ("connecting from music assistant", Scenario::MaPlaying, Box::new(|f: &mut Facts| {
            f.state = NowState::Connecting { target: SpeakerId::new("kitchen"), from: SoundAt::MusicAssistant, stage: ConnectStage::Finding };
        })),
        ("dropouts on short delay", Scenario::DelaySwitchedToShort, Box::new(|f: &mut Facts| f.delay.as_mut().unwrap().note = Some(DelayNote::Dropouts { count: 1 }))),
        ("not accepted, back to own", Scenario::DelayNotAccepted, Box::new(|f: &mut Facts| f.delay.as_mut().unwrap().note = Some(DelayNote::NotAccepted { tried_ms: -100, back_to_ms: 0 }))),
        ("dropped out to own", Scenario::DelayDroppedOut, Box::new(|f: &mut Facts| f.delay.as_mut().unwrap().note = Some(DelayNote::DroppedOut { with_ms: -50, back_to_ms: 0 }))),
        ("switched to short with", Scenario::DelaySwitchedToShort, Box::new(|f: &mut Facts| {
            f.speakers.iter_mut().find(|s| s.id.as_str() == "office").unwrap().delay.as_mut().unwrap().change_ms = -100;
        })),
        ("switched to steady", Scenario::DelaySteadyRemove, Box::new(|f: &mut Facts| f.delay.as_mut().unwrap().note = Some(DelayNote::ChoiceSwitched { other_kept_ms: 0 }))),
        ("added limit", Scenario::Playing, Box::new(|f: &mut Facts| f.speakers[0].delay.as_mut().unwrap().change_ms = 1000)),
        ("measured", Scenario::Playing, Box::new(|f: &mut Facts| f.speakers[0].delay = Some(SpeakerDelay { own_ms: 210, measured: true, change_ms: -50 }))),
        ("another PC plays there", Scenario::BusyQuestion, Box::new(|f: &mut Facts| f.speakers[0].question = Some(TakeOver { other_pc: Some("LAPTOP-ANNA".into()) }))),
        ("switched to steady, nothing", Scenario::DelaySteadyRemove, Box::new(|f: &mut Facts| {
            f.speakers.iter_mut().find(|s| s.id.as_str() == "office").unwrap().delay.as_mut().unwrap().change_ms = 0;
            f.delay.as_mut().unwrap().note = Some(DelayNote::ChoiceSwitched { other_kept_ms: -50 });
        })),
        ("restart notification", Scenario::OnThisPc, Box::new(|f: &mut Facts| f.notification = Some(NotificationFacts { seq: 3, kind: NotificationKind::RestartToFinish, speaker: None }))),
        ("ma not connected, not chosen", Scenario::Playing, Box::new(|f: &mut Facts| f.music_assistant = Some(MaFacts { connected: false, last_seen: None }))),
    ];
    for (what, s, change) in variants {
        snapshots.push((what.to_string(), snap_with(s, change)));
    }

    // Engine flows: every step of connecting, reconnecting and switching.
    let mut flows: Vec<(String, Arc<Snapshot>)> = Vec::new();
    let mut step = |e: &ScriptedEngine, what: &str, commands: Vec<Command>| {
        for c in commands {
            e.send(Surface::Panel, c);
        }
        for i in 0..40 {
            e.advance(250);
            flows.push((format!("{what} +{i}"), e.snapshot()));
        }
    };
    let e = ScriptedEngine::new(Scenario::OnThisPc);
    step(&e, "connect", vec![Command::Play(Place::Speaker(k.clone()))]);
    let e = ScriptedEngine::new(Scenario::DelayFixedOnSteady);
    step(&e, "pair", vec![Command::Play(Place::ThisPc), Command::Play(Place::Speaker(SpeakerId::new("den")))]);
    let office = SpeakerId::new("office");
    let e = ScriptedEngine::new(Scenario::DelaySwitchedToShort);
    step(&e, "reconnect add", vec![Command::ChangeDelay { speaker: office.clone(), step: DelayStep::Add(25) }]);
    step(&e, "reconnect own", vec![Command::ChangeDelay { speaker: office.clone(), step: DelayStep::Reset }]);
    step(&e, "switch", vec![Command::UseDelayChoice { speaker: office.clone(), choice: DelayChoice::Steady }]);
    step(&e, "switch back", vec![Command::UseDelayChoice { speaker: office.clone(), choice: DelayChoice::Short }]);
    step(&e, "switch with", vec![
        Command::ChangeDelay { speaker: office.clone(), step: DelayStep::Remove(25) },
        Command::UseDelayChoice { speaker: office.clone(), choice: DelayChoice::Steady },
    ]);
    let e = ScriptedEngine::new(Scenario::Playing);
    step(&e, "to music assistant", vec![Command::Play(Place::MusicAssistant)]);
    for (what, s) in &flows {
        snapshots.push((what.clone(), (**s).clone()));
    }

    for (what, s) in &snapshots {
        check_snapshot(what, s);
        // Every snapshot can answer the command line.
        check_words(what, "cli", &cli_delay_info(EN, s).text);
    }

    // Outcomes: every delay reply and confirmation.
    let mut outcomes = Vec::new();
    let steps = [DelayStep::Remove(25), DelayStep::Remove(400), DelayStep::Add(25), DelayStep::Add(2000), DelayStep::SetTotal(-150), DelayStep::SetTotal(-5000), DelayStep::SetTotal(0), DelayStep::Reset];
    for s in [Scenario::Playing, Scenario::DelaySendspin, Scenario::DelaySteadyRemove, Scenario::DelayFixedOnSteady, Scenario::OnThisPc, Scenario::MaPlaying] {
        for change in [-200, -25, 0, 25, 800, 1000] {
            for step in steps {
                outcomes.push(delay_outcome(s, change, step));
            }
        }
    }
    let e = ScriptedEngine::new(Scenario::OnThisPc);
    let unknown = e.send(Surface::CommandLine, Command::Play(Place::Speaker(SpeakerId::new("nowhere"))));
    e.advance(0);
    outcomes.push(unknown.try_outcome().unwrap());
    let refused = Outcome {
        result: Err(Refusal::OnlyFromWindow { speaker: kitchen() }),
        snapshot: snap(Scenario::OnThisPc),
    };
    assert_eq!(cli_reply(EN, &refused).text, "To play to “Kitchen”, pick it in the Stream To Speaker window.");
    outcomes.push(refused);
    for o in &outcomes {
        check_words("cli reply", "cli", &cli_reply(EN, o).text);
        let c = confirmation(EN, o);
        for t in [&c.over, &c.big, &c.small] {
            check_words("confirmation", "confirmation", t);
        }
    }

    // The typed amount, both ways, at every point of the range.
    for s in [Scenario::Playing, Scenario::DelaySendspin] {
        for change in [-200, -150, -50, 0, 50, 800, 850, 1000] {
            let snapshot = snap_with(s, |f| {
                let id = f.delay.as_ref().unwrap().speaker.clone();
                f.speakers.iter_mut().find(|x| x.id == id).unwrap().delay.as_mut().unwrap().change_ms = change;
            });
            let view = snapshot.delay.clone().unwrap();
            for typed in [None, Some(0), Some(25), Some(50), Some(150), Some(300), Some(1000)] {
                for enter in [Enter::Remove, Enter::Add] {
                    let f = amount_field(EN, &view, typed, enter);
                    for t in [&f.line, &f.remove.label, &f.add.label, &f.label, &f.unit, &f.spoken, &f.close] {
                        check_words("amount", "amount", t);
                    }
                    for t in &f.phone_lines {
                        check_words("amount", "amount", t);
                    }
                }
            }
        }
    }

    // The sentences other programs show for the app.
    for t in [
        surfaces::device_name_not_running(EN),
        surfaces::phone_cant_reach(EN, "DESKTOP-7Q", "21:14:06"),
        surfaces::phone_link_revoked(EN),
        surfaces::cli_not_running(EN),
        surfaces::cli_no_reply(EN),
        surfaces::picker_prompt(EN, 3),
    ] {
        check_words("other", "other", &t);
    }
    let _ = surfaces::picker_header(EN, "0.2.0", "Wi-Fi");

    let used = USED.with(|u| u.borrow().clone());
    let unused: Vec<&Key> = Key::ALL.iter().filter(|k| !used.contains(k)).collect();
    assert!(unused.is_empty(), "sentences no surface uses: {unused:?}");
}

/// A language with another word order, other plural forms and a decimal
/// comma gets them from its table: the model composes no sentence itself.
#[test]
fn another_language_orders_its_own_words() {
    let fell_back = snap_with(Scenario::FellBack, |f| f.lang = Lang::Test);
    assert_eq!(fell_back.now.line2, "21.14 — Speakers (Realtek Audio): Kitchen stopped.");
    let playing = snap_with(Scenario::Playing, |f| f.lang = Lang::Test);
    let c = playing.now.delay.as_ref().unwrap();
    assert_eq!((c.figure.as_str(), c.change.as_deref()), ("≈0,15 s", Some("removed: 50")));
    for (count, want) in [
        (1, "100 for Kitchen? 1 drop (one)"),
        (3, "100 for Kitchen? 3 drops (few)"),
        (5, "100 for Kitchen? 5 drops (many)"),
        (11, "100 for Kitchen? 11 drops (many)"),
        (21, "100 for Kitchen? 21 drop (one)"),
        (22, "100 for Kitchen? 22 drops (few)"),
    ] {
        let s = snap_with(Scenario::DelayDropouts, |f| {
            f.lang = Lang::Test;
            f.delay.as_mut().unwrap().note = Some(DelayNote::Dropouts { count });
        });
        assert_eq!(s.now.delay.as_ref().unwrap().line.as_ref().unwrap().text, want);
    }
}

/// Waiting on a ticket is for the command line's thread; it times out
/// cleanly when nothing handles the command.
#[test]
fn a_ticket_waits_only_as_long_as_asked() {
    let e = ScriptedEngine::new(Scenario::Playing);
    let ticket = e.send(Surface::CommandLine, Command::Play(Place::ThisPc));
    assert!(ticket.wait(Duration::from_millis(1)).is_none());
    e.advance(0);
    assert!(ticket.wait(Duration::from_millis(1)).is_some());
    assert_eq!(e.received().len(), 1);
}

/// Two ways to one speaker are two rows, next to each other, the way used
/// last first, each with its kind and delay on the second line; neither
/// carries an address or the amber icon.
#[test]
fn twins_are_two_rows_side_by_side() {
    let snapshot = snap(Scenario::Twins);
    let rows: Vec<(String, String, String)> = snapshot
        .list
        .speakers
        .iter()
        .map(|r| {
            let id = match &r.place {
                Place::Speaker(id) => id.as_str().to_string(),
                _ => String::new(),
            };
            (id, r.title.clone(), r.second.clone())
        })
        .collect();
    let at = |id: &str| rows.iter().position(|(i, ..)| i == id).unwrap_or_else(|| panic!("{id}: {rows:?}"));
    // Kitchen was played last by AirPlay: that row first.
    assert_eq!(at("kitchen-airplay") + 1, at("kitchen"), "{rows:?}");
    assert_eq!(rows[at("kitchen-airplay")].2, "AirPlay · about 2 s delay");
    assert_eq!(rows[at("kitchen")].2, "Sonos · about 0.15 s delay");
    assert_eq!(rows[at("kitchen")].1, "Kitchen", "no address: these are one speaker");
    // Office, never played from here: the way with less delay first.
    assert_eq!(at("office-upnp") + 1, at("office"), "{rows:?}");
    assert_eq!(rows[at("office-upnp")].2, "Denon · about 0.3 s delay");
    for r in &snapshot.list.speakers {
        assert_ne!(r.status, Some(RowStatus::Attention), "{}", r.title);
    }
    // Each is its own place: the AirPlay way plays, the Sonos way is a
    // choice of its own.
    let k = kitchen();
    let sonos_way = snapshot.list.speakers.iter().find(|r| r.place == Place::Speaker(k.clone())).unwrap();
    assert_eq!(sonos_way.command, Some(Command::Play(Place::Speaker(k))));
}

/// Every speaker row has its kind and delay as a second line; This PC has
/// the output in use; Music Assistant what it does.
#[test]
fn rows_have_their_second_line() {
    let snapshot = snap(Scenario::OnThisPc);
    assert_eq!(snapshot.list.this_pc.second, "Speakers (Realtek Audio)");
    let living = snapshot.list.speakers.iter().find(|r| r.title.starts_with("Living Room")).unwrap();
    assert_eq!(living.second, "Sonos group · about 0.2 s delay");
    let office = snapshot.list.speakers.iter().find(|r| r.title == "Office").unwrap();
    assert_eq!(office.second, "AirPlay · about 2 s delay");
    // A state's words stay the tooltip; the second line stays the kind.
    let trying = snap(Scenario::NotResponding);
    let k = trying.list.speakers.iter().find(|r| r.place == Place::Speaker(kitchen())).unwrap();
    assert_eq!(k.second, "Sonos · about 0.15 s delay");
    assert_ne!(k.tooltip, k.second);
    assert!(!snapshot.list.music_assistant.as_ref().unwrap().row.second.is_empty());
}

/// This PC's outputs: a chevron when there is more than one; open, every
/// output with its device under This PC, the one in use checked; picking
/// another makes it This PC's output and moves the sound there.
#[test]
fn this_pcs_outputs_can_be_chosen() {
    let closed = snap(Scenario::PcOutputsClosed);
    let chevron = closed.list.this_pc.chevron.as_ref().unwrap();
    assert!(!chevron.open);
    assert_eq!(chevron.command, Command::ShowPcOutputs(true));
    assert_eq!(chevron.spoken, "Show this PC’s outputs");
    let open = snap(Scenario::PcOutputsOpen);
    let chevron = open.list.this_pc.chevron.as_ref().unwrap();
    assert!(chevron.open);
    assert_eq!(chevron.command, Command::ShowPcOutputs(false));
    let rows: Vec<(&str, &str, bool)> =
        open.list.pc_outputs.iter().map(|o| (o.title.as_str(), o.second.as_str(), o.checked)).collect();
    assert_eq!(
        rows,
        [
            ("Speakers", "Realtek Audio", true),
            ("Headphones", "Realtek Audio", false),
            ("LG TV", "NVIDIA High Definition Audio", false)
        ]
    );
    assert_eq!(open.list.pc_outputs[0].command, None, "the sound is there already");
    assert_eq!(open.list.pc_outputs[1].command, Some(Command::PlayOnPcOutput(PcOutputId::new("headphones"))));
    assert_eq!(open.list.pc_outputs[2].icon, crate::model::places::RowIcon::Display);
    // One output: no chevron, no list.
    let one = snap(Scenario::OnThisPc);
    assert!(one.list.this_pc.chevron.is_none() && one.list.pc_outputs.is_empty());

    // The scripted engine: picking the headphones moves This PC there.
    let engine = ScriptedEngine::new(Scenario::PcOutputsOpen);
    engine.send(Surface::Panel, Command::PlayOnPcOutput(PcOutputId::new("headphones")));
    engine.settle();
    let after = engine.snapshot();
    assert_eq!(after.list.this_pc.second, "Headphones (Realtek Audio)");
    assert!(after.list.pc_outputs[1].checked);
    engine.send(Surface::Panel, Command::ShowPcOutputs(false));
    engine.settle();
    assert!(!engine.snapshot().list.this_pc.chevron.as_ref().unwrap().open);
}
