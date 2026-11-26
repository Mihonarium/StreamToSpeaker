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
    assert_eq!((&s.speakers[0].id, s.speakers[0].state), (&id(), RowState::Connecting));
    go.send(()).unwrap();
    r.settle();
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
fn a_failed_or_cancelled_change_after_releasing_the_other_way_moves_playing_sound_to_pc() {
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
        if cancel { r.send(Command::CancelSwitch); }
        go.send(()).unwrap();
        r.settle();
        let situation = r.driver.situation();
        assert!(matches!(situation.spot, Spot::ThisPc(_)));
        assert!(matches!(situation.note.unwrap().cause, Cause::StoppedResponding { speaker, .. } if speaker == id()));
        assert_eq!(r.default(), output(SPEAKERS));
        assert_eq!(r.driver.core.kept().place, None);
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
