//! A speaker's answers and the binding each connect makes.

use super::*;
use crate::airplay::fake_raop::{Answer, FakeRaop};
use crate::model::situation::{Away, RowState};

#[test]
fn sound_starts_and_the_speaker_answers_then_keeps_the_connect_waiting() {
    let (fake, reached, go) = held_renderer("GetMediaInfo");
    let mut r = Rig::new(&fake, remembered());
    start_sound(&mut r);
    reached.recv_timeout(WAIT).unwrap();
    let s = after_two_seconds(&mut r);
    assert_eq!(s.note, None);
    assert_eq!(s.speakers.iter().find(|s| s.id == id()).unwrap().state, RowState::Connecting);
    go.send(()).unwrap();
    r.settle();
}

#[test]
fn sound_starts_and_the_speaker_has_yet_to_answer() {
    let (fake, reached, go) = held_renderer("GetTransportInfo");
    let mut r = Rig::new(&fake, remembered());
    start_sound(&mut r);
    reached.recv_timeout(WAIT).unwrap();
    let s = after_two_seconds(&mut r);
    assert_eq!(s.note.unwrap().cause, Cause::NotRespondingAtStart { speaker: id(), why: Away::NotResponding });
    assert_eq!(s.speakers.iter().find(|s| s.id == id()).unwrap().state, RowState::NotResponding);
    go.send(()).unwrap();
    r.settle();
}

fn start_sound(r: &mut Rig) {
    r.advance(3_010);
    r.windows.player("Music", Some(Status::Playing));
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.driver.look();
}

fn after_two_seconds(r: &mut Rig) -> Situation {
    for input in r.rx.try_iter() {
        r.driver.input(input);
    }
    r.advance(2_000);
    let s = r.driver.situation();
    assert_eq!(s.spot, Spot::Speaker(id()));
    assert_eq!(s.doing, Doing::Connecting);
    s
}

#[test]
fn each_connect_reports_one_answer_across_the_look_and_the_transport_requests() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    for _ in 0..2 {
        r.pick();
        let end = r.answered_connect();
        r.driver.input(end);
        r.settle();
        assert_eq!(r.driver.bound().unwrap().0, DEN);
        r.send(Command::StayOnThisPc);
        r.settle();
    }
    assert_eq!(fake.soap_actions().iter().filter(|a| *a == "Play").count(), 2);
}

#[test]
fn a_busy_speaker_gets_the_chime_only_after_confirmation() {
    let fake = FakeRenderer::start(|req| match req.soap_action() {
        Some("GetTransportInfo") => Reply::soap_ok(AV_TRANSPORT, "GetTransportInfo", &[("CurrentTransportState", "PLAYING")]),
        Some("GetMediaInfo") => Reply::soap_ok(AV_TRANSPORT, "GetMediaInfo", &[("CurrentURI", "http://other-pc/music")]),
        _ => answer(req),
    });
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.send(Command::PlayChime { speaker: id(), confirmed: false });
    r.settle();
    assert_eq!(fake.soap_actions(), ["GetTransportInfo", "GetMediaInfo"]);
    assert!(matches!(r.driver.situation().confirm_in_panel, Some(crate::model::situation::Confirm::ChimeBusy { .. })));

    r.send(Command::PlayChime { speaker: id(), confirmed: true });
    r.settle();
    assert_eq!(fake.soap_actions(), ["GetTransportInfo", "GetMediaInfo", "SetAVTransportURI", "Play"]);
    assert!(fake.requests().iter().any(|r| r.soap_action() == Some("SetAVTransportURI") && r.body_text().contains(crate::app::CHIME_PATH)));
}

#[test]
fn choosing_the_speaker_again_waits_for_its_cancelled_sessions_stop() {
    let (stopping, stopped) = bounded(1);
    let (go, resume) = bounded(1);
    let (playing, played) = crossbeam_channel::unbounded();
    let stops = AtomicU64::new(0);
    let fake = FakeRenderer::start(move |req| {
        if req.soap_action() == Some("Stop") && stops.fetch_add(1, Ordering::Relaxed) == 1 {
            stopping.send(()).unwrap();
            resume.recv_timeout(WAIT).unwrap();
        }
        if req.soap_action() == Some("Play") { playing.send(()).unwrap(); }
        answer(req)
    });
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    let ready = r.answered_connect();
    played.recv_timeout(WAIT).unwrap();
    r.send(Command::CancelSwitch);
    stopped.recv_timeout(WAIT).unwrap();
    r.pick();
    assert_eq!(played.recv_timeout(Duration::from_millis(3500)), Err(crossbeam_channel::RecvTimeoutError::Timeout));
    go.send(()).unwrap();
    played.recv_timeout(WAIT).unwrap();
    r.driver.input(ready);
    r.settle();
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    assert_eq!(r.driver.situation().place_speaker(), Some(&id()));
}

#[test]
fn picking_again_while_the_cancelled_connect_is_subscribing_plays_after_its_stop() {
    let (subscribing, subscribed) = bounded(1);
    let (resume, go) = bounded(1);
    let (playing, plays) = unbounded();
    let first = AtomicBool::new(true);
    let fake = FakeRenderer::start(move |req| {
        if req.method == "SUBSCRIBE" && first.swap(false, Ordering::Relaxed) {
            subscribing.send(()).unwrap();
            go.recv_timeout(WAIT).unwrap();
        }
        if req.soap_action() == Some("Play") { playing.send(()).unwrap(); }
        answer(req)
    });
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    plays.recv_timeout(WAIT).unwrap();
    subscribed.recv_timeout(WAIT).unwrap();
    assert!(r.driver.bound().is_none());
    r.send(Command::CancelSwitch);
    r.pick();
    assert_eq!(plays.recv_timeout(Duration::from_millis(400)), Err(crossbeam_channel::RecvTimeoutError::Timeout));
    resume.send(()).unwrap();
    r.settle();
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    let transport: Vec<_> = fake.soap_actions().into_iter().filter(|a| a == "Play" || a == "Stop").collect();
    assert_eq!(transport, ["Stop", "Play", "Stop", "Stop", "Play"]);
}

#[test]
fn replacing_a_session_on_the_same_renderer_only_unsubscribes_the_old_one() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    r.settle();
    let topology = format!(r#"<ZoneGroups><ZoneGroup Coordinator="fake-den">
        <ZoneGroupMember UUID="fake-den" ZoneName="Den" Location="{}"/>
        <ZoneGroupMember UUID="fake-office" ZoneName="Office"/></ZoneGroup></ZoneGroups>"#, fake.url("/description.xml"));
    let room = FakeRenderer::start(move |q| match q.soap_action() {
        Some("GetZoneGroupState") => Reply::soap_ok("urn:schemas-upnp-org:service:ZoneGroupTopology:1",
            "GetZoneGroupState", &[("ZoneGroupState", &topology)]),
        None if q.method == "GET" => Reply::ok(&renderer_description("Office", &["ZoneGroupTopology"])),
        _ => answer(q),
    });
    let office = r.app.add_manual_speaker(crate::manual_speakers::ManualKind::Upnp, "Office", &room.url("/description.xml"), "").unwrap();
    let before = fake.requests().len();
    r.app.engine_connect(&office, true).unwrap();
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    assert!(!room.soap_actions().iter().any(|a| a == "Play"));
    let requests = fake.requests();
    let play = requests[before..].iter().position(|q| q.soap_action() == Some("Play")).unwrap();
    let after = &requests[before + play + 1..];
    assert!(after.iter().any(|q| q.method == "UNSUBSCRIBE"));
    assert!(!after.iter().any(|q| q.soap_action() == Some("Stop")));
}

