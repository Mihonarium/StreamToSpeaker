//! A stepped driver, Windows in memory and a speaker on loopback.

use super::*;
use crate::app::AppConfig;
use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT};
use crate::model::facts::{Presence, SpeakerKind};
use crate::model::situation::{Cause, Doing, Spot, Standing, Situation};
use crate::model::state::Place;
use crate::platform::fake::{Asked, FakePlatform, Passed, HEADPHONES, HOME_NET, OURS, SPEAKERS};
use crate::platform::{PassThrough, PlayerStatus as Status, Role};
use crate::ssdp::{DiscoveryState, ServiceUrls};
use crate::user_config::UserConfig;
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};
use crossbeam_channel::{bounded, unbounded, Receiver};
use std::sync::atomic::{AtomicBool, AtomicU64};

const WAIT: Duration = Duration::from_secs(20);
const DEN: &str = "uuid:fake-den";

fn id() -> SpeakerId {
    SpeakerId::new(DEN)
}
fn output(s: &str) -> PcOutputId {
    PcOutputId::new(s)
}

struct Rig {
    app: Arc<App>,
    windows: Arc<FakePlatform>,
    driver: PlaceDriver,
    rx: Receiver<PlaceInput>,
    _host: TestApp,
}

struct TestApp {
    app: Arc<App>,
    server: Option<JoinHandle<()>>,
}

impl TestApp {
    fn with_speaker(fake: &FakeRenderer, cfg: UserConfig) -> Self {
        let found = DiscoveryState::new();
        let r = crate::ssdp::fetch_and_parse_device(&fake.url("/description.xml"), WAIT, ServiceUrls::AsGiven).unwrap();
        assert_eq!(r.stable_id(), DEN);
        found.replace(vec![r]);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap();
        let app = App::with_settings(AppConfig { bind, ..App::test_config() }, cfg, Some(found), None);
        let config = crate::http_server::HttpServerConfig::minimal(bind, app.hub.clone());
        let stopped = app.clone();
        let server = crate::http_server::serve_on(listener, config, move || stopped.is_shutting_down()).unwrap();
        Self { app, server: Some(server) }
    }
}

impl Rig {
    fn new(fake: &FakeRenderer, cfg: UserConfig) -> Self {
        let windows = FakePlatform::new();
        Self::with_host(TestApp::with_speaker(fake, cfg), windows.clone(), windows.platform())
    }

    fn on(app: Arc<App>) -> Self {
        let windows = FakePlatform::new();
        let platform = windows.platform();
        Self::with_platform(app, windows, platform)
    }

    fn with_platform(app: Arc<App>, windows: Arc<FakePlatform>, platform: Platform) -> Self {
        Self::with_host(TestApp { app, server: None }, windows, platform)
    }

    fn with_host(host: TestApp, windows: Arc<FakePlatform>, platform: Platform) -> Self {
        let app = host.app.clone();
        app.capture_tap.get_or_init(|| tap::Tap::with_frames(WIRE_SAMPLE_RATE as usize / 2));
        windows.set_output_on(HEADPHONES, false);
        let mut driver = PlaceDriver::new(app.clone(), platform, Arc::new(Mutex::new(None)), Instant::now());
        let rx = driver.events.clone();
        driver.start(StartKind::Normal);
        Self { _host: host, app, windows, driver, rx }
    }

    fn send(&mut self, cmd: Command) {
        self.driver.command(Surface::Panel, &cmd).unwrap();
    }

    fn pick(&mut self) {
        self.send(Command::Switch { place: Place::Speaker(id()), way: None });
    }

    fn default(&self) -> PcOutputId {
        self.windows.default_for(Role::Console).unwrap()
    }

    /// One first answer, followed by the end of this same connect.
    fn answered_connect(&mut self) -> PlaceInput {
        let PlaceInput::Answered(gen) = self.rx.recv_timeout(WAIT).unwrap() else { panic!("expected the first answer") };
        self.driver.input(PlaceInput::Answered(gen));
        let end = self.rx.recv_timeout(WAIT).unwrap();
        assert!(matches!(end, PlaceInput::Event(Event::Connected { gen: n, .. }) if n == gen));
        end
    }

