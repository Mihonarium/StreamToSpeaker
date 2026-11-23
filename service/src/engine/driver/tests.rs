//! The engine against the app, fake speakers and a fake Windows.

use std::sync::atomic::{AtomicBool, AtomicU32};

use crossbeam_channel::{bounded, unbounded};

use super::*;
use crate::airplay::AirPlayDiscoveryState;
use crate::app::AppConfig;
use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT};
use crate::manual_speakers::{ManualKind, ManualSpeaker};
use crate::model::state::SpeakerId;
use crate::model::NowState;
use crate::platform::fake::{FakePlatform, OURS, SPEAKERS};
use crate::platform::Role;
use crate::user_config::UserConfig;

/// Long enough for anything on loopback; only reached when a test fails.
const WAIT: Duration = Duration::from_secs(20);

/// The engine's driver, stepped by the test: commands go in directly, and
/// what its threads report is taken in by [`Rig::until`].
struct Rig {
    app: Arc<App>,
    windows: Arc<FakePlatform>,
    driver: Driver,
    rx: Receiver<Input>,
    port: PortCore,
}

impl Rig {
    fn on(app: Arc<App>) -> Rig {
        let windows = FakePlatform::new();
        let (tx, rx) = unbounded();
        let now = Instant::now();
        let core = Core::new(None, None, now);
        let mut driver = Driver::new(app.clone(), windows.platform(), core, tx, Arc::new(Mutex::new(None)), now);
        let port = PortCore::new(driver.snapshot(1));
        Rig { app, windows, driver, rx, port }
    }

    fn send(&mut self, from: Surface, cmd: Command) -> Result<Effect, Refusal> {
        self.driver.command(from, &cmd, Instant::now())
    }

    fn play(&mut self, from: Surface, id: &str) -> Result<Effect, Refusal> {
        self.send(from, Command::Play(Place::Speaker(SpeakerId::new(id))))
    }

    /// Take what the engine's threads report until `done` holds.
    fn until(&mut self, what: &str, done: impl Fn(&Driver) -> bool) {
        let end = Instant::now() + WAIT;
        while !done(&self.driver) {
            match self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(input) => self.driver.input(input, &self.port),
                Err(_) => panic!("waited in vain until {what}"),
            }
        }
    }

    /// Wait for the engine's threads to end, then take what they said.
    fn settle(&mut self) {
        self.driver.join_threads();
        while let Ok(input) = self.rx.try_recv() {
            self.driver.input(input, &self.port);
        }
        self.driver.join_threads();
    }

    fn plays_on(&mut self, id: &str) {
        let place = Place::Speaker(SpeakerId::new(id));
        self.until("it plays", |d| d.core.bound() == Some(&place) && d.core.connecting().is_none());
    }

    fn state(&mut self) -> NowState {
        let now = Instant::now();
        let w = self.driver.world(now);
        self.driver.core.state(&w, now)
    }

    fn bound_in_app(&self) -> Option<String> {
        self.app.session.lock().unwrap().as_ref().map(|s| s.stable_id())
    }
}

/// Windows' default output for music and the like.
fn windows_default(rig: &Rig) -> String {
    rig.windows.default_for(Role::Console).map(|o| o.0.to_string()).unwrap_or_default()
}

/// An app whose stream server is up on loopback; settings in memory.
fn serving_app() -> Arc<App> {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let app = App::with_settings(
        AppConfig { advertise_ip: None, bind: SocketAddr::from(([127, 0, 0, 1], port)), ..App::test_config() },
        UserConfig::in_memory(),
        None,
        None,
    );
    crate::http_server::start_http_server(crate::http_server::HttpServerConfig::minimal(app.config.bind, app.hub.clone()))
        .unwrap();
    app
}

use std::net::SocketAddr;

/// A speaker added by the address of `fake`'s description; its id.
fn add_speaker(app: &App, fake: &FakeRenderer) -> String {
    let m = ManualSpeaker { kind: ManualKind::Upnp, name: String::new(), host: fake.url("/description.xml"), port: 0 };
    let id = m.id();
    app.user_config.lock().unwrap().manual_speakers.push(m);
    id
}