#[test]
fn a_late_stop_cannot_move_windows_after_another_speaker_binds() {
    let (stopping, stopped) = bounded(1);
    let (resume, go) = bounded(1);
    let stops = AtomicU64::new(0);
    let den = FakeRenderer::start(move |req| {
        if req.soap_action() == Some("Stop") && stops.fetch_add(1, Ordering::Relaxed) == 1 {
            stopping.send(()).unwrap();
            go.recv_timeout(WAIT).unwrap();
        }
        answer(req)
    });
    let office = FakeRenderer::start(|req| {
        if req.method == "GET" { Reply::ok(&renderer_description("Office", &[])) } else { answer(req) }
    });
    let mut r = Rig::new(&den, UserConfig::in_memory());
    let found = r.app.discovery.as_ref().unwrap();
    let mut rows = found.renderers();
    rows.push(crate::ssdp::fetch_and_parse_device(&office.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap());
    found.replace(rows);
    r.playing();
    r.send(Command::StayOnThisPc);
    stopped.recv_timeout(WAIT).unwrap();
    let target = SpeakerId::new("uuid:fake-office");
    r.send(Command::Switch { place: Place::Speaker(target.clone()), way: None });
    let ready = next_report(&mut r);
    r.driver.input(ready);
    assert_eq!(r.driver.situation().place_speaker(), Some(&target));
    resume.send(()).unwrap();
    r.settle();
    assert_eq!(r.default(), output(OURS));
}

#[test]
fn a_superseded_job_leaves_the_later_session_to_the_same_speaker_playing() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    let completion = r.answered_connect();
    let first = r.driver.bound().unwrap();
    // The delay engine or another job can bind the same speaker before
    // the engine processes the first completion.
    let app = r.app.clone();
    let (done, rebound) = bounded(1);
    let worker = std::thread::spawn(move || done.send(app.engine_connect(DEN, true)).unwrap());
    rebound.recv_timeout(WAIT).unwrap().unwrap();
    worker.join().unwrap();
    let second = r.driver.bound().unwrap();
    assert_eq!(first.0, second.0);
    assert_ne!(first.1, second.1);
    let stops = fake.soap_actions().iter().filter(|action| *action == "Stop").count();
    r.send(Command::CancelSwitch);
    r.driver.input(completion);
    r.settle();
    assert_eq!(r.driver.bound(), Some(second));
    assert_eq!(fake.soap_actions().iter().filter(|action| *action == "Stop").count(), stops);
}

pub(super) fn airplay_rig(receiver: &FakeRaop, cfg: UserConfig, upnp: Option<Arc<DiscoveryState>>) -> Rig {
    use crate::airplay::{discovery::RAOP_SERVICE, AirPlayDiscoveryState};
    use crate::discovery_net::LanTargets;

    let lan = LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)]);
    let discovery = AirPlayDiscoveryState::with_lan(lan.with_fakes_on_loopback());
    let info = mdns_sd::ServiceInfo::new(
        RAOP_SERVICE,
        "0A1B2C3D4E5F@Office",
        "office.local.",
        "127.0.0.1",
        receiver.port(),
        &[("et", "0,1"), ("cn", "0,1")][..],
    )
    .unwrap();
    discovery.ingest_for_tests(RAOP_SERVICE, &info);
    Rig::on(App::with_settings(
        AppConfig { advertise_ip: None, ..App::test_config() },
        cfg,
        upnp,
        Some(Arc::new(discovery)),
    ))
}