    /// Pump completions until the work they started has ended too.
    fn settle(&mut self) {
        let end = Instant::now() + WAIT;
        loop {
            while let Ok(input) = self.rx.try_recv() {
                self.driver.input(input);
            }
            if self.driver.threads.iter().all(|t| t.is_finished()) {
                for t in self.driver.threads.drain(..) {
                    t.join().unwrap();
                }
                if let Ok(input) = self.rx.try_recv() {
                    self.driver.input(input);
                    continue;
                }
                break;
            }
            assert!(Instant::now() < end, "worker didn't finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        if let Some(worker) = &self.driver.pass {
            worker.settle(WAIT);
        }
    }

    fn advance(&mut self, ms: u64) {
        self.driver.skip(ms);
        self.driver.look();
    }

    fn playing(&mut self) {
        self.pick();
        self.settle();
        self.windows.app_plays("Music", OURS, Some(0.5));
        self.windows.player("Music", Some(Status::Playing));
        self.driver.look();
        assert_eq!(self.driver.situation().doing, Doing::Playing);
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.app.request_shutdown();
        if let Some(server) = self.server.take() {
            let _ = std::net::TcpStream::connect_timeout(&self.app.config.bind, WAIT);
            let end = Instant::now() + WAIT;
            while !server.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(1));
            }
            if server.is_finished() {
                let _ = server.join();
            } else if !std::thread::panicking() {
                panic!("the stream server didn't stop");
            }
        }
    }
}

fn answer(req: &crate::fake_renderer::Request) -> Reply {
    match req.soap_action() {
        Some("GetTransportInfo") => {
            Reply::soap_ok(AV_TRANSPORT, "GetTransportInfo", &[("CurrentTransportState", "STOPPED")])
        }
        Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
        None if req.method == "GET" => Reply::ok(&renderer_description("Den", &[])),
        None => {
            static SID: AtomicU64 = AtomicU64::new(0);
            let n = SID.fetch_add(1, Ordering::Relaxed);
            Reply::raw(format!(
                "HTTP/1.1 200 OK\r\nSID: uuid:place-{n}\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"
            ))
        }
    }
}

fn held_renderer(action: &'static str) -> (FakeRenderer, Receiver<()>, Sender<()>) {
    let (reached, rx) = bounded(1);
    let (go, resume) = bounded(1);
    let first = AtomicBool::new(true);
    let fake = FakeRenderer::start(move |req| {
        if req.soap_action() == Some(action) && first.swap(false, Ordering::SeqCst) {
            reached.send(()).unwrap();
            let _ = resume.recv_timeout(WAIT);
        }
        answer(req)
    });
    (fake, rx, go)
}

fn remembered() -> UserConfig {
    let mut cfg = UserConfig::in_memory();
    cfg.sound_place = Some(Kept { place: Some(id()), named: Some(id()), first_run_done: true, ..Default::default() });
    cfg
}

#[test]
fn panel_switch_moves_windows_only_when_ready_and_keeps_both_settings() {
    let (fake, reached, go) = held_renderer("Play");
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    reached.recv_timeout(WAIT).unwrap();
    assert_eq!(r.default(), output(SPEAKERS));
    assert_eq!(r.driver.situation().picking.unwrap().speaker, id());
    go.send(()).unwrap();
    r.settle();
    assert_eq!(r.default(), output(OURS));
    assert_eq!(r.driver.situation().spot, Spot::Speaker(id()));
    assert_eq!(r.windows.default_for(Role::Communications), Some(output(SPEAKERS)));
    let cfg = r.app.user_config.lock().unwrap();
    let kept = cfg.sound_place.as_ref().unwrap();
    assert_eq!((&kept.place, &kept.named), (&Some(id()), &Some(id())));
    assert_eq!(cfg.last_speaker_id.as_deref(), Some(DEN));
    assert!(!cfg.chose_this_pc);
    assert_eq!(cfg.pc_output.as_deref(), Some(SPEAKERS));
    assert_eq!(cfg.speaker_settings[DEN].network, Some(NetId(HOME_NET.into())));
}

