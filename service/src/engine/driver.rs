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
            if let Some(ours) = platform.outputs.our_output() {
                let outputs = platform.outputs.clone();
                let levels = crate::latest::Latest::new("stream-to-speaker-windows-level", move |(level, muted): &(u32, bool)| {
                    outputs.set_volume(&ours, *level as u8, *muted).is_ok()
                });
                app.set_windows_level_through(move |level, muted| {
                    levels.want((level, muted));
                });
            }
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

    /// Quitting: give Windows' output back and let the speaker go, then
    /// name the app's output for when it isn't running. Waits at most a
    /// second for the engine.
    pub fn quit(&self) {
        self.quit_from(Surface::Panel);
    }

    /// Send the single quit command with the origin of the shutdown request.
    pub fn quit_from(&self, from: Surface) {
        let ticket = self.send(from, Command::Quit);
        let _ = ticket.wait(Duration::from_secs(1));
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

pub(super) fn busy(app: &App, id: &str) -> Option<Option<String>> {
    if app.discovery.as_ref().is_some_and(|d| d.find_by_id(id).is_some()) {
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
