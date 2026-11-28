//! Connect the place driver's publication and command queue to the surface port.
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use crossbeam_channel::Sender;
use log::debug;
use super::place::StartKind;
use super::place_driver::{run, PlaceInput};
use super::signals::SpeakerSignal;
use crate::app::App;
use crate::model::command::{ticket, Command, Open, Outcome, Refusal, Surface, Ticket};
use crate::model::port::{Port, PortCore, Watch, Watcher};
use crate::model::text::LocalTime;
use crate::model::Snapshot;
use crate::platform::Platform;

/// Windows' slider for the app's output is set through Windows' endpoint
/// volume (on the PC where the engine knows the app's output) rather than
/// the driver's push. `false` is the fallback if Windows' slider doesn't
/// follow it.
pub const LEVEL_THROUGH_WINDOWS: bool = true;
pub(super) const QUIT_MOVE_WAIT: Duration = Duration::from_secs(2);
/// A UPnP speaker that stopped fetching the stream for this long has
/// dropped out (its own reconnects take far less).
pub(super) const UPNP_GONE_AFTER: Duration = Duration::from_secs(4);
/// A UPnP speaker that took Play but hasn't fetched the stream this long
/// after isn't taking the sound.
pub(super) const UPNP_NOT_TAKING_AFTER: Duration = Duration::from_secs(10);
/// Where navigation commands (open a window or a page) go.
pub(crate) type SurfaceOpener = Box<dyn Fn(Surface, Open) + Send + Sync>;
pub type Opener = Box<dyn Fn(Open) + Send + Sync>;

/// The real engine: implements the model's [`Port`].
pub struct Engine {
    port: Arc<PortCore>,
    tx: Sender<PlaceInput>,
    platform: Platform,
    opener: Arc<Mutex<Option<SurfaceOpener>>>,
}


impl Engine {
    pub fn start(app: Arc<App>, platform: Platform) -> anyhow::Result<Arc<Engine>> {
        let start = {
            let mut cfg = app.user_config.lock().unwrap();
            let updated = std::mem::take(&mut cfg.closed_for_update);
            if updated || cfg.last_version_seen.is_none() {
                cfg.last_version_seen.get_or_insert_with(|| crate::display_version().to_string());
                cfg.save();
            }
            if updated { StartKind::AfterUpdate } else { StartKind::Normal }
        };
        if LEVEL_THROUGH_WINDOWS {
            let outputs = platform.outputs.clone();
            let levels = crate::latest::Latest::new("stream-to-speaker-windows-level", move |(id, level, muted): &(crate::model::PcOutputId, u32, bool)| {
                outputs.set_volume(id, *level as u8, *muted).is_ok()
            });
            let outputs = platform.outputs.clone();
            app.try_windows_level_through(move |level, muted| {
                let Some(id) = outputs.our_output() else { return false };
                levels.want((id, level, muted));
                true
            });
        }

        let opener = Arc::new(Mutex::new(None));
        // The first published scene is also the first one a caller can read.
        let core = Arc::new(OnceLock::<Arc<PortCore>>::new());
        let levels = core.clone();
        let (ready, first) = crossbeam_channel::bounded(1);
        let outlet = run::Outlet {
            publish: Box::new(move |v| {
                if let Some(port) = core.get() {
                    return port.publish(v);
                }
                let port = core.get_or_init(|| Arc::new(PortCore::new(v)));
                let _ = ready.send(port.clone());
                port.snapshot()
            }),
            level: Box::new(move |value| {
                if let Some(port) = levels.get() { port.set_level(value); }
            }),
        };
        let (tx, _thread) = run::spawn(app.clone(), platform.clone(), opener.clone(), start, outlet)?;
        let port = first.recv()?;
        let _ = app.engine_port.set(Arc::downgrade(&port));
        let sink = tx.clone();
        app.set_command_sink(Box::new(move |from, cmd| enqueue(&sink, from, cmd)));
        Ok(Arc::new(Engine { port, tx, platform, opener }))
    }

    /// Where navigation commands go (the window that opens Settings, a
    /// dialog or one of Windows' pages).
    pub fn on_open(&self, opener: Opener) {
        *self.opener.lock().unwrap() = Some(Box::new(move |_, open| opener(open)));
    }

    #[cfg(windows)]
    pub(crate) fn on_open_from(&self, opener: SurfaceOpener) {
        *self.opener.lock().unwrap() = Some(opener);
    }

    /// Wait up to two seconds for the engine to stop the speaker and give
    /// Windows its outputs back. If the engine hasn't answered, return the
    /// outputs here, then give the app's output its name for when it is closed.
    pub fn quit(&self) {
        self.quit_from(Surface::Panel);
    }