#[test]
fn an_airplay_password_request_depends_on_what_was_typed_for_this_connect() {
    use crate::model::{command::FirstRunAction, situation::PickWhy};
    let receiver = FakeRaop::start(|_| Answer::Reply(crate::airplay::fake_raop::Reply::status(401, "Unauthorized")
        .header("WWW-Authenticate", r#"Digest realm="raop", nonce="1""#)));
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    let needed = PickWhy::NeedsSecret(Secret::Password);
    let wrong = PickWhy::WrongSecret(Secret::Password);
    r.send(Command::FirstRunAction(FirstRunAction::Chime(speaker.clone())));
    r.settle();
    assert_eq!(r.driver.situation().pick_failed.unwrap().why, needed);
    for (entry, expected) in [(None, needed), (Some(true), wrong), (Some(false), wrong), (None, needed)] {
        if let Some(once) = entry {
            let mut dialog = r.app.password_dialog(speaker.as_str(), "Office");
            dialog.one_session = once;
            r.app.enter_airplay_password(&dialog, "entered").unwrap();
        }
        r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: None });
        r.settle();
        let scene = r.driver.scene();
        assert_eq!(scene.situation.pick_failed.as_ref().unwrap().why, expected);
        assert_eq!(scene.situation.note.as_ref().unwrap().cause, Cause::DidntGetReady { speaker: speaker.clone(), why: expected });
        let title = crate::model::notes::didnt_get_ready(&scene, &speaker, expected).0;
        assert_eq!(title, if expected == needed { "Office needs its password" } else { "That password didn’t work" });
    }
}

#[test]
fn a_password_submitted_for_folded_sonos_uses_its_airplay_choice() {
    use crate::delay::DelayChoice;
    let receiver = FakeRaop::start(|req| {
        if req.header("Authorization").is_some() { Answer::Normal }
        else { Answer::Reply(crate::airplay::fake_raop::Reply::status(401, "Unauthorized")
            .header("WWW-Authenticate", r#"Digest realm="raop", nonce="1""#)) }
    });
    let fake = FakeRenderer::start(|_| Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])));
    let found = DiscoveryState::new();
    found.replace(vec![crate::ssdp::fetch_and_parse_device(&fake.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap()]);
    let ap = "airplay:0A1B2C3D4E5F";
    for (choice, remembered) in [(DelayChoice::Short, None), (DelayChoice::Steady, None), (DelayChoice::Steady, Some(WayChoice::AirPlayShort))] {
        let mut r = airplay_rig(&receiver, UserConfig::in_memory(), Some(found.clone()));
        r.app.set_delay_choice(ap, choice).unwrap();
        if let Some(way) = remembered { r.send(Command::ChooseWay { speaker: id(), way }); }
        let scene = r.driver.scene();
        assert_eq!(scene.speakers.len(), 1);
        let row = &scene.speakers[0];
        assert_eq!(row.way, remembered.unwrap_or(WayChoice::Native));
        let (sent, commands) = bounded(1);
        r.app.set_command_sink(Box::new(move |from, cmd| {
            sent.send((from, cmd)).unwrap();
            crate::model::command::ticket().1
        }));
        r.app.submit_password_dialog(&r.app.password_dialog(ap, &row.name), "entered").unwrap();
        let (from, cmd) = commands.try_recv().unwrap();
        r.driver.command(from, &cmd).unwrap();
        assert_eq!(r.driver.situation().picking.unwrap().way, remembered.unwrap_or(row.airplay.unwrap()));
        r.settle();
        assert_eq!(r.driver.situation().place_speaker(), Some(&id()));
        assert_eq!(r.driver.bound().unwrap().0, ap);
        r.send(Command::StayOnThisPc);
        r.settle();
    }
}

#[test]
fn a_queued_pairing_switch_gives_way_to_a_newer_pick() {
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    let paired = |picks_seen| Command::PlayAfterPairing { speaker: speaker.clone(), picks_seen };
    let before = r.app.user_pick_count();
    r.send(Command::StayOnThisPc);
    r.send(paired(before));
    assert!(r.driver.situation().picking.is_none());
    r.send(paired(r.app.user_pick_count()));
    assert_eq!(r.driver.situation().picking.unwrap().speaker, speaker);
    r.settle();
    assert_eq!(r.driver.situation().place_speaker(), Some(&speaker));
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn an_airplay_reply_counts_for_the_sonos_place_before_the_session_is_ready() {
    let (reached, waiting) = bounded(1);
    let (go, resume) = bounded(1);
    let requests = AtomicU64::new(0);
    let receiver = FakeRaop::start(move |_| {
        if requests.fetch_add(1, Ordering::Relaxed) == 1 {
            reached.send(()).unwrap();
            let _ = resume.recv_timeout(WAIT);
        }
        Answer::Normal
    });
    let fake = FakeRenderer::start(|_| Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])));
    let found = DiscoveryState::new();
    found.replace(vec![
        crate::ssdp::fetch_and_parse_device(&fake.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap(),
    ]);
    let mut cfg = remembered();
    cfg.sound_place.as_mut().unwrap().ways.insert(id(), WayChoice::AirPlaySteady);
    let mut r = airplay_rig(&receiver, cfg, Some(found));
    start_sound(&mut r);
    waiting.recv_timeout(WAIT).unwrap();
    let s = after_two_seconds(&mut r);
    assert_eq!(s.note, None);
    assert_eq!(s.speakers.len(), 1);
    assert_eq!(r.driver.scene().speakers[0].airplay_id, Some(SpeakerId::new("airplay:0A1B2C3D4E5F")));
    assert_eq!((&s.speakers[0].id, s.speakers[0].state), (&id(), RowState::Connecting));
    go.send(()).unwrap();
    r.settle();
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn choosing_less_delay_away_from_the_place_sets_the_airplay_rows_next_switch() {
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    r.app.set_delay_choice(speaker.as_str(), crate::delay::DelayChoice::Steady).unwrap();
    r.send(Command::ChooseWay { speaker: speaker.clone(), way: WayChoice::AirPlayShort });
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: None });
    assert_eq!(r.driver.situation().picking.unwrap().way, WayChoice::AirPlayShort);
    r.settle();
    assert_eq!(r.driver.bound().unwrap().0, speaker.as_str());
    assert_eq!(r.app.delay_status_of(speaker.as_str()).choice, Some(crate::delay::DelayChoice::Short));
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn changing_airplay_way_reconnects_once_even_with_the_same_adjustment() {
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: Some(WayChoice::AirPlaySteady) });
    let end = r.answered_connect();
    r.driver.input(end);
    r.settle();
    let first = r.driver.bound().unwrap();
    r.send(Command::ChooseWay { speaker: speaker.clone(), way: WayChoice::AirPlayShort });
    let end = r.answered_connect();
    r.driver.input(end);
    r.settle();
    assert_ne!(r.driver.bound().unwrap().1, first.1);
    assert_eq!(receiver.requests().iter().filter(|q| q.method == "RECORD").count(), 2);
    assert_eq!(r.driver.core.kept().ways[&speaker], WayChoice::AirPlayShort);
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn the_welcome_volume_reaches_the_test_speaker_before_replay() {
    use crate::model::{command::FirstRunAction as A, situation::FirstRunStep};
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    let output = r.default();
    r.send(Command::FirstRunAction(A::Chime(id())));
    r.settle();
    assert!(matches!(r.driver.situation().first_run, Some(FirstRunStep::Two { .. })));
    let before = fake.requests().len();
    r.send(Command::FirstRunAction(A::ChimeVolume(43)));
    r.send(Command::FirstRunAction(A::ChimeAgain));
    r.settle();
    let requests = fake.requests();
    let volume = requests[before..].iter().position(|r| r.soap_action() == Some("SetVolume") && r.body_text().contains(">43<")).unwrap();
    let play = requests[before..].iter().position(|r| r.soap_action() == Some("Play")).unwrap();
    assert!(volume < play);
    assert_eq!(r.default(), output);
    assert!(r.driver.bound().is_none());
}

#[test]
fn a_failed_way_moves_to_pc_and_a_cancelled_way_restores_the_place() {
    for cancel in [false, true] {
        let (trying, tried) = bounded(1);
        let (go, resume) = bounded(1);
        let first = AtomicBool::new(true);
        let receiver = FakeRaop::start(move |_| {
            if first.swap(false, Ordering::SeqCst) {
                trying.send(()).unwrap();
                let _ = resume.recv_timeout(WAIT);
            }
            Answer::Close
        });
        let fake = FakeRenderer::start(|req| {
            if req.method == "GET" { Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])) }
            else { answer(req) }
        });
        let found = DiscoveryState::new();
        found.replace(vec![crate::ssdp::fetch_and_parse_device(&fake.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap()]);
        let mut r = airplay_rig(&receiver, UserConfig::in_memory(), Some(found));
        r.playing();
        r.send(Command::ChooseWay { speaker: id(), way: WayChoice::AirPlaySteady });
        tried.recv_timeout(WAIT).unwrap();
        assert!(r.driver.bound().is_none());
        if cancel {
            r.send(Command::CancelSwitch);
        }
        go.send(()).unwrap();
        r.settle();
        let situation = r.driver.situation();
        if cancel {
            assert_eq!(situation.place_speaker(), Some(&id()));
            assert!(situation.note.is_none());
            assert_eq!(r.driver.bound().unwrap().0, DEN);
            assert_eq!(r.default(), output(OURS));
        } else {
            assert!(matches!(situation.spot, Spot::ThisPc(_)));
            assert!(matches!(situation.note.unwrap().cause, Cause::StoppedResponding { speaker, .. } if speaker == id()));
            assert_eq!(r.default(), output(SPEAKERS));
            assert_eq!(r.driver.core.kept().place, None);
        }
    }
}

