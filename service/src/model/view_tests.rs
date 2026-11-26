use super::{by_clock, panel, samples, view::view, SpeakerId};

#[test]
fn every_sample_supplies_all_the_surfaces() {
    for sample in samples::all() {
        let s = sample.scene;
        let v = view(s.clone(), None, false, 7);
        assert_eq!(v.version, 7);
        assert_eq!(v.lang, s.lang);
        assert_eq!(v.scene, s, "{}", sample.name);
        assert_eq!(v.panel, panel::panel(&s));
        assert_eq!(v.tray, by_clock::tray(&s));
        assert_eq!(v.popup, by_clock::popup(&s));
        assert_eq!(v.confirmation, by_clock::confirmation(&s));
        assert_eq!(v.window, by_clock::window_words(&s));
    }
}

#[test]
fn windows_name_follows_the_named_speaker_and_device_presence() {
    let s = samples::named("pc-plain");
    let name = |id: Option<&SpeakerId>, missing| view(s.clone(), id, missing, 1).device_name;
    assert_eq!(name(None, false).as_deref(), Some("Stream To Speaker"));
    assert_eq!(
        name(Some(&samples::kitchen()), false).as_deref(),
        Some("Kitchen (Stream To Speaker)")
    );
    assert_eq!(
        name(Some(&SpeakerId::music_assistant()), false).as_deref(),
        Some("Music Assistant (Stream To Speaker)")
    );
    assert_eq!(name(Some(&samples::kitchen()), true), None);
}

#[test]
fn short_and_cli_status_use_the_visible_panel_words() {
    for (sample, status, cli) in [
        ("kitchen-playing", "Kitchen: Playing", "Kitchen: Playing · Sonos · about 150 ms behind the picture · volume 32"),
        ("kitchen-muted", "Kitchen: Playing", "Kitchen: Playing · Sonos · about 150 ms behind the picture · muted"),
        ("office-playing", "Office: Playing", "Office: Playing · Office is about 150 ms behind the picture. · volume 32"),
        ("pc-plain", "This PC · Speakers: Playing", "This PC · Speakers: Playing · volume 40"),
        ("pc-headphones-connected", "This PC · Headphones: Playing", "This PC · Headphones: Playing · volume 40"),
        ("music-assistant-playing", "Music Assistant: Playing", "Music Assistant: Playing · Music Assistant is about 150 ms behind the picture. · volume 32"),
    ] {
        let v = view(samples::named(sample), None, false, 1);
        assert_eq!(v.status, status, "{sample}");
        assert_eq!(v.cli_status, cli, "{sample}");
    }
}

#[test]
fn cli_replies_use_the_published_place_and_delay() {
    use super::command::{DelayOutcome, DelayStep, Effect, Outcome};
    use super::view::cli_reply;
    use std::sync::Arc;
    let snapshot = Arc::new(view(samples::named("kitchen-removing"), None, false, 7));
    let reply = |effect| cli_reply(snapshot.lang, &Outcome { snapshot: snapshot.clone(), result: Ok(effect) });
    assert_eq!(reply(Effect::Done).text, snapshot.lang.sentence(&snapshot.status));
    let delay = DelayOutcome { speaker: samples::kitchen(), step: DelayStep::Remove(25), before_ms: -25, after_ms: -50, stopped_at_limit: false };
    assert_eq!(reply(Effect::Delay(delay.clone())).text, "Kitchen: removed 25 ms — 50 ms removed in all. Removing…");
    let mut scene = samples::named("kitchen-change-stops-sound");
    scene.progress = super::facts::Progress::ReconnectPending;
    let snapshot = Arc::new(view(scene, None, false, 8));
    let reply = cli_reply(snapshot.lang, &Outcome { snapshot: snapshot.clone(), result: Ok(Effect::Delay(delay)) });
    assert_eq!(reply.text, "Kitchen: removed 25 ms — 50 ms removed in all. Applying it: the sound stops for a few seconds.");
}

#[test]
fn first_run_does_not_change_the_panel() {
    for sample in ["pc-welcome-pick", "pc-welcome-chime", "kitchen-welcome-done"] {
        let mut s = samples::named(sample);
        for speaker in &mut s.speakers { speaker.used_here = false; }
        let first = panel::panel(&s);
        s.situation.first_run = None;
        assert_eq!(first, panel::panel(&s));
    }
}

#[test]
fn every_surface_word_is_used() {
    use super::{command::*, facts::Progress, text::{Key, Lang, USED}, view::*};
    use std::sync::Arc;
    USED.with(|used| used.borrow_mut().clear());
    super::scene_tests::check_all_scenes();
    crate::settings::views::tests::draw_welcome_words();
    for sample in samples::all() { view(sample.scene, Some(&samples::kitchen()), false, 1); }
    device_name_not_running(Lang::English);
    for pc in [None, Some("Laptop")] { busy_reply(Lang::English, "Kitchen", pc); }
    let mut scene = samples::named("kitchen-playing");
    let k = samples::kitchen();
    for progress in [Progress::Settled, Progress::Live { to_go_ms: -25 }, Progress::Live { to_go_ms: 25 }, Progress::ReconnectPending] {
        scene.progress = progress;
        for limit in [0, 100] {
            scene.speakers.iter_mut().find(|s| s.id == k).unwrap().ways[0].range.max_removed_ms = limit;
            let snapshot = Arc::new(view(scene.clone(), None, false, 1));
            let reply = |result| cli_reply(snapshot.lang, &Outcome { result, snapshot: snapshot.clone() });
            for step in [DelayStep::Reset, DelayStep::Remove(25), DelayStep::Add(25), DelayStep::SetTotal(0), DelayStep::SetTotal(100)] {
                for (before_ms, after_ms, stopped_at_limit) in [(25, 25, true), (25, 0, false), (0, -25, false), (0, 25, false), (0, 25, true)] {
                    reply(Ok(Effect::Delay(DelayOutcome { speaker: k.clone(), step, before_ms, after_ms, stopped_at_limit })));
                }
            }
            for why in [Refusal::NoDelayHere, Refusal::MusicAssistantDelay, Refusal::FixedOnSteady { speaker: k.clone() },
                Refusal::UnknownPlace { name: "Office".into() }, Refusal::OnlyFromWindow { speaker: k.clone() }] {
                reply(Err(why));
            }
        }
    }
    for kind in [super::facts::SpeakerKind::HomePod, super::facts::SpeakerKind::HomePodPair, super::facts::SpeakerKind::AppleTv,
        super::facts::SpeakerKind::SonosGroup, super::facts::SpeakerKind::Other { maker: None }, super::facts::SpeakerKind::Sendspin] {
        crate::settings::words::kind_short(Lang::English, &kind);
    }
    let unused: Vec<_> = USED.with(|used| Key::ALL.iter()
        .filter(|k| !crate::settings::words::is_settings_key(**k) && !used.borrow().contains(k)).collect());
    assert!(unused.is_empty(), "sentences no surface uses: {unused:?}");
}
