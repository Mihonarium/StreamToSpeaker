use super::*;
use crate::airplay::fake_raop::{Answer, FakeRaop};
use crate::model::facts::{Applies, Level, Progress, SearchPhase};
use crate::engine::place_driver::scene::CANT_FIND_AFTER_MS;
use crate::model::situation::{ConnKind, RowState};
use crate::user_config::SpeakerSettings;

#[test]
fn folded_sonos_and_airplay_show_the_figures_for_each_choice() {
    let ap = FakeRaop::start(|_| Answer::Normal);
    let fake =
        FakeRenderer::start(|_| Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])));
    let found = DiscoveryState::new();
    found.replace(vec![crate::ssdp::fetch_and_parse_device(
        &fake.url("/description.xml"),
        WAIT,
        ServiceUrls::AsGiven,
    )
    .unwrap()]);
    let mut cfg = UserConfig::in_memory();
    cfg.speaker_settings.insert(
        DEN.into(),
        SpeakerSettings {
            delay_change_ms: -35,
            show_as: Some("Kitchen".into()),
            last_played: Some(12),
            ..Default::default()
        },
    );
    let airplay = "airplay:0A1B2C3D4E5F";
    cfg.speaker_settings.insert(
        airplay.into(),
        SpeakerSettings {
            delay_choice: Some(crate::delay::DelayChoice::Steady),
            delay_change_ms: 75,
            steady_delay_change_ms: -150,
            ..Default::default()
        },
    );
    let mut r = connections::airplay_rig(&ap, cfg, Some(found));
    assert_eq!(super::super::speaker_for_pick(&r.app, airplay), id());
    for choice in [
        crate::delay::DelayChoice::Steady,
        crate::delay::DelayChoice::Short,
    ] {
        r.app.set_delay_choice(airplay, choice).unwrap();
        let scene = r.driver.scene();
        assert_eq!(scene.speakers.len(), 1);
        let s = &scene.speakers[0];
        assert_eq!(
            (&s.id, &s.kind, s.name.as_str()),
            (&id(), &SpeakerKind::Sonos, "Kitchen")
        );
        assert_eq!(s.address.as_deref(), Some("127.0.0.1"));
        assert!(s.used_here);
        assert_eq!(s.last_used, Some(12));
        assert_eq!(s.way, WayChoice::Native);
        assert_eq!(
            s.airplay,
            Some(if choice == crate::delay::DelayChoice::Short {
                WayChoice::AirPlayShort
            } else {
                WayChoice::AirPlaySteady
            })
        );
        let figures: Vec<_> = s
            .ways
            .iter()
            .map(|w| {
                (
                    w.way,
                    w.delay.own_ms,
                    w.delay.change_ms,
                    w.range.max_removed_ms,
                    w.range.max_added_ms,
                    w.applies,
                )
            })
            .collect();
        assert_eq!(
            figures,
            [
                (WayChoice::Native, 200, -35, 200, 1000, Applies::Live),
                (
                    WayChoice::AirPlayShort,
                    300,
                    75,
                    280,
                    699,
                    Applies::Reconnect
                ),
                (
                    WayChoice::AirPlaySteady,
                    2000,
                    -150,
                    1000,
                    1000,
                    Applies::Reconnect
                ),
            ]
        );
    }
}

#[test]
fn hidden_speaker_is_visible_at_its_place_and_while_named() {
    let fake = FakeRenderer::start(answer);
    let mut cfg = UserConfig::in_memory();
    cfg.speaker_settings.insert(
        DEN.into(),
        SpeakerSettings {
            hidden: true,
            ..Default::default()
        },
    );
    let mut r = Rig::new(&fake, cfg);
    assert!(r.driver.scene().speakers.is_empty());
    r.pick();
    r.settle();
    let scene = r.driver.scene();
    assert_eq!(scene.situation.place_speaker(), Some(&id()));
    assert_eq!(scene.speakers[0].id, id());
    r.send(Command::StayOnThisPc);
    r.settle();
    assert_eq!(r.driver.core.named(), Some(&id()));
    assert_eq!(r.driver.scene().speakers[0].id, id());
}