#[test]
fn stop_answers_before_windows_moves_back() {
    let (at_stop, reached) = bounded(1);
    let (go, resume) = bounded(1);
    let stops = AtomicU64::new(0);
    let fake = FakeRenderer::start(move |req| {
        if req.soap_action() == Some("Stop") && stops.fetch_add(1, Ordering::SeqCst) == 1 {
            at_stop.send(()).unwrap();
            let _ = resume.recv_timeout(WAIT);
        }
        answer(req)
    });
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.playing();
    r.send(Command::StayOnThisPc);
    reached.recv_timeout(WAIT).unwrap();
    assert_eq!(r.default(), output(OURS));
    go.send(()).unwrap();
    r.settle();
    assert_eq!(r.default(), output(SPEAKERS));
    assert!(r.driver.bound().is_none());
}

#[test]
fn restart_waits_for_sound_and_its_player() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, remembered());
    r.advance(3_010);
    assert_eq!(r.default(), output(OURS));
    r.advance(60_000);
    assert!(fake.soap_actions().is_empty());
    r.windows.player("Music", Some(Status::Playing));
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.driver.look();
    r.settle();
    assert_eq!(r.driver.situation().doing, Doing::Playing);
    assert_eq!(r.driver.bound().unwrap().0, DEN);
}

#[test]
fn a_short_sound_replays_once_without_connecting() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, remembered());
    r.advance(3_010);
    r.windows.app_plays("Chat", OURS, Some(0.5));
    r.advance(250);
    r.app.capture_tap.get().unwrap().block(&[0, 0, 0, 6, 8, 9]);
    r.windows.app_plays("Chat", OURS, None);
    r.advance(1_100);
    r.advance(250);
    r.settle();
    assert_eq!(
        r.windows.passed(),
        [
            Passed::Opened(output(SPEAKERS)),
            Passed::Wrote { frames: 2, rate: crate::WIRE_SAMPLE_RATE, channels: crate::WIRE_CHANNELS },
            Passed::Closed,
        ]
    );
    assert!(fake.soap_actions().is_empty());
}

#[test]
fn pass_through_writes_only_while_open() {
    let mut r = Rig::on(App::for_tests());
    r.settle();
    let previous = r.windows.passed().len();
    r.driver.execute(vec![Do::PassThrough(Some(output(SPEAKERS)))]);
    r.settle();
    r.app.capture_tap.get().unwrap().block(&[1, 2, 3, 4]);
    r.driver.execute(vec![Do::PassThrough(None)]);
    r.settle();
    r.app.capture_tap.get().unwrap().block(&[5, 6]);
    assert_eq!(
        r.windows.passed()[previous..],
        [
            Passed::Opened(output(SPEAKERS)),
            Passed::Wrote { frames: 2, rate: crate::WIRE_SAMPLE_RATE, channels: crate::WIRE_CHANNELS },
            Passed::Closed,
        ]
    );
}

struct ObservedPass {
    windows: Arc<FakePlatform>,
    delay: Duration,
    writes: Sender<(Instant, Vec<i16>)>,
    // Interruption tests hold the first write until the replacement is queued.
    release: Option<Receiver<()>>,
}

impl PassThrough for ObservedPass {
    fn open(&self, output: &PcOutputId) -> Result<(), String> {
        std::thread::sleep(self.delay);
        self.windows.open(output)
    }

    fn write(&self, samples: &[i16], rate: u32, channels: u16) {
        let at = Instant::now();
        self.windows.write(samples, rate, channels);
        let _ = self.writes.send((at, samples.to_vec()));
        if let Some(release) = &self.release {
            assert_ne!(release.recv_timeout(WAIT), Err(crossbeam_channel::RecvTimeoutError::Timeout));
        }
    }

    fn close(&self) {
        std::thread::sleep(self.delay);
        self.windows.close();
    }
}

