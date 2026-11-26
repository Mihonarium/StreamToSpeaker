use super::*;
use crate::engine::place_driver::run::{spawn, Outlet};
use crate::model::command::Open;
use crate::model::view::View;
use crate::platform::fake::MONITOR;

struct Running {
    windows: Arc<FakePlatform>,
    input: Sender<PlaceInput>,
    thread: JoinHandle<()>,
    views: Receiver<Arc<View>>,
    levels: Receiver<f32>,
    current: Arc<Mutex<Option<Arc<View>>>>,
}

impl Running {
    fn on(app: Arc<App>) -> Self {
        let windows = FakePlatform::new();
        let (views_tx, views) = unbounded();
        let (meter, levels) = unbounded();
        let current = Arc::new(Mutex::new(None));
        let published = current.clone();
        let out = Outlet {
            publish: Box::new(move |v| {
                let v = Arc::new(v);
                *published.lock().unwrap() = Some(v.clone());
                let _ = views_tx.send(v.clone());
                v
            }),
            level: Box::new(move |peak| {
                let _ = meter.send(peak);
            }),
        };
        let (input, thread) = spawn(
            app,
            windows.platform(),
            Arc::new(Mutex::new(None)),
            StartKind::Normal,
            out,
        )
        .unwrap();
        Self {
            windows,
            input,
            thread,
            views,
            levels,
            current,
        }
    }

    fn command(&self, cmd: Command) -> (Result<Effect, Refusal>, Arc<View>) {
        let current = self.current.clone();
        let (sent, answer) = bounded(1);
        self.input
            .send(PlaceInput::Command(
                Surface::Panel,
                cmd,
                Some(Box::new(move |result, view| {
                    assert!(Arc::ptr_eq(
                        current.lock().unwrap().as_ref().unwrap(),
                        &view
                    ));
                    sent.send((result, view)).unwrap();
                })),
            ))
            .unwrap();
        answer.recv_timeout(WAIT).unwrap()
    }

    fn wait_view(&self, matches: impl Fn(&View) -> bool) -> Arc<View> {
        let end = Instant::now() + WAIT;
        loop {
            let view = self.views.recv_deadline(end).unwrap();
            if matches(&view) {
                return view;
            }
        }
    }

    fn finish(self, stop: bool) {
        if stop {
            self.input.send(PlaceInput::Stop).unwrap();
        } else {
            drop(self.input);
        }
        let end = Instant::now() + WAIT;
        while !self.thread.is_finished() {
            assert!(Instant::now() < end, "loop didn't stop");
            std::thread::sleep(Duration::from_millis(2));
        }
        self.thread.join().unwrap();
        assert!(self.windows.passing_to().is_none());
    }
}

#[test]
fn only_changed_views_publish_and_commands_answer_with_the_current_arc() {
    let r = Running::on(App::for_tests());
    let v = r.views.recv_timeout(WAIT).unwrap();
    assert_eq!(v.version, 1);
    for _ in 0..4 {
        assert_eq!(r.levels.recv_timeout(WAIT).unwrap(), 0.0);
    }
    assert!(r.views.is_empty());
    r.windows.set_output_on(MONITOR, true);
    let next = r.views.recv_timeout(WAIT).unwrap();
    assert_eq!(next.version, 2);
    assert!(next.scene.outputs.iter().any(|o| o.id == output(MONITOR)));
    let (result, open) = r.command(Command::ShowPcOutputs(true));
    assert_eq!(result, Ok(Effect::Done));
    assert!(open.scene.outputs_open);
    assert_eq!(open.version, 3);
    assert!(Arc::ptr_eq(&open, &r.views.recv_timeout(WAIT).unwrap()));
    assert!(Arc::ptr_eq(
        &open,
        &r.command(Command::ShowPcOutputs(true)).1
    ));
    assert_eq!(
        r.command(Command::Open(Open::SoundSettings)).0,
        Err(Refusal::WindowOnly)
    );
    assert!(r.views.is_empty());
    r.finish(true);
}

#[test]
fn names_and_handback_follow_only_changes_of_their_inputs() {
    let r = Running::on(App::for_tests());
    r.wait_view(|_| true);
    assert_eq!(r.windows.names(), ["Stream To Speaker"]);
    assert_eq!(
        r.windows.handback(),
        Some((
            Some(output(SPEAKERS)),
            "Stream To Speaker (not running)".into()
        ))
    );
    assert_eq!(r.windows.preparations(), 1);
    r.windows.set_our_output_present(false);
    r.wait_view(|v| v.device_name.is_none());
    assert_eq!(r.windows.names(), ["Stream To Speaker"]);
    r.windows.set_our_output_present(true);
    r.wait_view(|v| v.device_name.is_some());
    assert_eq!(
        r.windows.names(),
        ["Stream To Speaker", "Stream To Speaker"]
    );
    assert_eq!(r.windows.preparations(), 1);
    r.command(Command::SwitchOutput(output(HEADPHONES)))
        .0
        .unwrap();
    assert_eq!(
        r.windows.handback(),
        Some((
            Some(output(HEADPHONES)),
            "Stream To Speaker (not running)".into()
        ))
    );
    r.command(Command::ShowPcOutputs(true)).0.unwrap();
    assert_eq!(r.windows.preparations(), 2);
    r.finish(true);
}