    /// Send the single quit command with the origin of the shutdown request.
    pub fn quit_from(&self, from: Surface) {
        let ticket = self.send(from, Command::Quit);
        if ticket.wait(QUIT_MOVE_WAIT).is_none() {
            self.platform.handback.hand_back_now();
        }
        let lang = self.port.snapshot().lang;
        let _ = self.platform.device.rename(&crate::model::view::device_name_not_running(lang));
        self.platform.device.settle(Duration::from_secs(1));
    }
}

fn enqueue(tx: &Sender<PlaceInput>, from: Surface, command: Command) -> Ticket {
    let (reply, ticket) = ticket();
    let _ = tx.send(PlaceInput::Command(from, command, Some(Box::new(move |result, snapshot| {
        reply.send(Outcome { result, snapshot });
    }))));
    ticket
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(PlaceInput::Stop);
    }
}

impl Port for Engine {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.port.snapshot()
    }

    fn watch(&self, watcher: Watcher) -> Watch {
        self.port.watch(watcher)
    }

    fn send(&self, from: Surface, command: Command) -> Ticket {
        enqueue(&self.tx, from, command)
    }

    fn level(&self) -> f32 {
        self.port.level()
    }
}
/// A DACP listener for one receiver and its announcement.
pub(super) struct Buttons {
    id: String,
    _listener: crate::airplay::dacp::Listener,
    announced: Option<(mdns_sd::ServiceDaemon, String)>,
}

impl Drop for Buttons {
    fn drop(&mut self) {
        if let Some((daemon, name)) = self.announced.take() {
            crate::airplay::dacp::unannounce(&daemon, &name);
            let _ = daemon.shutdown();
        }
    }
}

impl Buttons {
    /// Listen for the bound AirPlay receiver's own buttons (off until
    /// AirPlay speakers have been tried with it).
    pub(super) fn follow(app: &Arc<App>, buttons: &mut Option<Buttons>) {
        if !crate::airplay::dacp::LISTEN_FOR_SPEAKER_BUTTONS {
            return;
        }
        let ids = match app.session.lock().unwrap().as_ref() {
            Some(crate::app::ActiveSession::AirPlay(s)) => Some((s.renderer.stable_id(), s.remote_ids())),
            _ => None,
        };
        let Some((id, (dacp_id, active_remote))) = ids else {
            *buttons = None;
            return;
        };
        if buttons.as_ref().is_some_and(|b| b.id == id) {
            return;
        }
        let app = app.clone();
        let speaker = id.clone();
        let heard = Arc::new(move |signal| app.signals.push(&speaker, signal));
        let listener = match crate::airplay::dacp::Listener::start(
            std::net::Ipv4Addr::UNSPECIFIED.into(),
            active_remote,
            heard,
        ) {
            Ok(l) => l,
            Err(e) => {
                debug!("DACP: can't listen: {}", e);
                return;
            }
        };
        let announced = mdns_sd::ServiceDaemon::new().ok().and_then(|daemon| {
            let host = format!("{}.local.", crate::PRODUCT_NAME.replace(' ', "-"));
            match crate::airplay::dacp::announce(&daemon, &dacp_id, &host, listener.port) {
                Ok(name) => Some((daemon, name)),
                Err(e) => {
                    debug!("DACP: can't announce: {}", e);
                    None
                }
            }
        });
        *buttons = Some(Buttons { id, _listener: listener, announced });
    }
}
pub(super) fn upnp_info(s: &crate::app::ActiveSession) -> Option<String> {
    match s {
        crate::app::ActiveSession::Upnp(r) => {
            let uri = &r.stream_uri;
            let rest = uri.strip_prefix("http://")?;
            let host_end = rest.find('/')?;
            Some(format!("http://{}/", &rest[..host_end]))
        }
        _ => None,
    }
}

pub(super) fn refusal(why: crate::delay::Unchangeable, speaker: &crate::model::SpeakerId) -> Refusal {
    match why {
        crate::delay::Unchangeable::NoSpeaker => Refusal::NoDelayHere,
        crate::delay::Unchangeable::MusicAssistant => Refusal::MusicAssistantDelay,
        crate::delay::Unchangeable::KeptBySpeakerOnSteady => Refusal::FixedOnSteady { speaker: speaker.clone() },
    }
}