fn observed_rig(delay: Duration, release: Option<Receiver<()>>) -> (Rig, Receiver<(Instant, Vec<i16>)>) {
    let windows = FakePlatform::new();
    let mut platform = windows.platform();
    let (writes, rx) = unbounded();
    platform.pass = Arc::new(ObservedPass { windows: windows.clone(), delay, writes, release });
    let mut r = Rig::with_platform(App::for_tests(), windows, platform);
    r.driver.execute(vec![Do::PassThrough(None)]);
    r.settle();
    (r, rx)
}

fn wrote(frames: usize) -> Passed {
    Passed::Wrote { frames, rate: WIRE_SAMPLE_RATE, channels: WIRE_CHANNELS }
}

#[test]
fn replay_writes_every_sample_in_order_at_capture_speed() {
    let (mut r, writes) = observed_rig(Duration::ZERO, None);
    let frames = WIRE_SAMPLE_RATE as usize * 450 / 1000;
    let samples: Vec<_> = (0..frames * WIRE_CHANNELS as usize).map(|i| (i as i16).wrapping_add(1)).collect();
    r.app.capture_tap.get().unwrap().block(&samples);
    r.driver.execute(vec![Do::ReplayShort { on: output(SPEAKERS), ms: 1_000 }]);
    r.settle();

    let writes: Vec<_> = writes.try_iter().collect();
    let block_samples = WIRE_SAMPLE_RATE as usize / 10 * WIRE_CHANNELS as usize;
    assert_eq!(writes.len(), samples.len().div_ceil(block_samples));
    for ((_, actual), expected) in writes.iter().zip(samples.chunks(block_samples)) {
        assert_eq!(actual, expected);
    }
    let elapsed = writes.last().unwrap().0 - writes[0].0;
    assert!(elapsed >= Duration::from_millis(450 * 8 / 10), "first to last write took {elapsed:?}");
}

#[test]
fn replay_and_pass_changes_return_promptly_with_a_slow_output() {
    let (mut r, writes) = observed_rig(Duration::from_millis(300), None);
    let samples = vec![1; WIRE_SAMPLE_RATE as usize / 2 * WIRE_CHANNELS as usize];
    r.app.capture_tap.get().unwrap().block(&samples);
    let prompt = |driver: &mut PlaceDriver, action| {
        let start = Instant::now();
        driver.execute(vec![action]);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(50), "driver took {elapsed:?}");
    };
    prompt(&mut r.driver, Do::ReplayShort { on: output(SPEAKERS), ms: 1_000 });
    r.app.capture_tap.get().unwrap().block(&vec![2; samples.len()]);
    r.settle();
    assert_eq!(writes.try_iter().flat_map(|(_, block)| block).collect::<Vec<_>>(), samples);
    prompt(&mut r.driver, Do::PassThrough(Some(output(SPEAKERS))));
    r.settle();
    assert_eq!(r.windows.passing_to(), Some(output(SPEAKERS)));
    prompt(&mut r.driver, Do::PassThrough(None));
    r.settle();
    assert_eq!(r.windows.passing_to(), None);
}

#[test]
fn changing_output_during_replay_switches_directly_to_live_sound() {
    interrupt_replay(Some(output(HEADPHONES)));
}

#[test]
fn turning_passing_off_during_replay_closes_and_stops_writing() {
    interrupt_replay(None);
}

fn interrupt_replay(next: Option<PcOutputId>) {
    let (release, held) = bounded(0);
    let (mut r, writes) = observed_rig(Duration::ZERO, Some(held));
    r.windows.set_output_on(HEADPHONES, true);
    let tap = r.app.capture_tap.get().unwrap();
    tap.block(&vec![1; WIRE_SAMPLE_RATE as usize / 2 * WIRE_CHANNELS as usize]);
    r.driver.execute(vec![Do::PassThrough(Some(output(SPEAKERS)))]);
    r.settle();
    let previous = r.windows.passed().len();
    r.driver.execute(vec![Do::ReplayShort { on: output(SPEAKERS), ms: 1_000 }]);
    writes.recv_timeout(WAIT).unwrap();
    r.driver.execute(vec![Do::PassThrough(next.clone())]);
    drop(release);
    r.settle();
    r.app.capture_tap.get().unwrap().block(&[2, 3]);

    let mut events = vec![Passed::Opened(output(SPEAKERS)), wrote(WIRE_SAMPLE_RATE as usize / 10)];
    if let Some(id) = next {
        events.extend([Passed::Opened(id), wrote(1)]);
        assert_eq!(writes.try_recv().unwrap().1, [2, 3]);
    } else {
        events.push(Passed::Closed);
    }
    assert!(writes.try_recv().is_err());
    assert_eq!(r.windows.passed()[previous..], events);
}