/// A GENA subscription answer with an id no other test uses.
fn subscribed() -> Reply {
    static N: AtomicU32 = AtomicU32::new(0);
    let sid = format!("uuid:engine-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
    Reply::raw(format!("HTTP/1.1 200 OK\r\nSID: {sid}\r\nTIMEOUT: Second-1800\r\nContent-Length: 0\r\n\r\n"))
}

/// What a plain renderer answers.
fn answer(req: &crate::fake_renderer::Request) -> Reply {
    match req.soap_action() {
        Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
        None if req.method == "GET" => Reply::ok(&renderer_description("Den Amp", &[])),
        None => subscribed(),
    }
}

/// A renderer that takes every request.
fn plain_renderer() -> FakeRenderer {
    FakeRenderer::start(answer)
}

/// A renderer that, at its first `hold` (a SOAP action, or a method such
/// as `SUBSCRIBE`), says so on `reached` and answers only once `go` is
/// sent.
struct Held {
    fake: FakeRenderer,
    reached: Receiver<()>,
    go: Sender<()>,
}

fn held_renderer(hold: &'static str) -> Held {
    let (reached_tx, reached) = bounded(1);
    let (go, go_rx) = bounded::<()>(1);
    let held = AtomicBool::new(false);
    let fake = FakeRenderer::start(move |req| {
        let what = req.soap_action().unwrap_or(req.method.as_str());
        if what == hold && !held.swap(true, Ordering::SeqCst) {
            let _ = reached_tx.send(());
            let _ = go_rx.recv_timeout(WAIT);
        }
        answer(req)
    });
    Held { fake, reached, go }
}

// ---- one owner, the latest choice ------------------------------------

#[test]
fn a_pick_plays_and_this_pc_gives_the_sound_back() {
    let fake = plain_renderer();
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &fake);
    assert_eq!(windows_default(&rig), SPEAKERS);

    rig.play(Surface::Panel, &id).unwrap();
    rig.plays_on(&id);
    assert_eq!(rig.bound_in_app().as_deref(), Some(id.as_str()));
    // Windows' sound goes to the app once the speaker plays.
    assert_eq!(windows_default(&rig), OURS);
    assert_eq!(rig.app.user_config.lock().unwrap().last_speaker_id.as_deref(), Some(id.as_str()));

    rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
    rig.settle();
    assert_eq!(rig.driver.core.bound(), None);
    assert_eq!(rig.bound_in_app(), None);
    assert_eq!(windows_default(&rig), SPEAKERS);
    assert_eq!(fake.soap_actions().last().map(String::as_str), Some("Stop"));
    // This PC is remembered as the choice; the speaker as the one named.
    let uc = rig.app.user_config.lock().unwrap();
    assert!(uc.chose_this_pc);
    assert_eq!(uc.last_speaker_id.as_deref(), Some(id.as_str()));
}

#[test]
fn sound_stays_where_it_is_until_the_speaker_plays() {
    let held = held_renderer("Play");
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &held.fake);
    rig.play(Surface::Panel, &id).unwrap();
    held.reached.recv_timeout(WAIT).unwrap();
    assert_eq!(windows_default(&rig), SPEAKERS);
    assert!(rig.windows.switches().is_empty());
    held.go.send(()).unwrap();
    rig.plays_on(&id);
    rig.settle();
    assert_eq!(windows_default(&rig), OURS);
    rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
    rig.settle();
}