#[test]
fn a_bound_airplay_speakers_saved_level_ages_from_the_last_sound() {
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: Some(WayChoice::AirPlaySteady) });
    r.settle();
    r.app.vsync.window_changed(64);
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.driver.look();
    r.advance(place::numbers::LEVEL_SAVED_AFTER_MS + 1);
    let saved = || r.app.speaker_settings(speaker.as_str());
    let last = saved().level_at.unwrap();
    assert_eq!(saved().level, Some(64));
    r.windows.app_plays("Music", OURS, None);
    r.advance(60_001);
    assert!(r.driver.session_of(&speaker).is_some());
    let saved = r.app.speaker_settings(speaker.as_str());
    assert_eq!(saved.level_at, Some(last));
    assert_eq!(crate::app::start_level(saved.level, saved.level_at, last + 3 * 3600 + 1, None), 30);
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn a_refused_stop_does_not_report_the_stream_stopped() {
    let fake = FakeRenderer::start(|req| match req.soap_action() {
        Some("Stop") => Reply::soap_fault(701, "Transition unavailable"),
        _ => answer(req),
    });
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    r.settle();
    let (_, number) = r.driver.bound().unwrap();
    assert_eq!(r.app.take_stream_off_number(Some(number)), None);
}

#[test]
fn cancelling_after_the_place_was_released_restores_it_unless_a_new_switch_wins() {
    for newer in [false, true] {
        let (stopping, stopped) = bounded(1);
        let (go, resume) = bounded(1);
        let stops = AtomicU64::new(0);
        let den = FakeRenderer::start(move |req| {
            if req.soap_action() == Some("Stop") && stops.fetch_add(1, Ordering::SeqCst) == 1 {
                stopping.send(()).unwrap();
                resume.recv_timeout(WAIT).unwrap();
            }
            answer(req)
        });
        let office = FakeRenderer::start(|req| {
            if req.method == "GET" { Reply::ok(&renderer_description("Office", &[])) } else { answer(req) }
        });
        let mut r = Rig::new(&den, UserConfig::in_memory());
        let found = r.app.discovery.as_ref().unwrap();
        let mut speakers = found.renderers();
        speakers.push(crate::ssdp::fetch_and_parse_device(&office.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap());
        found.replace(speakers);
        r.app.set_sendspin_source_enabled(true).unwrap();
        r.playing();
        let first = r.driver.bound().unwrap();
        r.send(Command::Switch { place: Place::MusicAssistant, way: None });
        stopped.recv_timeout(WAIT).unwrap();
        assert!(r.driver.bound().is_none());
        let expected = if newer {
            let office = SpeakerId::new("uuid:fake-office");
            r.send(Command::Switch { place: Place::Speaker(office.clone()), way: None });
            office
        } else {
            r.send(Command::CancelSwitch);
            id()
        };
        go.send(()).unwrap();
        r.settle();
        // The app samples the sound all the while; here the last sample
        // was taken before the waits above, so one is taken now.
        r.driver.look();
        let s = r.driver.situation();
        assert_eq!(s.place_speaker(), Some(&expected));
        assert_eq!(s.doing, Doing::Playing);
        assert!(s.note.is_none());
        assert_ne!(r.driver.session_of(&expected).unwrap().1, first.1);
        assert_eq!(den.soap_actions().iter().filter(|a| *a == "Play").count(), if newer { 1 } else { 2 });
        r.send(Command::StayOnThisPc);
        r.settle();
        r.app.set_sendspin_source_enabled(false).unwrap();
    }
}

#[test]
fn a_failed_delay_change_on_one_airplay_receiver_moves_playing_sound_to_pc() {
    failed_delay_change(false);
}

#[test]
fn use_steadier_saves_the_choice_until_playing_and_recovers_a_failed_change() {
    failed_delay_change(true);
}

fn failed_delay_change(from_help: bool) {
    let fail = Arc::new(AtomicBool::new(false));
    let reject = fail.clone();
    let receiver = FakeRaop::start(move |_| {
        if !reject.load(Ordering::Relaxed) { Answer::Normal }
        else if from_help { Answer::Close }
        else { Answer::Reply(crate::airplay::fake_raop::Reply::status(403, "Forbidden")) }
    });
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    let starting_way = if from_help {
        r.send(Command::UseDelayChoice { speaker: speaker.clone(), choice: crate::model::facts::DelayChoice::Short });
        assert_eq!(r.app.delay_status_of(speaker.as_str()).choice, Some(crate::delay::DelayChoice::Short));
        assert!(r.driver.jobs.is_empty() && receiver.requests().is_empty());
        assert!(matches!(r.driver.situation().spot, Spot::ThisPc(_)));
        WayChoice::AirPlayShort
    } else { WayChoice::AirPlaySteady };
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: Some(starting_way) });
    r.settle();
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.windows.player("Music", Some(Status::Playing));
    r.driver.look();
    assert_eq!(r.driver.situation().doing, Doing::Playing);
    fail.store(true, Ordering::Relaxed);
    if from_help {
        r.send(Command::UseDelayChoice { speaker: speaker.clone(), choice: crate::model::facts::DelayChoice::Steady });
    } else {
        r.send(Command::ChooseWay { speaker: speaker.clone(), way: WayChoice::AirPlayShort });
    }
    assert_eq!(r.driver.situation().picking.unwrap().by, crate::model::situation::SwitchBy::Way);
    // The running driver samples the music throughout the reconnect.
    loop {
        let input = r.rx.recv_timeout(WAIT).unwrap();
        let finished = if let PlaceInput::Connected { ended, .. } = &input {
            assert_eq!(*ended, if from_help { Ended::NotResponding } else { Ended::Refused });
            true
        } else { false };
        r.driver.look();
        r.driver.input(input);
        if finished { break; }
    }
    r.settle();
    let s = r.driver.situation();
    assert_eq!(s.spot, Spot::ThisPc(Some(output(SPEAKERS))));
    let cause = s.note.unwrap().cause;
    if from_help { assert!(matches!(cause, Cause::StoppedResponding { speaker: k, .. } if k == speaker)); }
    else { assert_eq!(cause, Cause::WayFailed { speaker, why: crate::model::situation::PickWhy::Refused }); }
    assert_eq!(r.default(), output(SPEAKERS));
    assert!(r.driver.bound().is_none());
}

#[test]
fn closing_first_run_after_the_chime_uri_cancels_play() {
    use crate::model::command::FirstRunAction as A;
    let (fake, reached, go) = held_renderer("SetAVTransportURI");
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.send(Command::FirstRunAction(A::Chime(id())));
    reached.recv_timeout(WAIT).unwrap();
    r.send(Command::FirstRunAction(A::Close));
    go.send(()).unwrap();
    r.settle();
    assert!(!fake.soap_actions().iter().any(|a| a == "Play"));
    assert!(r.driver.situation().first_run.is_none());
}