#[test]
fn finished_replay_restores_the_requested_live_output() {
    for passing in [None, Some(output(HEADPHONES))] {
        let mut r = Rig::on(App::for_tests());
        r.windows.set_output_on(HEADPHONES, true);
        r.driver.execute(vec![Do::PassThrough(passing.clone())]);
        r.settle();
        r.app.capture_tap.get().unwrap().block(&[1, 2]);
        let previous = r.windows.passed().len();
        r.driver.execute(vec![Do::ReplayShort { on: output(SPEAKERS), ms: 1_000 }]);
        r.settle();
        r.app.capture_tap.get().unwrap().block(&[3, 4]);
        assert_eq!(r.windows.passing_to(), passing);
        let mut events = vec![Passed::Opened(output(SPEAKERS)), wrote(1), Passed::Closed];
        if let Some(id) = passing {
            events.extend([Passed::Opened(id), wrote(1)]);
        }
        assert_eq!(r.windows.passed()[previous..], events);
    }
}

#[test]
fn a_failed_live_open_remains_the_requested_output_for_replay() {
    let mut r = Rig::on(App::for_tests());
    r.settle();
    let previous = r.windows.passed().len();
    r.driver.execute(vec![Do::PassThrough(Some(output(SPEAKERS))), Do::PassThrough(Some(output(HEADPHONES)))]);
    r.settle();
    assert_eq!(r.windows.passed()[previous..], [Passed::Opened(output(SPEAKERS)), Passed::Closed]);
    assert_eq!(r.driver.passing, Some(output(HEADPHONES)));
    r.app.capture_tap.get().unwrap().block(&[1, 2]);
    r.windows.set_output_on(HEADPHONES, true);
    r.driver.execute(vec![Do::ReplayShort { on: output(SPEAKERS), ms: 1_000 }]);
    r.settle();
    assert_eq!(r.windows.passing_to(), Some(output(HEADPHONES)));
}

#[test]
fn stop_joins_the_output_worker_during_replay_without_a_live_output() {
    let (mut r, writes) = observed_rig(Duration::ZERO, None);
    r.app.capture_tap.get().unwrap().block(&vec![1; WIRE_SAMPLE_RATE as usize / 2 * WIRE_CHANNELS as usize]);
    r.driver.execute(vec![Do::ReplayShort { on: output(SPEAKERS), ms: 1_000 }]);
    writes.recv_timeout(WAIT).unwrap();
    r.driver.input(PlaceInput::Stop);
    assert!(r.driver.pass.is_none());
    assert_eq!(Arc::strong_count(&r.driver.platform.pass), 1);
    assert_eq!(r.windows.passing_to(), None);
    assert_eq!(r.windows.passed().last(), Some(&Passed::Closed));
    assert!(writes.try_iter().count() < 4, "stop let the whole replay play");
    r.app.capture_tap.get().unwrap().block(&[2, 3]);
    assert!(writes.try_recv().is_err());
}

#[test]
fn a_lost_fetch_is_tried_then_left_on_this_pc_even_when_the_speaker_answers() {
    drop_during_play(false);
}

#[test]
fn losing_the_network_names_this_pcs_connection() {
    drop_during_play(true);
}