/// A newer choice stops a connect at whatever step it has reached: what
/// the speaker was told is undone, nothing is bound, and the newer choice
/// stands.
#[test]
fn a_newer_choice_stops_a_upnp_connect_at_every_step() {
    for (hold, expected) in [
        ("Stop", &["Stop"][..]),
        ("SetAVTransportURI", &["Stop", "SetAVTransportURI", "Stop"][..]),
        ("Play", &["Stop", "SetAVTransportURI", "Play", "Stop"][..]),
        ("SUBSCRIBE", &["Stop", "SetAVTransportURI", "Play", "Stop"][..]),
    ] {
        let held = held_renderer(hold);
        let mut rig = Rig::on(serving_app());
        let id = add_speaker(&rig.app, &held.fake);
        rig.play(Surface::Panel, &id).unwrap();
        held.reached.recv_timeout(WAIT).unwrap_or_else(|_| panic!("{hold}: never reached"));
        assert!(rig.driver.core.connecting().is_some(), "{hold}");

        rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
        assert!(rig.driver.core.connecting().is_none(), "{hold}: the newer choice wins at once");
        held.go.send(()).unwrap();
        rig.settle();

        assert_eq!(rig.driver.core.bound(), None, "{hold}");
        assert_eq!(rig.bound_in_app(), None, "{hold}");
        assert_eq!(rig.state(), NowState::OnThisPc, "{hold}");
        assert_eq!(fake_actions(&held.fake), expected, "{hold}");
        assert_eq!(windows_default(&rig), SPEAKERS, "{hold}");
    }
}

/// Cancel (the connect's own button) stops it the same way.
#[test]
fn cancel_stops_a_connect_and_the_sound_stays_here() {
    let held = held_renderer("SetAVTransportURI");
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &held.fake);
    rig.play(Surface::Panel, &id).unwrap();
    held.reached.recv_timeout(WAIT).unwrap();
    rig.send(Surface::Panel, Command::CancelConnect).unwrap();
    held.go.send(()).unwrap();
    rig.settle();
    assert_eq!(rig.bound_in_app(), None);
    assert_eq!(fake_actions(&held.fake), ["Stop", "SetAVTransportURI", "Stop"]);
    assert!(rig.windows.switches().is_empty());
}

/// A pick of a second speaker while the first connects: the first is
/// stopped, the second plays.
#[test]
fn the_latest_pick_wins_over_one_still_connecting() {
    let held = held_renderer("Play");
    let other = plain_renderer();
    let mut rig = Rig::on(serving_app());
    let first = add_speaker(&rig.app, &held.fake);
    let second = add_speaker(&rig.app, &other);
    rig.play(Surface::Panel, &first).unwrap();
    held.reached.recv_timeout(WAIT).unwrap();
    rig.play(Surface::Panel, &second).unwrap();
    held.go.send(()).unwrap();
    rig.plays_on(&second);
    rig.settle();
    assert_eq!(rig.bound_in_app().as_deref(), Some(second.as_str()));
    assert_eq!(fake_actions(&held.fake), ["Stop", "SetAVTransportURI", "Play", "Stop"]);
    rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
    rig.settle();
}

fn fake_actions(fake: &FakeRenderer) -> Vec<String> {
    fake.soap_actions()
}

// ---- someone else's speaker ------------------------------------------

#[test]
fn a_speaker_taken_by_someone_else_is_left_alone() {
    let fake = plain_renderer();
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &fake);
    rig.play(Surface::Panel, &id).unwrap();
    rig.plays_on(&id);
    rig.settle();
    let stops = |fake: &FakeRenderer| fake.soap_actions().iter().filter(|a| *a == "Stop").count();
    // The connect begins with one.
    assert_eq!(stops(&fake), 1);

    // Its transport now plays another source.
    rig.app.note_transport(crate::gena::TransportChange {
        state: Some("PLAYING".into()),
        uri: Some("x-sonos-spotify:track".into()),
    });
    rig.driver.look();
    rig.settle();
    assert_eq!(rig.driver.core.bound(), None);
    assert_eq!(rig.bound_in_app(), None);
    // Not told to stop: it plays someone else's music now.
    assert_eq!(stops(&fake), 1, "{:?}", fake.soap_actions());
    assert!(matches!(rig.state(), NowState::Taken { .. }), "{:?}", rig.state());
    assert_eq!(windows_default(&rig), SPEAKERS);
}