#[test]
fn play_when_listed_yields_to_windows_output_and_headphones() {
    use crate::platform::fake::MONITOR;
    for headphones in [false, true] {
        let fake = FakeRenderer::start(answer);
        let mut r = Rig::new(&fake, UserConfig::in_memory());
        let output_id = if headphones { HEADPHONES } else { MONITOR };
        if !headphones {
            r.windows.set_output_on(MONITOR, true);
            r.advance(3_000);
            r.advance(3_000);
        }
        let picks_seen = r.app.user_pick_count();
        if headphones { r.windows.set_output_on(HEADPHONES, true); }
        r.windows.choose_in_windows(output_id);
        r.driver.input(PlaceInput::System(SystemEvent::DefaultOutputChanged));
        r.send(Command::PlayWhenListed { speaker: id(), picks_seen });
        assert!(r.driver.situation().picking.is_none());
        r.advance(3_000);
        r.settle();
        assert_eq!(r.driver.situation().spot, Spot::ThisPc(Some(output(output_id))));
        assert!(r.driver.bound().is_none());
        assert!(!fake.soap_actions().contains(&"Play".to_string()));
    }
}

#[test]
fn the_welcome_level_reaches_upnp_before_the_stream_plays() {
    use crate::model::command::FirstRunAction as A;
    for parked in [false, true] {
        let fake = FakeRenderer::start(answer);
        let mut r = Rig::new(&fake, UserConfig::in_memory());
        if parked {
            r.pick();
            r.settle();
            r.advance(place::numbers::STREAM_REST_AFTER_WALL_MS as u64 + 1);
            r.settle();
            assert!(r.driver.parked.is_some());
        }
        r.send(Command::FirstRunAction(A::Chime(id())));
        r.settle();
        r.send(Command::FirstRunAction(A::ChimeVolume(43)));
        let before = fake.requests().len();
        r.app.vsync.note_muted(true);
        r.send(Command::FirstRunAction(A::Heard));
        r.settle();
        let requests = fake.requests();
        let stream = &requests[before..];
        let volume = stream.iter().position(|r| r.soap_action() == Some("SetVolume") && r.body_text().contains(">43<")).unwrap();
        let play = stream.iter().position(|r| r.soap_action() == Some("Play")).unwrap();
        assert!(volume < play);
        assert!(r.app.vsync.is_muted());
        assert_eq!(r.driver.situation().place_speaker(), Some(&id()));
    }
}

#[test]
fn cancelling_a_switch_restores_the_place_that_bound_after_it_started() {
    let (den, waiting, play) = held_renderer("Play");
    let (reached, office_waiting) = bounded(1);
    let (go, office_play) = bounded(1);
    let office = FakeRenderer::start(move |req| {
        if req.method == "GET" { return Reply::ok(&renderer_description("Office", &[])); }
        if req.soap_action() == Some("Play") {
            reached.send(()).unwrap();
            office_play.recv_timeout(WAIT).unwrap();
        }
        answer(req)
    });
    let mut r = Rig::new(&den, remembered());
    let found = r.app.discovery.as_ref().unwrap();
    let mut speakers = found.renderers();
    speakers.push(crate::ssdp::fetch_and_parse_device(&office.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap());
    found.replace(speakers);
    start_sound(&mut r);
    waiting.recv_timeout(WAIT).unwrap();
    r.send(Command::Switch { place: Place::Speaker(SpeakerId::new("uuid:fake-office")), way: None });
    office_waiting.recv_timeout(WAIT).unwrap();
    assert!(r.driver.bound().is_none());
    play.send(()).unwrap();
    loop {
        let input = r.rx.recv_timeout(WAIT).unwrap();
        let ready = matches!(input, PlaceInput::Connected { ended: Ended::Ready, .. });
        r.driver.input(input);
        if ready { break; }
    }
    let first = r.driver.bound().unwrap();
    assert_eq!(first.0, DEN);
    go.send(()).unwrap();
    let ready = r.rx.recv_timeout(WAIT).unwrap();
    assert!(matches!(ready, PlaceInput::Connected { ended: Ended::Ready, .. }));
    r.send(Command::CancelSwitch);
    r.driver.input(ready);
    r.settle();
    // The app samples the sound all the while; here the last sample
    // was taken before the waits above, so one is taken now.
    r.driver.look();
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    assert_ne!(r.driver.bound().unwrap().1, first.1);
    let s = r.driver.situation();
    assert_eq!((s.spot, s.doing, s.note), (Spot::Speaker(id()), Doing::Playing, None));
}

#[test]
fn a_cancelled_delay_change_leaves_recovery_to_the_switch_that_took_the_session() {
    let (stopping, stopped) = bounded(1);
    let (go, resume) = bounded(1);
    let first = AtomicBool::new(true);
    let receiver = FakeRaop::start(move |req| {
        if req.method == "TEARDOWN" && first.swap(false, Ordering::SeqCst) {
            stopping.send(()).unwrap();
            let _ = resume.recv_timeout(WAIT);
        }
        Answer::Normal
    });
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    r.app.set_sendspin_source_enabled(true).unwrap();
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: Some(WayChoice::AirPlaySteady) });
    r.settle();
    r.windows.player("Music", Some(Status::Playing));
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.driver.look();
    r.send(Command::ChooseWay { speaker: speaker.clone(), way: WayChoice::AirPlayShort });
    let until = Instant::now() + WAIT;
    while r.app.delay_status_of(speaker.as_str()).progress != crate::delay::Progress::WillReconnect {
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    r.send(Command::Switch { place: Place::MusicAssistant, way: None });
    stopped.recv_timeout(WAIT).unwrap();
    r.app.set_sendspin_source_enabled(false).unwrap();
    go.send(()).unwrap();
    r.settle();
    // The app samples the sound all the while; here the last sample
    // was taken before the waits above, so one is taken now.
    r.driver.look();
    assert_eq!(r.driver.bound().unwrap().0, speaker.as_str());
    let s = r.driver.situation();
    assert_eq!((s.spot, s.doing), (Spot::Speaker(speaker), Doing::Playing));
    assert!(matches!(s.note.unwrap().cause, Cause::DidntGetReady { speaker, .. } if speaker.is_music_assistant()));
    assert_eq!(s.pick_failed.unwrap().speaker, SpeakerId::music_assistant());
}

#[test]
fn each_chime_obeys_the_limit_on_its_transport_identity() {
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let fake = FakeRenderer::start(|req| {
        if req.method == "GET" { Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])) }
        else { answer(req) }
    });
    let found = DiscoveryState::new();
    found.replace(vec![crate::ssdp::fetch_and_parse_device(&fake.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap()]);
    let r = airplay_rig(&receiver, UserConfig::in_memory(), Some(found));
    let ap = "airplay:0A1B2C3D4E5F";
    for (id, limit) in [(DEN, 18), (ap, 12)] {
        let mut settings = r.app.speaker_settings(id);
        settings.volume_limit = Some(limit);
        r.app.set_speaker_settings(id, settings);
        let alias = if id == DEN { "127.0.0.1" } else { "airplay:0A1B2C3D4E5F/0a1b2c3d4e5f@office._raop._tcp.local." };
        r.app.chime_beside(alias, true, Some(40), true).unwrap();
    }
    assert!(fake.requests().iter().any(|q| q.soap_action() == Some("SetVolume") && q.body_text().contains(">18<")));
    let levels: Vec<_> = receiver.requests().into_iter().filter_map(|q| {
        String::from_utf8(q.body).ok()?.strip_prefix("volume: ")?.trim().parse::<f32>().ok()
    }).collect();
    assert_eq!(levels.last().copied(), Some(crate::airplay::session::volume_pct_to_raop_db(12)));
    assert!(r.driver.bound().is_none());
}