fn drop_during_play(offline: bool) {
    let failing = Arc::new(AtomicBool::new(false));
    let down = failing.clone();
    let fake = FakeRenderer::start(move |req| {
        if req.soap_action().is_some() && down.load(Ordering::Relaxed) {
            Reply::soap_fault(705, "Unavailable")
        } else {
            answer(req)
        }
    });
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.advance(60_001);
    r.playing();
    let fetch = r.app.hub.subscribe_from("127.0.0.1".parse().unwrap());
    r.driver.look();
    drop(fetch);
    r.app.hub.publish(crate::http_server::PcmFrame(Arc::new(vec![0; 4])));
    if offline {
        r.windows.connect_to(vec![]);
        r.driver.input(PlaceInput::System(SystemEvent::NetworkChanged));
    }
    failing.store(true, Ordering::Relaxed);
    for _ in 0..60 {
        r.advance(250);
        r.settle();
    }
    assert_eq!(r.default(), output(SPEAKERS));
    let situation = r.driver.situation();
    assert_eq!(situation.spot, Spot::ThisPc(Some(output(SPEAKERS))));
    let note = situation.note.unwrap();
    assert_eq!(note.cause.speaker(), &id());
    if offline {
        assert!(matches!(note.cause, Cause::NetworkLost { .. }));
    } else {
        assert!(matches!(note.cause, Cause::StoppedResponding { .. }));
    }
    assert!(!note.available_again);
    assert_eq!(fake.soap_actions().iter().filter(|a| *a == "SetAVTransportURI").count(), 4);
    failing.store(false, Ordering::Relaxed);
    if offline {
        r.windows.connect_to(vec![crate::platform::NetConnection {
            id: HOME_NET.into(),
            name: None,
            kind: crate::platform::ConnKind::WiFi,
        }]);
    }
    r.advance(16_000);
    r.settle();
    assert!(r.driver.situation().note.unwrap().available_again);
    assert_eq!(r.default(), output(SPEAKERS));
    assert!(r.driver.bound().is_none());
}

#[test]
fn sound_start_leaves_a_busy_speaker_alone() {
    let fake = FakeRenderer::start(|req| match req.soap_action() {
        Some("GetTransportInfo") => {
            Reply::soap_ok(AV_TRANSPORT, "GetTransportInfo", &[("CurrentTransportState", "PLAYING")])
        }
        Some("GetMediaInfo") => {
            Reply::soap_ok(AV_TRANSPORT, "GetMediaInfo", &[("CurrentURI", "x-sonos-spotify:track")])
        }
        _ => answer(req),
    });
    let mut r = Rig::new(&fake, remembered());
    r.advance(3_010);
    r.windows.app_plays("Music", OURS, Some(0.5));
    r.windows.player("Music", Some(Status::Playing));
    r.driver.look();
    let end = r.answered_connect();
    r.driver.input(end);
    r.settle();
    assert_eq!(r.default(), output(SPEAKERS));
    assert!(matches!(r.driver.situation().note.unwrap().cause, Cause::PlayingElse { speaker, .. } if speaker == id()));
    assert_eq!(fake.soap_actions(), ["GetTransportInfo", "GetMediaInfo"]);
}

#[test]
fn a_refused_windows_switch_restores_the_old_place() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.windows.refuse_switches(true);
    r.pick();
    r.settle();
    let s = r.driver.situation();
    assert_eq!(s.spot, Spot::ThisPc(Some(output(SPEAKERS))));
    assert!(s.standing.contains(&Standing::WindowsDidntSwitch { speaker: id() }));
    assert!(r.driver.bound().is_none());
}

#[test]
fn headphones_take_the_sound_and_leave_it_on_this_pc() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.playing();
    r.windows.set_output_on(HEADPHONES, true);
    r.windows.choose_in_windows(HEADPHONES);
    r.driver.input(PlaceInput::System(SystemEvent::DefaultOutputChanged));
    r.settle();
    assert_eq!(r.driver.situation().spot, Spot::ThisPc(Some(output(HEADPHONES))));
    assert!(r.driver.bound().is_none());
    assert_eq!(fake.soap_actions().iter().filter(|a| *a == "Stop").count(), 2);
    r.windows.set_output_on(HEADPHONES, false);
    r.windows.choose_in_windows(SPEAKERS);
    r.driver.look();
    let s = r.driver.situation();
    assert_eq!(s.spot, Spot::ThisPc(Some(output(SPEAKERS))));
    assert!(matches!(s.note.unwrap().cause, Cause::HeadphonesGone { before, .. } if before == id()));
}