#[test]
fn a_speaker_refilling_its_buffer_is_not_taken() {
    let fake = plain_renderer();
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &fake);
    rig.play(Surface::Panel, &id).unwrap();
    rig.plays_on(&id);
    rig.settle();
    rig.app.note_transport(crate::gena::TransportChange { state: Some("TRANSITIONING".into()), uri: None });
    rig.driver.look();
    assert_eq!(rig.bound_in_app().as_deref(), Some(id.as_str()));
    assert_eq!(rig.driver.core.bound(), Some(&Place::Speaker(SpeakerId::new(&id))));
    rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
    rig.settle();
}

// ---- a receiver announcing another speaker's identity -----------------

const KITCHEN: &str = "airplay:B8E93735AC11";
const KITCHEN_2: &str = "airplay:B8E93735AC11/b8e93735ac11@kitchen 2._raop._tcp.local.";

/// Kitchen, and a newcomer claiming its MAC that answers at `port` on
/// loopback.
fn conflict_on_loopback(port: u16) -> Arc<AirPlayDiscoveryState> {
    use mdns_sd::ServiceInfo;
    let lan = crate::discovery_net::LanTargets::from_networks([("192.168.1.10".parse().unwrap(), 24)]);
    let d = AirPlayDiscoveryState::with_lan(lan.with_fakes_on_loopback());
    let raop = crate::airplay::discovery::RAOP_SERVICE;
    let txt = &[("et", "0,1"), ("cn", "0,1")][..];
    d.ingest_for_tests(raop, &ServiceInfo::new(raop, "B8E93735AC11@Kitchen", "k.local.", "127.0.0.1", 9, txt).unwrap());
    d.ingest_for_tests(
        raop,
        &ServiceInfo::new(raop, "B8E93735AC11@Kitchen 2", "e.local.", "127.0.0.1", port, txt).unwrap(),
    );
    Arc::new(d)
}

/// Only a person picking in the app's own window may play to a receiver
/// that announces another speaker's identity. Every other surface is
/// refused in the words the app has always used, and nothing reaches the
/// receiver.
#[test]
fn a_receiver_claiming_another_speakers_identity_plays_only_from_the_window() {
    use crate::airplay::fake_raop::{Answer, FakeRaop};
    let rx = FakeRaop::start(|_| Answer::Normal);
    let discovery = conflict_on_loopback(rx.port());
    let app = App::with_settings(
        AppConfig { advertise_ip: None, ..App::test_config() },
        UserConfig::in_memory(),
        None,
        Some(discovery.clone()),
    );
    assert!(discovery.find_by_id(KITCHEN_2).unwrap().is_newcomer());
    assert!(!discovery.find_by_id(KITCHEN).unwrap().is_newcomer());
    let mut rig = Rig::on(app.clone());

    let newcomer = SpeakerId::new(KITCHEN_2);
    // The words the app has always used, with the name the list shows.
    let words = format!(
        "To play to “{}”, pick it in the Stream To Speaker window.",
        app.speaker_name(KITCHEN_2).unwrap()
    );
    for from in [
        Surface::Tray,
        Surface::Shortcut,
        Surface::Notification,
        Surface::Phone,
        Surface::CommandLine,
        Surface::Api,
    ] {
        for cmd in [Command::Play(Place::Speaker(newcomer.clone())), Command::PlayTakingOver(newcomer.clone())] {
            let refused = rig.send(from, cmd.clone());
            assert_eq!(refused, Err(Refusal::OnlyFromWindow { speaker: newcomer.clone() }), "{from:?} {cmd:?}");
            let outcome = Outcome { result: refused, snapshot: rig.port.snapshot() };
            assert_eq!(
                crate::model::delay::cli_reply(crate::model::text::Lang::English, &outcome).text,
                words,
                "{from:?}"
            );
            assert!(rig.driver.core.connecting().is_none(), "{from:?} {cmd:?}");
        }
    }

    // Today's entry points, with the engine running, refuse before
    // anything is sent to it.
    let (sink_tx, sink_rx) = unbounded::<(Surface, Command)>();
    app.set_command_sink(Box::new(move |from, cmd| {
        let _ = sink_tx.send((from, cmd));
        ticket().1
    }));
    assert_eq!(app.select_speaker(KITCHEN_2).unwrap_err(), words);
    assert_eq!(app.select_speaker_from(Surface::CommandLine, KITCHEN_2).unwrap_err(), words);
    assert_eq!(app.select_speaker_from_remote(KITCHEN_2).unwrap_err(), words);
    // Enable, with it the speaker used last.
    app.user_config.lock().unwrap().last_speaker_id = Some(KITCHEN_2.to_string());
    assert_eq!(app.set_streaming_enabled(true).unwrap_err(), words);
    assert!(sink_rx.try_recv().is_err(), "nothing reached the engine");

    rig.settle();
    assert!(rx.requests().is_empty(), "the receiver heard from us: {:?}", rx.lines());
    assert_eq!(rig.bound_in_app(), None);

    // A pick in the window plays it.
    rig.play(Surface::Panel, KITCHEN_2).unwrap();
    rig.plays_on(KITCHEN_2);
    assert_eq!(rig.bound_in_app().as_deref(), Some(KITCHEN_2));
    assert!(!rx.requests().is_empty());
    rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
    rig.settle();
}