#[test]
fn absent_speakers_follow_their_network_except_for_the_place_and_name() {
    let mut cfg = remembered();
    cfg.sound_place.as_mut().unwrap().named = Some(SpeakerId::new("named"));
    for (key, net) in [(DEN, "old"), ("home", HOME_NET), ("other", "old")] {
        cfg.speaker_settings.insert(
            key.into(),
            SpeakerSettings {
                network: Some(NetId(net.into())),
                show_as: Some(format!("Room {key}")),
                last_played: Some(8),
                ..Default::default()
            },
        );
    }
    let mut r = Rig::on(App::with_settings(App::test_config(), cfg, None, None));
    let scene = r.driver.scene();
    assert_eq!(scene.situation.place_speaker(), Some(&id()));
    assert_eq!(
        scene
            .speakers
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["home", "named", DEN]
    );
    assert!(scene.speakers.iter().all(|s| s.ways.is_empty()));
    assert_eq!(scene.speakers[0].name, "Room home");
    assert_eq!(scene.speakers[0].last_used, Some(8));
    assert_eq!(scene.speakers[1].name, "named");
    // Startup grace ends. The rules can now move off the old network's place.
    r.advance(place::numbers::AFTER_WAKE_MS + 1);
    assert_eq!(
        r.driver
            .scene()
            .situation
            .speaker(&SpeakerId::new("home"))
            .unwrap()
            .state,
        RowState::NotFound
    );
}

#[test]
fn enabled_music_assistant_has_its_own_row() {
    let mut r = Rig::on(App::for_tests());
    r.app.set_sendspin_source_enabled(true).unwrap();
    let scene = r.driver.scene();
    r.app.set_sendspin_source_enabled(false).unwrap();
    assert_eq!(scene.speakers.len(), 1);
    assert_eq!(scene.speakers[0].id, SpeakerId::music_assistant());
    assert_eq!(scene.speakers[0].name, "Music Assistant");
    assert!(scene.speakers[0].ways.is_empty());
    assert!(scene
        .situation
        .speaker(&SpeakerId::music_assistant())
        .is_some());
}

#[test]
fn windows_outputs_and_network_kind_follow_the_observation() {
    let mut r = Rig::on(App::for_tests());
    r.windows.set_output_on(HEADPHONES, true);
    assert!(!r.driver.scene().outputs_open);
    r.send(Command::ShowPcOutputs(true));
    let scene = r.driver.scene();
    assert!(scene.outputs_open);
    assert_eq!(
        scene
            .outputs
            .iter()
            .map(|o| o.id.clone())
            .collect::<Vec<_>>(),
        [output(SPEAKERS), output(HEADPHONES)]
    );
    assert_eq!(scene.network, Some(ConnKind::WiFi));
    let conn = |id: &str, kind| crate::platform::NetConnection {
        id: id.into(),
        name: None,
        kind,
    };
    r.windows.connect_to(vec![
        conn("wire", ConnKind::Wired),
        conn("vpn", ConnKind::Other),
    ]);
    assert_eq!(r.driver.scene().network, Some(ConnKind::Wired));
    r.windows.connect_to(vec![
        conn("one", ConnKind::WiFi),
        conn("two", ConnKind::WiFi),
    ]);
    assert_eq!(r.driver.scene().network, None);
}

#[test]
fn level_and_delay_progress_belong_to_the_place() {
    let fake = FakeRenderer::start(answer);
    let mut cfg = UserConfig::in_memory();
    cfg.speaker_settings.insert(
        DEN.into(),
        SpeakerSettings {
            volume_limit: Some(60),
            ..Default::default()
        },
    );
    let mut r = Rig::new(&fake, cfg);
    r.playing();
    r.app.vsync.window_changed(38);
    r.app.vsync.note_muted(true);
    r.app
        .change_delay(Some(DEN), crate::delay::Request::Remove(75))
        .unwrap();
    let s = r.driver.scene();
    assert_eq!(s.volume.pc, Some(Level::at(40)));
    assert_eq!(
        s.volume.place,
        Some(Level {
            value: 38,
            muted: true,
            limit: Some(60)
        })
    );
    assert_eq!(s.progress, Progress::Live { to_go_ms: -75 });
    r.send(Command::StayOnThisPc);
    assert_eq!(r.driver.scene().progress, Progress::Settled);
    assert_eq!(r.driver.scene().volume.place, None);
    r.settle();
}

#[test]
fn discovery_search_times_out_then_clears_on_a_find() {
    let found = DiscoveryState::new();
    let mut r = Rig::on(App::with_settings(
        App::test_config(),
        UserConfig::in_memory(),
        Some(found.clone()),
        None,
    ));
    assert_eq!(r.driver.scene().search.phase, SearchPhase::Looking);
    r.driver.skip(CANT_FIND_AFTER_MS);
    assert_eq!(r.driver.scene().search.phase, SearchPhase::NoneFound);
    let fake = FakeRenderer::start(answer);
    found.replace(vec![crate::ssdp::fetch_and_parse_device(
        &fake.url("/description.xml"),
        WAIT,
        ServiceUrls::AsGiven,
    )
    .unwrap()]);
    r.advance(1000);
    assert_eq!(r.driver.scene().search, Default::default());
}