/// Whether speaker `id` plays something else now, where it shows that
/// (`Some`), with the name of the other PC that plays there when its
/// title says so.
pub(super) fn busy(app: &App, id: &str) -> Option<Option<String>> {
    if app.says_what_it_plays(id) {
        return match app.look_at_speaker(id) {
            SpeakerSignal::PlayingElse { who } => Some(who),
            _ => None,
        };
    }
    let r = app.find_airplay_renderer(id)?;
    app.another_device_sends_to(&r).then_some(None)
}
/// The time on the PC's clock.
pub(crate) fn local_time() -> LocalTime {
    #[cfg(windows)]
    {
        // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given.
        let st = unsafe {
            let mut st = std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>();
            windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
            st
        };
        LocalTime::new(st.wHour as u8, st.wMinute as u8)
    }
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        LocalTime::new(((secs / 3600) % 24) as u8, ((secs / 60) % 60) as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{platform::fake::FakePlatform, user_config::UserConfig};

    #[test]
    fn quit_returns_the_roles_when_the_driver_never_reads_the_ticket() {
        use crate::model::{samples, view::view};
        use crate::platform::{fake::{OURS, SPEAKERS}, Handback, HandbackPlan, Outputs, PcOutputId};
        let windows = FakePlatform::new();
        windows.choose_in_windows(OURS);
        let pc = Some(PcOutputId::new(SPEAKERS));
        windows.prepare(&HandbackPlan { our_output: Some(PcOutputId::new(OURS)), playback: pc.clone(),
            calls: pc.clone(), name: "Stream To Speaker (not running)".into() });
        let (tx, unread) = crossbeam_channel::unbounded();
        let engine = Engine { tx, platform: windows.platform(), opener: Default::default(),
            port: Arc::new(PortCore::new(view(samples::scene(), None, false, 1))) };
        engine.quit_from(Surface::Settings);
        assert!(matches!(unread.try_recv(), Ok(PlaceInput::Command(Surface::Settings, Command::Quit, Some(_)))));
        assert_eq!(windows.handbacks(), 1);
        assert_eq!(windows.default_output(), pc);
        assert_eq!(windows.calls_output(), pc);
    }

    #[test]
    fn a_speaker_added_by_its_address_that_plays_something_else_is_busy() {
        use crate::fake_renderer::{renderer_description, FakeRenderer, Reply, AV_TRANSPORT};
        use crate::manual_speakers::{ManualKind, ManualSpeaker};
        let fake = FakeRenderer::start(|req| match req.soap_action() {
            Some("GetTransportInfo") => Reply::soap_ok(AV_TRANSPORT, "GetTransportInfo", &[("CurrentTransportState", "PLAYING")]),
            Some("GetMediaInfo") => Reply::soap_ok(AV_TRANSPORT, "GetMediaInfo", &[("CurrentURI", "x-sonos-spotify:track")]),
            Some(action) => Reply::soap_ok(AV_TRANSPORT, action, &[]),
            None => Reply::ok(&renderer_description("Den Amp", &[])),
        });
        let app = App::for_tests();
        let m = ManualSpeaker { kind: ManualKind::Upnp, name: String::new(), host: fake.url("/description.xml"), port: 0 };
        app.user_config.lock().unwrap().manual_speakers.push(m.clone());
        assert_eq!(busy(&app, &m.id()), Some(None));
        assert_eq!(fake.soap_actions(), ["GetTransportInfo", "GetMediaInfo"]);
    }

    #[test]
    fn speaker_volume_reaches_an_output_that_was_missing_at_start() {
        use crate::platform::fake::OURS;
        let windows = FakePlatform::new();
        windows.set_our_output_present(false);
        let app = App::for_tests();
        let engine = Engine::start(app.clone(), windows.platform()).unwrap();
        windows.set_our_output_present(true);
        app.speaker_reported_level(Some(43), Some(true));
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while windows.level_of(OURS) != Some((43, true)) {
            assert!(std::time::Instant::now() < until, "the restored endpoint did not receive the volume and mute");
            std::thread::sleep(Duration::from_millis(2));
        }
        engine.quit();
    }

    #[test]
    fn the_port_answers_with_the_published_view() {
        let mut saved = UserConfig::in_memory();
        saved.last_speaker_id = Some("kitchen".into());
        let app = App::with_settings(App::test_config(), saved, None, None);
        let engine = Engine::start(app.clone(), FakePlatform::new().platform()).unwrap();
        let first = engine.snapshot();
        assert!(first.version > 0 && !first.scene.outputs_open);
        assert_eq!(first.scene.situation.place_speaker().unwrap().as_str(), "kitchen");
        {
            let saved = app.user_config.lock().unwrap();
            assert_eq!(saved.last_speaker_id.as_deref(), Some("kitchen"));
            let kept = saved.sound_place.as_ref().unwrap();
            assert_eq!(kept.place.as_ref().unwrap().as_str(), "kitchen");
            assert!(!kept.last_outputs.is_empty());
        }
        let answer = engine.send(Surface::Panel, Command::ShowPcOutputs(true)).wait(Duration::from_secs(2)).unwrap();
        assert_eq!(answer.result, Ok(crate::model::Effect::Done));
        assert!(answer.snapshot.version > first.version && answer.snapshot.scene.outputs_open);
        assert!(app.engine_view().unwrap().scene.outputs_open);
        assert!(engine.snapshot().scene.outputs_open);
        engine.quit();
    }
}