fn next_report(r: &mut Rig) -> PlaceInput {
    loop {
        let input = r.rx.recv_timeout(WAIT).unwrap();
        if matches!(input, PlaceInput::Connected { .. }) { return input; }
        r.driver.input(input);
    }
}

#[test]
fn cancelling_the_second_way_after_it_binds_restores_the_place() {
    let (stopping, stopped) = bounded(1);
    let (go, resume) = bounded(1);
    let stops = AtomicU64::new(0);
    let den = FakeRenderer::start(move |req| {
        if req.method == "GET" { return Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])); }
        if req.soap_action() == Some("Stop") && stops.fetch_add(1, Ordering::SeqCst) == 1 {
            stopping.send(()).unwrap();
            resume.recv_timeout(WAIT).unwrap();
        }
        answer(req)
    });
    let found = DiscoveryState::new();
    found.replace(vec![crate::ssdp::fetch_and_parse_device(&den.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap()]);
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), Some(found));
    r.playing();
    let first = r.driver.bound().unwrap();
    r.send(Command::ChooseWay { speaker: id(), way: WayChoice::AirPlayShort });
    stopped.recv_timeout(WAIT).unwrap();
    r.send(Command::ChooseWay { speaker: id(), way: WayChoice::AirPlaySteady });
    assert!(!receiver.wait_for_requests(1, Duration::from_millis(250)));
    assert!(r.driver.bound().is_none());
    go.send(()).unwrap();
    let a = next_report(&mut r);
    let b = next_report(&mut r);
    let (short, steady) = if matches!(a, PlaceInput::Connected { ended: Ended::Ready, .. }) { (b, a) } else { (a, b) };
    assert!(matches!(steady, PlaceInput::Connected { ended: Ended::Ready, .. }));
    assert_eq!(r.driver.bound().unwrap().0, "airplay:0A1B2C3D4E5F");
    assert!(matches!(&short, PlaceInput::Connected { released, .. } if released.contains(&first)));
    r.driver.input(short);
    r.send(Command::CancelSwitch);
    r.driver.input(steady);
    r.settle();
    // The music still plays: a fresh look sees it, however long the reconnect took.
    r.driver.look();
    let s = r.driver.situation();
    assert_eq!((s.spot, s.doing, s.note), (Spot::Speaker(id()), Doing::Playing, None));
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    assert_ne!(r.driver.bound().unwrap().1, first.1);
}

