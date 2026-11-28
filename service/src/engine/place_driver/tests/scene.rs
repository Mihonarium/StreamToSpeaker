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
    assert_eq!(scene.search.phase, SearchPhase::Off);
    let ma = SpeakerId::music_assistant();
    assert!(!scene.situation.speaker(&ma).unwrap().can_chime);
    r.send(Command::Switch { place: Place::MusicAssistant, way: None });
    r.settle();
    assert!(r.driver.scene().situation.speaker(&ma).unwrap().can_chime);
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.app.user_config.lock().unwrap().delay_shortcuts = true;
    r.driver.look();
    assert_eq!(r.driver.situation().doing, Doing::Playing);
    let keys = crate::platform::DelayKeys::default();
    assert!(r.windows.press_keys(&keys.0[1]));
    r.driver.input(r.rx.recv_timeout(WAIT).unwrap());
    assert_eq!(r.app.delay_status_of(crate::sendspin_app::MA_OUTPUT_ID).change_ms, 0);
    r.send(Command::StayOnThisPc);
    r.settle();
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
fn music_assistant_follows_the_network_it_connected_on() {
    use crate::sendspin::source::SourceState;
    let mut r = Rig::on(App::for_tests());
    r.app.set_sendspin_source_enabled(true).unwrap();
    let ma = SpeakerId::music_assistant();
    let listed = |r: &mut Rig| r.driver.scene().speakers.iter().any(|s| s.id.is_music_assistant());
    let network = |r: &Rig| {
        let cfg = r.app.user_config.lock().unwrap();
        cfg.speaker_settings.get(crate::sendspin_app::MA_OUTPUT_ID).and_then(|s| s.network.clone())
    };
    let join = |r: &mut Rig, net: &str| {
        r.windows.connect_to(vec![crate::platform::NetConnection { id: net.into(), name: None, kind: ConnKind::WiFi }]);
        r.driver.input(PlaceInput::System(SystemEvent::NetworkChanged));
        r.driver.look();
    };
    let connected = |r: &mut Rig, on: bool| {
        r.app.set_sendspin_source_state(if on {
            SourceState::Connected { server: "Server".into(), streaming: false }
        } else {
            SourceState::Waiting
        });
        r.driver.look();
    };

    // Before it has ever connected, its row is there on any network.
    join(&mut r, "cafe");
    assert!(listed(&mut r));
    assert_eq!(network(&r), None);

    join(&mut r, HOME_NET);
    connected(&mut r, true);
    assert_eq!(network(&r), Some(NetId(HOME_NET.into())));

    // This PC moves and the connection hasn't ended yet: that says nothing
    // about the new network.
    join(&mut r, "cafe");
    assert!(listed(&mut r));
    assert_eq!(network(&r), Some(NetId(HOME_NET.into())));

    // The connection ends. Music Assistant isn't a choice here.
    connected(&mut r, false);
    assert!(!listed(&mut r));

    // At home it is one that isn't found.
    join(&mut r, HOME_NET);
    r.advance(place::numbers::AFTER_WAKE_MS + 1);
    assert_eq!(r.driver.scene().situation.speaker(&ma).unwrap().state, RowState::NotFound);

    // Picked at the cafe, it keeps its row there: it is the place, and
    // after that the speaker the app's device in Windows is named after.
    // That doesn't make the cafe its network.
    join(&mut r, "cafe");
    assert!(!listed(&mut r));
    r.send(Command::Switch { place: Place::MusicAssistant, way: None });
    r.settle();
    assert!(listed(&mut r));
    r.send(Command::StayOnThisPc);
    r.settle();
    assert!(listed(&mut r));
    assert_eq!(network(&r), Some(NetId(HOME_NET.into())));

    // A Music Assistant that connects at the cafe belongs there from then on.
    connected(&mut r, true);
    assert_eq!(network(&r), Some(NetId("cafe".into())));

    // The same server connecting again before its earlier connection has
    // ended arrives on the network this PC is on by then.
    join(&mut r, HOME_NET);
    assert_eq!(network(&r), Some(NetId("cafe".into())));
    connected(&mut r, true);
    assert_eq!(network(&r), Some(NetId(HOME_NET.into())));
    connected(&mut r, false);
    assert!(listed(&mut r));
    r.app.set_sendspin_source_enabled(false).unwrap();
}