#[test]
fn the_level_and_last_played_follow_the_speaker_that_became_the_place() {
    let fake = FakeRenderer::start(answer);
    let host = TestApp::with_speaker(&fake, UserConfig::in_memory());
    let r = Running::on(host.app.clone());
    r.wait_view(|_| true);
    assert_eq!(r.levels.recv_timeout(WAIT).unwrap(), 0.0);
    assert_eq!(host.app.speaker_settings(DEN).last_played, None);
    r.command(Command::Switch {
        place: Place::Speaker(id()),
        way: None,
    })
    .0
    .unwrap();
    let v = r.wait_view(|v| v.scene.situation.place_speaker() == Some(&id()));
    let stamp = host.app.speaker_settings(DEN).last_played.unwrap();
    assert_eq!(v.scene.speakers[0].last_used, Some(stamp));
    assert_eq!(
        r.windows.names(),
        ["Stream To Speaker", "Den (Stream To Speaker)"]
    );
    for (sample, expected) in [(0.7, 0.7), (0.02, 0.05)] {
        r.windows.app_plays("Music", OURS, Some(sample));
        let deadline = Instant::now() + WAIT;
        while r.levels.recv_deadline(deadline).unwrap() != expected {}
    }
    r.command(Command::Quit).0.unwrap();
    assert_eq!(host.app.speaker_settings(DEN).last_played, Some(stamp));
    r.finish(true);
}

#[test]
fn a_second_pick_answers_with_the_connection_it_joined() {
    for from in [Surface::CommandLine, Surface::Api] {
        let (fake, reached, go) = held_renderer("Play");
        let host = TestApp::with_speaker(&fake, UserConfig::in_memory());
        let r = Running::on(host.app.clone());
        let (first, view) = r.command(Command::Switch { place: Place::Speaker(id()), way: None });
        first.unwrap();
        assert_eq!(view.scene.situation.picking.as_ref().unwrap().speaker, id());
        reached.recv_timeout(WAIT).unwrap();

        // Release Play once the second pick has been handled, while its view
        // still shows the first pick connecting.
        let input = r.input.clone();
        host.app.set_command_sink(Box::new(move |from, cmd| {
            let (reply, ticket) = crate::model::command::ticket();
            let go = go.clone();
            input.send(PlaceInput::Command(from, cmd, Some(Box::new(move |result, snapshot| {
                assert_eq!(snapshot.scene.situation.picking.as_ref().unwrap().speaker, id());
                reply.send(crate::model::Outcome { result, snapshot });
                go.send(()).unwrap();
            })))).unwrap();
            ticket
        }));
        assert_eq!(host.app.select_speaker_from(from, DEN), Ok(()));
        assert_eq!(fake.soap_actions().iter().filter(|a| *a == "Play").count(), 1);
        r.wait_view(|v| v.scene.situation.place_speaker() == Some(&id()));
        r.command(Command::Quit).0.unwrap();
        r.finish(true);
    }
}

#[test]
fn dropping_the_last_caller_joins_even_with_windows_subscribed() {
    let r = Running::on(App::for_tests());
    r.wait_view(|_| true);
    let end = Instant::now() + WAIT;
    while r.windows.passing_to().is_none() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    }
    r.finish(false);
}

#[test]
fn levels_set_outside_the_panel_settle_and_song_titles_follow_other_sound() {
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.playing();
    let played = r.app.speaker_settings(DEN).last_played.unwrap();
    r.app.vsync.window_changed(47);
    r.windows.app_plays("Music", OURS, None);
    let track = crate::now_playing::NowPlaying { title: "Song".into(), ..Default::default() };
    let spotify = crate::app::TitleSource {
        app: Some("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify".into()),
        playing: true,
        title: Some(track.clone()),
    };
    let title = || crate::app::title_to_send(std::slice::from_ref(&spotify), &r.app.sounding_apps.lock().unwrap());
    r.windows.app_plays(r"C:\Music\Spotify.exe", OURS, Some(0.5));
    r.windows.app_plays("Browser", SPEAKERS, Some(0.6));
    r.driver.look();
    assert_eq!(title(), Some(track.clone()));
    r.windows.app_plays(r"C:\Music\TIDAL.exe", OURS, Some(0.5));
    r.driver.look();
    assert_eq!(title(), None);
    r.windows.app_plays(r"C:\Music\TIDAL.exe", OURS, None);
    r.windows.app_plays("Browser", OURS, Some(0.4));
    r.driver.look();
    assert_eq!(title(), None);
    r.advance(place::numbers::LEVEL_SAVED_AFTER_MS + 1);
    let saved = r.app.speaker_settings(DEN);
    assert_eq!(saved.level, Some(47));
    assert_eq!(saved.last_played, Some(played));
    assert_eq!(r.driver.core.kept().levels[&id()].level, 47);
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn window_commands_open_the_requested_page() {
    let mut r = Rig::on(App::for_tests());
    let (opened, pages) = unbounded();
    *r.driver.opener.lock().unwrap() = Some(Box::new(move |_, page| opened.send(page).unwrap()));
    for page in [
        Open::Panel,
        Open::Settings,
        Open::SoundSettings,
        Open::Pair(id()),
        Open::SpeakerSettings(id()),
        Open::MusicAssistantHowTo,
    ] {
        r.send(Command::Open(page.clone()));
        assert_eq!(pages.recv_timeout(WAIT).unwrap(), page);
    }
    assert!(pages.is_empty());
}
