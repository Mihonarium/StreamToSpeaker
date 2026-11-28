//! The serial loop for the new surfaces.

use super::*;
use crate::model::situation::{Doing, Standing};
use crate::model::view::device_name_not_running;
use crate::model::view::{view, View};

/// Where the loop sends what it makes.
pub(crate) struct Outlet {
    /// Makes a view current for every surface and returns it as published.
    pub publish: Box<dyn Fn(View) -> Arc<View> + Send>,
    /// The level meter, from zero to one.
    pub level: Box<dyn Fn(f32) + Send>,
}

/// Called once a command has been carried out and its effect published.
pub(crate) type Answer = Box<dyn FnOnce(Result<Effect, Refusal>, Arc<View>) + Send>;

impl PlaceDriver {
    /// Runs until Stop arrives or the last caller drops its sender.
    pub(crate) fn run(mut self, rx: Receiver<PlaceInput>, start: StartKind, out: Outlet) {
        self.start(start);
        let mut current = self.publish(&out, None);
        loop {
            // Only callers own senders to rx. Windows and the driver's
            // workers can report events without keeping the loop alive.
            let received = crossbeam_channel::select_biased! {
                recv(self.events) -> msg => Some(msg.unwrap_or(PlaceInput::Stop)),
                recv(rx) -> msg => Some(msg.unwrap_or(PlaceInput::Stop)),
                default(self.next_wake()) => None,
            };
            if matches!(received, Some(PlaceInput::Stop)) {
                break;
            }
            let answer = received.and_then(|i| self.input(i));
            self.look();
            current = self.publish(&out, Some(current));
            if let Some((reply, result)) = answer {
                reply(result, current.clone());
            }
            for (reply, result) in self.held_answers.drain(..) {
                reply(result, current.clone());
            }
        }
        self.input(PlaceInput::Stop);
        for t in self.threads {
            let _ = t.join();
        }
    }

    pub(super) fn prepare_handback(&mut self, pc: Option<PcOutputId>, ours: Option<PcOutputId>, lang: crate::model::text::Lang) {
        let saved = self.app.user_config.lock().unwrap().calls_back.clone();
        let calls = saved.filter(|id| self.platform.outputs.list().iter().any(|o| o.id == *id));
        let plan = crate::platform::HandbackPlan {
            our_output: ours,
            calls: calls.or_else(|| pc.clone()),
            playback: pc,
            name: device_name_not_running(lang),
        };
        if self.handback.as_ref() != Some(&plan) {
            self.platform.handback.prepare(&plan);
            self.handback = Some(plan);
        }
    }

    fn publish(&mut self, out: &Outlet, previous: Option<Arc<View>>) -> Arc<View> {
        let (scene, ours) = self.scene_and_output();
        let sending = sends_to_place(&scene.situation, self.core.connected_speaker());
        let level = ours
            .as_ref()
            .filter(|_| sending)
            .map_or(0.0, |id| self.platform.sessions.peak(id).clamp(0.05, 1.0));
        let version = previous.as_ref().map_or(0, |v| v.version);
        let mut next = view(scene, self.core.named(), ours.is_none(), version);
        self.prepare_handback(next.scene.situation.pc_output.clone(), ours, next.lang);
        let current = match previous {
            Some(old) if *old == next => old,
            old => {
                if let Some(name) = &next.device_name {
                    if old.as_ref().and_then(|v| v.device_name.as_ref()) != Some(name) {
                        if let Err(e) = self.platform.device.rename(name) {
                            debug!("couldn't name the app's output: {e}");
                        }
                    }
                }
                next.version += 1;
                (out.publish)(next)
            }
        };
        (out.level)(level);
        current
    }
}

/// Whether this PC's sound reaches the speaker that is the place: what the
/// level meter shows. It doesn't while Music Assistant is the place and its
/// server isn't connected or hasn't started this PC's sound.
pub(super) fn sends_to_place(sit: &crate::model::situation::Situation, connected: Option<&SpeakerId>) -> bool {
    sit.doing == Doing::Playing
        && sit.place_speaker().is_some_and(|id| connected == Some(id))
        && !sit
            .standing
            .iter()
            .any(|s| matches!(s, Standing::MusicAssistantNotStarted | Standing::MusicAssistantNotConnected))
}

/// Starts the place driver on its own thread and subscribes it to Windows.
pub(crate) fn spawn(
    app: Arc<App>,
    platform: Platform,
    opener: Arc<Mutex<Option<Opener>>>,
    start: StartKind,
    out: Outlet,
) -> std::io::Result<(Sender<PlaceInput>, JoinHandle<()>)> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let notices = platform.events.clone();
    let driver = PlaceDriver::new(app, platform, opener, Instant::now());
    let events = driver.tx.clone();
    let thread = std::thread::Builder::new()
        .name("stream-to-speaker-place".into())
        .spawn(move || driver.run(rx, start, out))?;
    notices.subscribe(Arc::new(move |e| {
        let _ = events.send(PlaceInput::System(e));
    }));
    Ok((tx, thread))
}