#[test]
fn a_switch_whose_ready_arrives_after_its_session_was_taken_plays() {
    let (den, waiting, play) = held_renderer("Play");
    let office = FakeRenderer::start(|req| {
        if req.method == "GET" { Reply::ok(&renderer_description("Office", &[])) } else { answer(req) }
    });
    let mut r = Rig::new(&den, remembered());
    let found = r.app.discovery.as_ref().unwrap();
    let mut rows = found.renderers();
    rows.push(crate::ssdp::fetch_and_parse_device(&office.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap());
    found.replace(rows);
    start_sound(&mut r);
    waiting.recv_timeout(WAIT).unwrap();
    let target = SpeakerId::new("uuid:fake-office");
    r.send(Command::Switch { place: Place::Speaker(target.clone()), way: None });
    let office_ready = next_report(&mut r);
    let first = r.driver.bound().unwrap();
    assert!(matches!(office_ready, PlaceInput::Connected { ended: Ended::Ready, .. }));
    assert_eq!(first.0, target.as_str());
    play.send(()).unwrap();
    let den_ready = next_report(&mut r);
    assert!(matches!(&den_ready, PlaceInput::Connected { ended: Ended::Ready, released, .. } if released.contains(&first)));
    r.driver.input(den_ready);
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    r.driver.input(office_ready);
    r.settle();
    // The app samples the sound all the while; here the last sample
    // was taken before the waits above, so one is taken now.
    r.driver.look();
    let s = r.driver.situation();
    assert_eq!((s.spot, s.doing, s.note), (Spot::Speaker(target.clone()), Doing::Playing, None));
    assert_eq!(r.driver.bound().unwrap().0, target.as_str());
    assert_ne!(r.driver.bound().unwrap().1, first.1);
}

#[test]
fn the_released_coordinators_session_still_names_its_room() {
    let den = FakeRenderer::start(answer);
    let mut r = Rig::new(&den, UserConfig::in_memory());
    r.pick();
    let ready = r.answered_connect();
    let coordinator = "uuid:coordinator";
    if let Some(crate::app::ActiveSession::Upnp(s)) = r.app.session.lock().unwrap().as_mut() {
        s.id_override = Some(coordinator.into());
    }
    for n in 100..132 {
        r.driver.recent_bindings.push_front((n, SpeakerId::new(&format!("room-{n}"))));
    }
    r.driver.input(ready);
    assert_eq!(r.driver.recent_bindings.len(), 32);
    assert_eq!(r.driver.recent_bindings.back(), Some(&(101, SpeakerId::new("room-101"))));
    let bound = r.driver.session_of(&id()).unwrap();
    assert_eq!(bound.0, coordinator);
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.windows.player("Music", Some(Status::Playing));
    r.driver.look();
    r.app.session.lock().unwrap().take();
    r.driver.situation();
    assert_eq!(r.driver.speaker_of(coordinator), SpeakerId::new(coordinator));
    r.driver.finished(u64::MAX, Ended::Refused, vec![bound]);
    r.settle();
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    assert_eq!(r.driver.situation().note, None);
}

#[test]
fn cancelling_a_mixed_chime_only_stops_its_own_session() {
    use crate::model::command::FirstRunAction as A;
    for replaced in [false, true] {
        let den = FakeRenderer::start(answer);
        let mut r = Rig::new(&den, UserConfig::in_memory());
        r.playing();
        r.send(Command::FirstRunAction(A::Chime(id())));
        let ready = next_report(&mut r);
        assert!(r.app.test_sound_playing());
        if replaced {
            r.app.engine_connect(DEN, true).unwrap();
            r.app.play_test_sound();
        }
        r.send(Command::FirstRunAction(A::Close));
        r.driver.input(ready);
        r.settle();
        assert_eq!(r.app.test_sound_playing(), replaced);
    }
}

#[test]
fn cancelling_an_airplay_chime_flushes_before_teardown() {
    use crate::model::command::FirstRunAction as A;
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    r.send(Command::FirstRunAction(A::Chime(speaker)));
    assert!(receiver.wait_until(WAIT, |events| events.iter().any(|e|
        matches!(e, crate::airplay::fake_raop::Event::Request(q) if q.method == "RECORD"))));
    r.send(Command::FirstRunAction(A::Close));
    r.settle();
    let methods: Vec<_> = receiver.requests().into_iter().map(|q| q.method).collect();
    assert!(methods.ends_with(&["FLUSH".into(), "TEARDOWN".into()]), "{methods:?}");
}

#[test]
fn lowering_the_playing_speakers_maximum_turns_it_down() {
    use crate::settings::{Act, SettingsPort, real::AppSettings};
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.playing();
    r.app.set_speaker_volume(64);
    let settings = AppSettings::new(r.app.clone());
    settings.act(Act::VolumeLimit(DEN.into(), 29));
    assert_eq!(r.app.current_volume(), Some(29));
    assert!(crate::sendspin::server::test_player::eventually(|| fake.requests().iter().any(|q|
        q.soap_action() == Some("SetVolume") && q.soap_arg("DesiredVolume").as_deref() == Some("29"))));
    r.send(Command::PlayChime { speaker: id(), confirmed: true });
    r.settle();
    assert_eq!(r.app.current_volume(), Some(29));
}

#[test]
fn a_chime_without_a_level_only_turns_down_a_speaker_above_its_limit() {
    let volume = Arc::new(AtomicU64::new(60));
    let current = volume.clone();
    let den = FakeRenderer::start(move |req| match req.soap_action() {
        Some("GetVolume") => Reply::soap_ok("urn:schemas-upnp-org:service:RenderingControl:1", "GetVolume",
            &[("CurrentVolume", &current.load(Ordering::Relaxed).to_string())]),
        _ => answer(req),
    });
    let r = Rig::new(&den, UserConfig::in_memory());
    let mut settings = r.app.speaker_settings(DEN);
    settings.volume_limit = Some(20);
    r.app.set_speaker_settings(DEN, settings);
    for level in [60, 10] {
        volume.store(level, Ordering::Relaxed);
        let before = den.requests().len();
        r.app.chime_beside(DEN, true, None, true).unwrap();
        let requests = den.requests();
        let sent = &requests[before..];
        let play = sent.iter().position(|r| r.soap_action() == Some("Play")).unwrap();
        let set = sent.iter().position(|r| r.soap_action() == Some("SetVolume"));
        if level > 20 {
            let set = set.unwrap();
            assert!(set < play && sent[set].body_text().contains(">20<"));
        } else { assert!(set.is_none()); }
    }
}

#[test]
fn use_steadier_while_another_place_plays_sets_the_next_switches_way() {
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: Some(WayChoice::AirPlayShort) });
    r.settle();
    r.send(Command::StayOnThisPc);
    r.settle();
    r.windows.app_plays("Music", SPEAKERS, Some(0.5));
    r.windows.player("Music", Some(Status::Playing));
    r.driver.look();
    r.send(Command::UseDelayChoice { speaker: speaker.clone(), choice: crate::model::facts::DelayChoice::Steady });
    assert!(matches!(r.driver.situation().spot, Spot::ThisPc(_)));
    assert!(r.driver.jobs.is_empty());
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: None });
    r.settle();
    assert_eq!(r.driver.bound().unwrap().0, speaker.as_str());
    assert_eq!(r.driver.core.kept().ways[&speaker], WayChoice::AirPlaySteady);
    assert_eq!(r.app.delay_status_of(speaker.as_str()).choice, Some(crate::delay::DelayChoice::Steady));
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn restoring_an_airplay_level_keeps_the_receiver_muted_until_unmute() {
    use crate::model::command::VolumeOwner;
    let (sent, levels) = unbounded();
    let receiver = FakeRaop::start(move |req| {
        if let Some(db) = std::str::from_utf8(&req.body).ok().and_then(|body| body.strip_prefix("volume: ")) {
            sent.send(db.trim().parse::<f32>().unwrap()).unwrap();
        }
        Answer::Normal
    });
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), None);
    let speaker = SpeakerId::new("airplay:0A1B2C3D4E5F");
    let owner = VolumeOwner::Speaker(speaker.clone());
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: None });
    r.settle();
    r.send(Command::SetMuted { owner: owner.clone(), muted: true });
    while levels.recv_timeout(WAIT).unwrap() != -144.0 {}
    let (pushed, slider) = unbounded();
    r.app.set_windows_level_through(move |level, muted| pushed.send((level, muted)).unwrap());
    r.driver.execute(vec![Do::SetLevel { speaker, level: 42 }]);
    assert_eq!(slider.try_recv(), Ok((42, true)));
    assert!(crate::sendspin::server::test_player::eventually(|| r.app.session.lock().unwrap()
        .as_ref().and_then(|s| s.level_on_connect()) == Some(42)));
    assert!(r.app.vsync.is_muted());
    assert!(levels.is_empty());
    r.send(Command::SetMuted { owner, muted: false });
    let db = levels.recv_timeout(WAIT).unwrap();
    assert!((db - crate::airplay::session::volume_pct_to_raop_db(42)).abs() < 0.001);
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn a_fixed_volume_sendspin_player_starts_with_the_apps_mute() {
    use crate::sendspin::{server::{test_player, SendspinDiscoveryState}, ws::WsMessage};
    let app = App::for_tests();
    let (renderer, player) = test_player::listen(&[], None);
    let speaker = SpeakerId::new(&renderer.stable_id());
    let mut kept = Kept::default();
    kept.levels.insert(speaker.clone(), KeptLevel { level: 27, at: clock_since(Instant::now()).wall });
    app.user_config.lock().unwrap().sound_place = Some(kept);
    let discovery = SendspinDiscoveryState::new();
    discovery.insert(renderer);
    *app.sendspin.discovery.lock().unwrap() = Some(discovery);
    app.user_config.lock().unwrap().sendspin.enabled = true;
    let mut r = Rig::on(app);
    r.app.vsync.note_muted(true);
    r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: None });
    r.settle();
    assert_eq!(r.app.current_volume(), Some(27));
    assert!(r.app.vsync.is_muted());
    let (_peer, heard) = player.recv_timeout(WAIT).unwrap();
    let frame = crate::http_server::PcmFrame(Arc::new(16_000i16.to_le_bytes().repeat(882 * 2 * 4)));
    let hub = r.app.hub.clone();
    while heard.try_recv().is_ok() {}
    let deadline = Instant::now() + WAIT;
    let next_frame = || loop {
        hub.publish(frame.clone());
        if let WsMessage::Binary(bytes) = heard.recv_deadline(deadline).unwrap() {
            assert_eq!(bytes.len(), 9 + 882 * 2 * 2);
            break bytes;
        }
    };
    // Keep feeding the tone past the player's initial silence.
    for _ in 0..8 {
        assert!(next_frame()[9..].iter().all(|b| *b == 0));
    }
    r.send(Command::SetVolume { owner: crate::model::command::VolumeOwner::Speaker(speaker), level: 50 });
    assert_eq!(r.app.current_volume(), Some(50));
    assert!(!r.app.vsync.is_muted());
    loop {
        let bytes = next_frame();
        if bytes[9..].iter().any(|b| *b != 0) {
            assert!(bytes[9..].chunks_exact(2).all(|sample| sample == 2_000i16.to_le_bytes()));
            break;
        }
    }
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn a_fixed_volume_sendspin_player_has_sliders_and_starts_at_its_saved_level() {
    use crate::model::{command::FirstRunAction as A, situation::FirstRunStep};
    use crate::sendspin::server::{test_player, SendspinDiscoveryState};
    let mut cfg = UserConfig::in_memory();
    for restart in [false, true] {
        let app = App::with_settings(App::test_config(), cfg.clone(), None, None);
        let (renderer, _player) = test_player::listen(&[], None);
        let speaker = SpeakerId::new(&renderer.stable_id());
        let discovery = SendspinDiscoveryState::new();
        discovery.insert(renderer);
        *app.sendspin.discovery.lock().unwrap() = Some(discovery);
        app.user_config.lock().unwrap().sendspin.enabled = true;
        let mut r = Rig::on(app);
        r.send(Command::Switch { place: Place::Speaker(speaker.clone()), way: None });
        r.settle();
        let level = if restart { 37 } else { 100 };
        let view = crate::model::view::view(r.driver.scene(), None, false, 1);
        let slider = view.panel.place.volume.unwrap();
        assert_eq!(slider.owner, Some(crate::model::command::VolumeOwner::Speaker(speaker.clone())));
        assert_eq!(u32::from(slider.level), level);
        assert_eq!(r.app.current_volume(), Some(level));
        assert!(test_player::eventually(|| r.app.session.lock().unwrap().as_ref().and_then(|s| s.level_on_connect()) == Some(level)));
        if !restart {
            let from_keys = r.app.vsync.driver_changed(42).unwrap();
            r.app.send_to_speaker(crate::volume_sync::SpeakerCommand::Volume(from_keys));
            assert!(test_player::eventually(|| r.app.session.lock().unwrap().as_ref().and_then(|s| s.level_on_connect()) == Some(42)));
            r.send(Command::FirstRunAction(A::ShowAgain));
            r.send(Command::FirstRunAction(A::Chime(speaker.clone())));
            r.settle();
            assert_eq!(r.driver.situation().first_run, Some(FirstRunStep::Two { speaker: speaker.clone(), level: Some(42), limit: 100 }));
            r.send(Command::FirstRunAction(A::ChimeVolume(37)));
            r.send(Command::FirstRunAction(A::Heard));
            assert_eq!(r.app.current_volume(), Some(37));
            assert_eq!(r.driver.core.kept().levels[&speaker].level, 37);
        }
        r.send(Command::StayOnThisPc);
        r.settle();
        cfg = r.app.user_config.lock().unwrap().clone();
    }
}