/// An app whose discovery lists Kitchen and a receiver claiming its
/// identity, answering on `rx`.
fn newcomer_app(rx: &crate::airplay::fake_raop::FakeRaop) -> Arc<App> {
    let discovery = conflict_on_loopback(rx.port());
    assert!(discovery.find_by_id(KITCHEN_2).unwrap().is_newcomer());
    App::with_settings(AppConfig { advertise_ip: None, ..App::test_config() }, UserConfig::in_memory(), None, Some(discovery))
}

/// Run what the core decides at `at` (time the test moves on); how many
/// connects it started by itself.
fn tick_at(rig: &mut Rig, at: Instant) -> usize {
    let w = rig.driver.world(at);
    let dos = rig.driver.core.tick(&w, at);
    let n = by_itself(&dos);
    rig.driver.execute(dos);
    n
}

fn by_itself(dos: &[Do]) -> usize {
    dos.iter().filter(|d| matches!(d, Do::Connect(j) if j.origin == Origin::ByItself)).count()
}

/// The app never plays to a receiver announcing another speaker's
/// identity by itself: not at start-up, not when Windows chooses the app's
/// output, not as a retry after it dropped. Nothing reaches it.
#[test]
fn a_receiver_claiming_another_speakers_identity_is_never_played_by_the_app_itself() {
    use crate::airplay::fake_raop::{Answer, FakeRaop};
    let rx = FakeRaop::start(|_| Answer::Normal);
    let newcomer = Place::Speaker(SpeakerId::new(KITCHEN_2));

    // Start-up, with it remembered.
    let mut rig = Rig::on(newcomer_app(&rx));
    let now = Instant::now();
    rig.driver.core = Core::new(Some(newcomer.clone()), Some(newcomer.clone()), now);
    let w = rig.driver.world(now);
    let dos = rig.driver.core.event(Event::Started { after_update: false }, &w, now);
    rig.driver.execute(dos);
    assert_eq!(tick_at(&mut rig, now + Duration::from_secs(1)), 1, "the start rule returns to it");
    rig.settle();
    assert_eq!(rig.bound_in_app(), None);
    assert!(rx.requests().is_empty(), "start-up: {:?}", rx.lines());

    // Windows chooses the app's output, which is named after it.
    let mut rig = Rig::on(newcomer_app(&rx));
    rig.driver.core.named = Some(newcomer.clone());
    rig.windows.choose_in_windows(OURS);
    let now = Instant::now();
    let w = rig.driver.world(now);
    let dos = rig.driver.core.event(Event::DefaultChanged, &w, now);
    assert_eq!(by_itself(&dos), 1, "Windows is a second door: {dos:?}");
    rig.driver.execute(dos);
    rig.settle();
    assert_eq!(rig.bound_in_app(), None);
    assert!(rx.requests().is_empty(), "Windows' choice: {:?}", rx.lines());

    // Picked in the window, it plays; when it drops, the app doesn't try
    // it again by itself.
    let mut rig = Rig::on(newcomer_app(&rx));
    rig.play(Surface::Panel, KITCHEN_2).unwrap();
    rig.plays_on(KITCHEN_2);
    rig.settle();
    rig.app.release_session(true).map(|t| t.join());
    let heard = rx.requests().len();
    let now = Instant::now();
    let w = rig.driver.world(now);
    let dos = rig.driver.core.event(Event::Dropped, &w, now);
    rig.driver.execute(dos);
    let mut tries = 0;
    for secs in [1, 3, 6, 10, 15, 30] {
        tries += tick_at(&mut rig, now + Duration::from_secs(secs));
        rig.settle();
    }
    assert!(tries >= 1, "no retry was decided");
    assert_eq!(rig.bound_in_app(), None);
    assert_eq!(rx.requests().len(), heard, "a retry reached it: {:?}", rx.lines());
    // And it was never remembered as the speaker to return to.
    assert_eq!(rig.app.user_config.lock().unwrap().last_speaker_id, None);
}