#[test]
fn music_assistant_picked_before_a_server_connects_has_no_network() {
    let mut r = Rig::on(App::for_tests());
    r.app.set_sendspin_source_enabled(true).unwrap();
    r.send(Command::Switch { place: Place::MusicAssistant, way: None });
    r.settle();
    r.send(Command::StayOnThisPc);
    r.settle();
    let network = {
        let cfg = r.app.user_config.lock().unwrap();
        cfg.speaker_settings.get(crate::sendspin_app::MA_OUTPUT_ID).and_then(|s| s.network.clone())
    };
    assert_eq!(network, None);
    r.app.set_sendspin_source_enabled(false).unwrap();
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

#[test]
fn first_run_without_speaker_search_starts_with_search_off() {
    let mut r = Rig::on(App::for_tests());
    let scene = r.driver.scene();
    assert!(scene.speakers.is_empty() && scene.situation.first_run.is_some());
    assert_eq!(scene.search.phase, SearchPhase::Off);
}

#[test]
fn disabled_output_uses_visibility_and_recovers_without_repair() {
    use crate::model::scene::RepairNow;
    let mut r = Rig::on(App::for_tests());
    r.windows.set_output_on(OURS, false);
    r.driver.look();
    assert_eq!(r.driver.scene().repair, RepairNow::TurnedOff);
    r.send(Command::RepairOutput);
    r.windows.set_protected_install(false);
    assert_eq!(r.driver.scene().repair, RepairNow::TurnedOff);
    r.windows.refuse_turn_on(true);
    r.send(Command::TurnOnOutput);
    r.settle();
    assert_eq!(r.driver.scene().repair, RepairNow::TurnOnFailed);
    assert_eq!(r.windows.recovery_calls(), (1, 0));

    r.windows.set_our_output_present(false);
    r.driver.look();
    assert_eq!(r.driver.scene().repair, RepairNow::Unavailable);
    r.windows.set_our_output_present(true);
    r.windows.refuse_turn_on(false);
    r.driver.look();
    r.send(Command::TurnOnOutput);
    r.driver.look();
    let scene = r.driver.scene();
    assert!(!scene.situation.standing.contains(&Standing::OutputMissing));
    assert_eq!(scene.repair, RepairNow::Worked);
    assert_eq!(r.windows.recovery_calls(), (2, 0));
}

#[test]
fn per_app_repair_returns_multimedia_when_windows_refuses_console() {
    use crate::platform::{Outputs, RepairExit, PLAYBACK_ROLES};
    let mut r = Rig::on(App::for_tests());
    r.app.user_config.lock().unwrap().only_some_apps = true;
    r.windows.set_our_output_present(false);
    r.windows.set_repair_exit(RepairExit::Success);
    r.send(Command::RepairOutput);
    r.settle();

    r.windows.set_our_output_present(true);
    r.windows.set_default(&output(OURS), PLAYBACK_ROLES).unwrap();
    r.windows.refuse_role(Some(Role::Console));
    let before = r.windows.switches().len();
    r.driver.look();
    assert_eq!(r.default(), output(OURS));
    assert_eq!(r.windows.default_for(Role::Multimedia), Some(output(SPEAKERS)));
    assert_eq!(r.windows.switches()[before..], [
        (output(SPEAKERS), vec![Role::Console]),
        (output(SPEAKERS), vec![Role::Multimedia]),
    ]);
}

#[test]
fn repair_needs_a_protected_install_and_a_window_and_follows_the_output() {
    use crate::model::scene::RepairNow;
    use crate::platform::RepairExit;
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.windows.set_our_output_present(false);
    r.driver.look();
    assert_eq!(r.driver.situation().speaker(&id()).unwrap().state, RowState::RepairFirst);
    for from in [Surface::Phone, Surface::Api, Surface::CommandLine] {
        assert_eq!(r.driver.command(from, &Command::RepairOutput), Err(Refusal::WindowOnly));
    }
    r.send(Command::TurnOnOutput);
    assert_eq!(r.windows.recovery_calls(), (0, 0));

    r.windows.set_protected_install(false);
    assert_eq!(r.driver.scene().repair, RepairNow::Unavailable);
    r.send(Command::RepairOutput);
    r.settle();
    assert_eq!(r.windows.recovery_calls(), (0, 0));
    r.windows.set_protected_install(true);
    r.windows.set_output_on(HEADPHONES, true);
    r.windows.choose_in_windows(HEADPHONES);
    r.driver.look();
    r.advance(2_000);
    let switches = r.windows.switches();

    r.windows.set_repair_exit(RepairExit::Cancelled);
    r.send(Command::RepairOutput);
    r.send(Command::RepairOutput);
    r.settle();
    assert_eq!(r.driver.scene().repair, RepairNow::NotApproved);
    assert_eq!(r.windows.recovery_calls(), (0, 1));

    r.windows.set_repair_exit(RepairExit::Failed);
    r.send(Command::RepairOutput);
    r.settle();
    assert_eq!(r.driver.scene().repair, RepairNow::DidntWork);

    r.windows.set_repair_exit(RepairExit::Success);
    r.send(Command::RepairOutput);
    assert_eq!(r.driver.scene().repair, RepairNow::NotAsked);
    let approved = r.rx.recv_timeout(WAIT).unwrap();
    assert!(matches!(approved, PlaceInput::Event(Event::RepairApproved(_))));
    r.driver.input(approved);
    assert_eq!(r.driver.scene().repair, RepairNow::Running);
    r.send(Command::RepairOutput);
    r.advance(119_000);
    assert_eq!(r.driver.scene().repair, RepairNow::Running);
    r.advance(1_001);
    assert_eq!(r.driver.scene().repair, RepairNow::DidntWork);
    assert_eq!(r.windows.switches(), switches);
    assert_eq!(r.windows.recovery_calls(), (0, 3));
    let late_exit = r.rx.recv_timeout(WAIT).unwrap();

    r.windows.set_repair_exit(RepairExit::Failed);
    r.send(Command::RepairOutput);
    r.driver.input(late_exit);
    r.windows.set_output_on(OURS, false);
    r.windows.set_our_output_present(true);
    r.settle();
    assert_eq!(r.driver.scene().repair, RepairNow::TurnedOff);
    r.send(Command::TurnOnOutput);
    r.driver.look();
    assert_eq!(r.driver.scene().repair, RepairNow::Worked);
    assert_ne!(r.driver.situation().speaker(&id()).unwrap().state, RowState::RepairFirst);
    r.advance(60_000);
    assert_eq!(r.driver.scene().repair, RepairNow::Worked);
    r.pick();
    assert_eq!(r.driver.scene().repair, RepairNow::NotAsked);
    r.settle();
    assert_eq!(r.windows.recovery_calls(), (1, 4));
}