#[test]
fn a_player_request_waits_while_windows_decides_and_yields_to_the_persons_choice() {
    let den = FakeRenderer::start(answer);
    let office = FakeRenderer::start(|req| {
        if req.method == "GET" { return Reply::ok(&renderer_description("Office", &[])); }
        answer(req)
    });
    let mut r = Rig::new(&den, UserConfig::in_memory());
    r.pick();
    r.settle();
    r.send(Command::StayOnThisPc);
    r.settle();
    let found = r.app.discovery.as_ref().unwrap();
    let mut speakers = found.renderers();
    speakers.push(crate::ssdp::fetch_and_parse_device(&office.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap());
    found.replace(speakers);
    r.advance(1_100);
    let picks_seen = r.app.user_pick_count();
    // The person chooses the app's output, named after Den, in Windows.
    r.windows.choose_in_windows(OURS);
    r.driver.input(PlaceInput::System(SystemEvent::DefaultOutputChanged));
    assert!(r.driver.core.deciding_output());
    let (answered, answers) = bounded(1);
    let reply: crate::engine::place_driver::run::Answer = Box::new(move |result, _| answered.send(result).unwrap());
    let request = Command::PlayWhenListed { speaker: SpeakerId::new("uuid:fake-office"), picks_seen };
    assert!(r.driver.input(PlaceInput::Command(Surface::CommandLine, request, Some(reply))).is_none());
    assert!(!r.driver.held.is_empty());
    assert!(r.driver.situation().picking.is_none());
    r.advance(3_000);
    assert!(r.driver.held.is_empty());
    let (reply, result) = r.driver.held_answers.pop().unwrap();
    reply(result, Arc::new(crate::model::view::view(r.driver.scene(), None, false, 1)));
    assert_eq!(answers.try_recv().unwrap(), Ok(Effect::Done));
    r.settle();
    assert_eq!(r.driver.situation().place_speaker(), Some(&id()));
    assert_eq!(r.driver.bound().unwrap().0, DEN);
    assert!(!office.soap_actions().contains(&"Play".to_string()));
}


#[test]
fn player_and_pairing_requests_wait_when_headphones_arrive_before_windows_changes_output() {
    let den = FakeRenderer::start(answer);
    let mut r = Rig::new(&den, UserConfig::in_memory());
    let picks_seen = r.app.user_pick_count();
    r.windows.set_output_on(HEADPHONES, true);
    r.driver.look();
    assert!(r.driver.core.deciding_output());
    for cmd in [Command::PlayWhenListed { speaker: id(), picks_seen }, Command::PlayAfterPairing { speaker: id(), picks_seen }] {
        r.driver.input(PlaceInput::Command(Surface::Panel, cmd, None));
    }
    assert_eq!(r.driver.held.len(), 2);
    assert!(r.driver.jobs.is_empty());
    r.advance(3_000);
    r.settle();
    assert!(!r.driver.core.deciding_output());
    assert!(r.driver.held.is_empty());
    assert_eq!(r.default(), output(HEADPHONES));
    assert!(r.driver.bound().is_none());
    assert!(den.soap_actions().is_empty());
}

#[test]
fn the_other_way_of_a_silenced_speaker_waits_for_its_stop() {
    let (stopping, stopped) = bounded(1);
    let (go, resume) = bounded(1);
    let stops = AtomicU64::new(0);
    let den = FakeRenderer::start(move |req| {
        if req.method == "GET" { return Reply::ok(&renderer_description("Den", &["ZoneGroupTopology"])); }
        if req.soap_action() == Some("Stop") && stops.fetch_add(1, Ordering::SeqCst) == 1 {
            stopping.send(()).unwrap();
            resume.recv_timeout(WAIT).unwrap();
        }
        answer(req)
    });
    let found = DiscoveryState::new();
    found.replace(vec![crate::ssdp::fetch_and_parse_device(&den.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap()]);
    let receiver = FakeRaop::start(|_| Answer::Normal);
    let mut r = airplay_rig(&receiver, UserConfig::in_memory(), Some(found));
    r.playing();
    // This PC: the core silences Den by its UPnP way, and its Stop is held.
    r.send(Command::StayOnThisPc);
    stopped.recv_timeout(WAIT).unwrap();
    r.send(Command::Switch { place: Place::Speaker(id()), way: Some(WayChoice::AirPlaySteady) });
    assert!(!receiver.wait_for_requests(1, Duration::from_millis(250)));
    go.send(()).unwrap();
    r.settle();
    assert_eq!(r.driver.bound().unwrap().0, "airplay:0A1B2C3D4E5F");
    assert_eq!(r.driver.situation().place_speaker(), Some(&id()));
}