/// The window's own pick reaches the engine as the window's.
#[test]
fn todays_entry_points_send_commands_with_their_own_surface() {
    let fake = plain_renderer();
    let app = serving_app();
    let id = add_speaker(&app, &fake);
    let (sink_tx, sink_rx) = unbounded::<(Surface, Command)>();
    app.set_command_sink(Box::new(move |from, cmd| {
        let _ = sink_tx.send((from, cmd));
        ticket().1
    }));
    let play = Command::Play(Place::Speaker(SpeakerId::new(&id)));
    app.select_speaker_async(&id);
    assert_eq!(sink_rx.try_recv().unwrap(), (Surface::Panel, play.clone()));
    // (Nothing answers here, so these return at once.)
    let _ = app.select_speaker(&id);
    assert_eq!(sink_rx.try_recv().unwrap(), (Surface::Api, play.clone()));
    let _ = app.select_speaker_from_remote(&id);
    assert_eq!(sink_rx.try_recv().unwrap(), (Surface::Api, play.clone()));
    let _ = app.select_speaker_from(Surface::CommandLine, &id);
    assert_eq!(sink_rx.try_recv().unwrap(), (Surface::CommandLine, play.clone()));
    app.set_streaming_enabled(false).unwrap();
    assert_eq!(sink_rx.try_recv().unwrap(), (Surface::Tray, Command::Play(Place::ThisPc)));
    // An automatic connect is the engine's own decision: nothing is sent.
    app.connect_in_background(&id);
    assert!(sink_rx.try_recv().is_err());
    // Nothing was sent to the speaker by any of them.
    assert!(fake.requests().is_empty(), "{:?}", fake.soap_actions());
}

/// The web API's select answers once the speaker plays, or with the words
/// saying why it doesn't, as it did before the engine; the connect itself
/// runs on the engine's thread.
#[test]
fn the_web_apis_select_answers_when_the_connect_has_ended() {
    let plays = plain_renderer();
    let refuses = FakeRenderer::start(|req| match req.soap_action() {
        Some("SetAVTransportURI") => Reply::soap_fault(714, "Illegal MIME-type"),
        _ => answer(req),
    });
    let app = serving_app();
    let good = add_speaker(&app, &plays);
    let bad = add_speaker(&app, &refuses);
    let windows = FakePlatform::new();
    let engine = Engine::start(app.clone(), windows.platform(), false);

    assert_eq!(app.select_speaker_from_remote(&good), Ok(()));
    assert_eq!(app.session.lock().unwrap().as_ref().map(|s| s.stable_id()).as_deref(), Some(good.as_str()));
    // Again: it plays there already.
    assert_eq!(app.select_speaker_from_remote(&good), Ok(()));

    let err = app.select_speaker_from_remote(&bad).unwrap_err();
    assert!(err.contains("didn't take the stream"), "{err}");

    engine.send(Surface::Panel, Command::Play(Place::ThisPc)).wait(WAIT).unwrap();
    engine.stop();
}

