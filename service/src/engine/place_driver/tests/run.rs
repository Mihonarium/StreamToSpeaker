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
    let app = App::for_tests();
    let r = Running::on(app.clone());
    r.wait_view(|_| true);
    assert_eq!(r.windows.names(), ["Stream To Speaker"]);
    assert_eq!(
        r.windows.handback(),
        Some(crate::platform::HandbackPlan {
            our_output: Some(output(OURS)),
            playback: Some(output(SPEAKERS)), calls: Some(output(SPEAKERS)), name: "Stream To Speaker (not running)".into()
        })
    );
    assert_eq!(r.windows.preparations(), 1);
    r.windows.set_our_output_present(false);
    r.wait_view(|v| v.device_name.is_none());
    assert_eq!(r.windows.handback().unwrap().our_output, None);
    assert_eq!(r.windows.names(), ["Stream To Speaker"]);
    r.windows.set_our_output_present(true);
    r.wait_view(|v| v.device_name.is_some());
    assert_eq!(
        r.windows.names(),
        ["Stream To Speaker", "Stream To Speaker"]
    );
    assert_eq!(r.windows.preparations(), 3);
    r.command(Command::SwitchOutput(output(HEADPHONES)))
        .0
        .unwrap();
    assert_eq!(
        r.windows.handback(),
        Some(crate::platform::HandbackPlan {
            our_output: Some(output(OURS)),
            playback: Some(output(HEADPHONES)), calls: Some(output(HEADPHONES)), name: "Stream To Speaker (not running)".into()
        })
    );
    r.command(Command::ShowPcOutputs(true)).0.unwrap();
    assert_eq!(r.windows.preparations(), 4);
    app.user_config.lock().unwrap().only_some_apps = true;
    r.wait_view(|v| v.scene.situation.only_some_apps);
    let plan = r.windows.handback().unwrap();
    assert_eq!(plan.playback, Some(output(HEADPHONES)));
    assert_eq!(plan.calls, Some(output(HEADPHONES)));
    use crate::platform::{Handback, Outputs};
    r.windows.set_default(&output(OURS), &[Role::Communications]).unwrap();
    r.windows.hand_back_now();
    assert_eq!(r.windows.calls_output(), Some(output(HEADPHONES)));
    r.finish(true);
}

#[test]
fn the_level_meter_shows_only_sound_that_reaches_the_place() {
    use crate::engine::place_driver::run::sends_to_place;
    use crate::model::samples;
    let kitchen = samples::kitchen();
    let playing = samples::named("kitchen-playing").situation;
    assert!(sends_to_place(&playing, Some(&kitchen)));
    // Connected to another speaker than the place, or to none.
    assert!(!sends_to_place(&playing, Some(&id())));
    assert!(!sends_to_place(&playing, None));
    // Still connecting to it.
    assert!(!sends_to_place(&samples::named("kitchen-connecting").situation, Some(&kitchen)));
    // Music Assistant is the place and no server is connected: nothing of
    // this PC's sound is heard, so the meter stays still.
    let ma = SpeakerId::music_assistant();
    let mut sit = samples::named("music-assistant-not-connected").situation;
    assert!(sit.standing.contains(&Standing::MusicAssistantNotConnected));
    assert!(!sends_to_place(&sit, Some(&ma)));
    // Its server connected and hasn't started this PC's sound.
    sit.standing = vec![Standing::MusicAssistantNotStarted];
    assert!(!sends_to_place(&sit, Some(&ma)));
    // It started the sound.
    sit.standing.clear();
    assert!(sends_to_place(&sit, Some(&ma)));
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
fn an_app_on_another_output_is_named_as_windows_names_it() {
    use crate::model::situation::{Situation, Standing};
    let fake = FakeRenderer::start(answer);
    let mut r = Rig::new(&fake, UserConfig::in_memory());
    r.playing();
    r.windows.app_plays(r"C:\Apps\Browser.exe", SPEAKERS, Some(0.6));
    for _ in 0..20 {
        r.advance(250);
    }
    let named = |s: &Situation| {
        s.standing.iter().find_map(|s| match s {
            Standing::AppElsewhere { app, .. } => Some(app.clone()),
            _ => None,
        })
    };
    // The rules know the app by the name of its program's file; what
    // people read is the name Windows shows for it.
    let key = named(&r.driver.situation()).expect("the rules see the app on the other output");
    let shown = named(&r.driver.scene().situation).expect("the scene names it");
    assert_eq!(key, crate::platform::app_key(r"C:\Apps\Browser.exe"));
    assert_eq!(shown, r.windows.platform().sessions.sessions().iter().find(|s| s.key == key).unwrap().app);
    assert_ne!(shown, key);
    r.send(Command::StayOnThisPc);
    r.settle();
}

#[test]
fn window_commands_send_the_requested_page_to_the_opener() {
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

#[test]
fn handback_uses_playback_when_the_call_device_leaves() {
    use crate::platform::{Handback, Outputs};
    let app = App::for_tests();
    let r = Running::on(app.clone());
    r.wait_view(|_| true);
    r.windows.set_default(&output(OURS), &[Role::Communications]).unwrap();
    app.user_config.lock().unwrap().calls_back = Some(output(HEADPHONES));
    r.command(Command::ShowPcOutputs(true)).0.unwrap();
    let prepared = r.windows.handback().unwrap();
    assert_eq!(prepared.calls, Some(output(HEADPHONES)));
    r.windows.set_output_on(HEADPHONES, false);
    r.command(Command::ShowPcOutputs(false)).0.unwrap();
    assert_eq!(r.windows.handback().unwrap().calls, Some(output(SPEAKERS)));
    r.finish(true);
    // Unplugging after preparation leaves the old headset in the plan.
    let windows = FakePlatform::new();
    windows.set_output_on(HEADPHONES, false);
    windows.set_default(&output(OURS), &[Role::Communications]).unwrap();
    windows.prepare(&prepared);
    windows.refuse_switches(true);
    windows.hand_back_now();
    assert_eq!(windows.calls_output(), Some(output(OURS)));
    windows.refuse_switches(false);
    windows.hand_back_now();
    assert_eq!(windows.calls_output(), Some(output(SPEAKERS)));
}