#[test]
fn transport_pause_reaches_the_player_and_play_follows_the_core_rule() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.playing();
    r.windows.player("Other", Some(Status::Playing));
    r.app.note_transport(crate::gena::TransportChange { state: Some("PAUSED_PLAYBACK".into()), ..Default::default() });
    r.driver.look();
    assert_eq!(r.windows.asked(), [Asked::Pause("music".into())]);
    r.app.signals.push(DEN, SpeakerSignal::PlayPressed);
    r.driver.look();
    if !place::trials::T3_PLAY_ON_SPEAKER_STARTS_PLAYER {
        assert_eq!(r.windows.asked(), [Asked::Pause("music".into())]);
        r.driver.execute(vec![Do::Play(vec![PlayerId::new("music")])]);
    }
    assert_eq!(r.windows.asked(), [Asked::Pause("music".into()), Asked::Play("music".into())]);
}

#[test]
fn cancel_while_connecting_leaves_windows_and_the_session_alone() {
    let (fake, reached, go) = held_renderer("SetAVTransportURI");
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    reached.recv_timeout(WAIT).unwrap();
    r.send(Command::CancelSwitch);
    go.send(()).unwrap();
    r.settle();
    assert!(r.driver.bound().is_none());
    assert!(r.driver.situation().picking.is_none());
    assert_eq!(r.default(), output(SPEAKERS));
}

#[test]
fn cancel_after_bind_before_completion_releases_only_that_session() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.pick();
    let completion = r.answered_connect();
    assert!(r.driver.bound().is_some());
    r.send(Command::CancelSwitch);
    r.driver.input(completion);
    r.settle();
    assert!(r.driver.bound().is_none());
    assert_eq!(r.default(), output(SPEAKERS));
    assert_eq!(fake.soap_actions().iter().filter(|a| *a == "Stop").count(), 2);
}

#[test]
fn settings_from_before_restore_the_place_and_the_device_name() {
    let fake = FakeRenderer::start(answer);
    for pc in [false, true] {
        let dir = std::env::temp_dir().join(format!("sts-place-legacy-{}-{pc}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "last_speaker_id": DEN, "chose_this_pc": pc, "pc_output": SPEAKERS,
            })
            .to_string(),
        )
        .unwrap();
        let r = Rig::new(&fake, UserConfig::load_from(&path));
        let kept = r.driver.core.kept();
        assert_eq!(kept.place, (!pc).then(id));
        assert_eq!(kept.named, Some(id()));
        assert_eq!(kept.pc_output, Some(output(SPEAKERS)));
        assert!(kept.first_run_done);
        drop(r);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn a_sonos_and_its_twin_share_a_row_and_airplay_follows_the_delay_choice() {
    let row = |id: &str, kind, twin: Option<&str>| SpeakerFacts {
        id: SpeakerId::new(id),
        name: id.into(),
        address: String::new(),
        kind,
        presence: Presence::Found,
        twin: twin.map(SpeakerId::new),
        used_here: false,
        hidden: false,
        last_used: None,
    };
    let list = [
        row("ap", SpeakerKind::AirPlay, Some("sonos")),
        row("sonos", SpeakerKind::Sonos, Some("ap")),
        row("alone", SpeakerKind::AirPlay, None),
        row("pair", SpeakerKind::HomePodPair, None),
    ];
    let result = world::fold(&list, |id| match id {
        "ap" => Some(crate::delay::DelayChoice::Short),
        "alone" => Some(crate::delay::DelayChoice::Steady),
        _ => None,
    });
    use WayChoice::*;
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].1.ways, [Native, AirPlayShort, AirPlaySteady]);
    assert!(result[0].0.has("ap") && result[0].0.has("sonos"));
    assert_eq!(result[0].1.id.as_str(), "sonos");
    assert_eq!(result[1].1.ways, [AirPlaySteady, AirPlayShort]);
    assert_eq!(result[2].1.ways, [AirPlaySteady]);
}

mod connections;
mod scene;
mod run;