// ---- quitting, sleep ---------------------------------------------------

#[test]
fn quitting_gives_windows_sound_back_and_lets_the_speaker_go() {
    let fake = plain_renderer();
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &fake);
    rig.play(Surface::Panel, &id).unwrap();
    rig.plays_on(&id);
    rig.settle();
    assert_eq!(windows_default(&rig), OURS);
    rig.send(Surface::Panel, Command::Quit).unwrap();
    rig.settle();
    assert_eq!(windows_default(&rig), SPEAKERS);
    assert_eq!(rig.bound_in_app(), None);
    assert_eq!(fake.soap_actions().last().map(String::as_str), Some("Stop"));
}

#[test]
fn sleep_gives_the_sound_back_and_keeps_the_choice() {
    let fake = plain_renderer();
    let mut rig = Rig::on(serving_app());
    let id = add_speaker(&rig.app, &fake);
    rig.play(Surface::Panel, &id).unwrap();
    rig.plays_on(&id);
    rig.settle();
    rig.driver.input(Input::System(SystemEvent::Suspending), &rig.port);
    rig.settle();
    assert_eq!(windows_default(&rig), SPEAKERS);
    assert_eq!(rig.bound_in_app(), None);
    assert_eq!(rig.driver.core.choice, Some(Place::Speaker(SpeakerId::new(&id))));
}

// ---- no thread left behind ---------------------------------------------

/// How many threads this process has once those that are ending have
/// ended (they are given up to `WAIT`).
fn threads_settled_to(at_most: usize) -> usize {
    let end = Instant::now() + WAIT;
    loop {
        let n = crate::engine::thread_count();
        if n <= at_most || Instant::now() >= end {
            return n;
        }
        std::thread::yield_now();
    }
}

fn thread_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("/proc/self/task")
        .map(|d| {
            d.filter_map(|e| std::fs::read_to_string(e.ok()?.path().join("comm")).ok())
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Run in a process of its own by [`picks_leave_no_thread_behind`]: the
/// count is the whole process's.
#[test]
#[ignore = "run in a process of its own by picks_leave_no_thread_behind"]
fn picks_thread_probe() {
    use crate::airplay::fake_raop::{Answer, FakeRaop};
    let a = plain_renderer();
    let b = plain_renderer();
    let raop = FakeRaop::start(|_| Answer::Normal);
    let discovery = conflict_on_loopback(raop.port());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let app = App::with_settings(
        AppConfig { advertise_ip: None, bind: SocketAddr::from(([127, 0, 0, 1], port)), ..App::test_config() },
        UserConfig::in_memory(),
        None,
        Some(discovery),
    );
    crate::http_server::start_http_server(crate::http_server::HttpServerConfig::minimal(app.config.bind, app.hub.clone()))
        .unwrap();
    let mut rig = Rig::on(app);
    let a = add_speaker(&rig.app, &a);
    let b = add_speaker(&rig.app, &b);
    let round = |rig: &mut Rig| {
        for id in [a.as_str(), b.as_str(), KITCHEN_2] {
            rig.play(Surface::Panel, id).unwrap();
            rig.plays_on(id);
        }
        rig.send(Surface::Panel, Command::Play(Place::ThisPc)).unwrap();
        rig.settle();
    };
    // Once first: what lasts (the servers' own threads) has started.
    round(&mut rig);
    let before = crate::engine::thread_count();
    let names_before = thread_names();
    for _ in 0..5 {
        round(&mut rig);
    }
    let after = threads_settled_to(before);
    assert!(after <= before, "before: {names_before:?}\nafter: {:?}", thread_names());
}

#[test]
fn picks_leave_no_thread_behind() {
    if crate::engine::thread_count() == 0 {
        return; // No way to count threads here.
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "engine::driver::tests::picks_thread_probe", "--test-threads=1"])
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("1 passed"), "the probe didn't run, or failed:\n{text}");
}
