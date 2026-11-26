//! A sample-backed port for rendering the shell and exercising its controls.
use super::{
    command::{ticket, Outcome},
    samples,
    scene::Scene,
    view::view,
    Command, Effect, Port, PortCore, Snapshot, Surface, Ticket, Watch, Watcher,
};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Mutex};

pub struct ScriptedEngine {
    port: PortCore,
    commands: Mutex<Vec<(Surface, Command)>>,
    close_explained: AtomicBool,
    popup_seq: AtomicU64,
    close_popup_at: Mutex<Option<std::time::Instant>>,
}

impl ScriptedEngine {
    pub fn new(sample: &str) -> Self {
        let scene = samples::named(sample);
        Self {
            popup_seq: AtomicU64::new(scene.situation.popups.popup.as_ref().map_or(0, |p| p.seq)),
            port: PortCore::new(view(scene, None, false, 1)),
            commands: Mutex::default(),
            close_explained: false.into(),
            close_popup_at: Mutex::default(),
        }
    }

    pub fn publish(&self, scene: Scene) {
        if let Some(p) = &scene.situation.popups.popup { self.popup_seq.fetch_max(p.seq, Ordering::Relaxed); }
        self.port
            .publish(view(scene, None, false, self.port.next_version()));
    }

    /// Advance the one timed popup raised by a scripted shell action.
    pub fn tick(&self) {
        let mut at = self.close_popup_at.lock().unwrap();
        if at.is_some_and(|until| std::time::Instant::now() >= until) {
            *at = None;
            drop(at);
            let mut scene = self.snapshot().scene.clone();
            if scene.situation.popups.popup.as_ref().is_some_and(|p| p.what == super::situation::Popup::WindowClosed) {
                scene.situation.popups.popup = None;
                self.publish(scene);
            }
        }
    }

    fn first_run(&self, command: &Command) {
        use super::{command::FirstRunAction as Act, situation::{Confirm, FirstRunStep as Step, RowState, Spot}};
        let mut scene = self.snapshot().scene.clone();
        let sit = &mut scene.situation;
        match (command, sit.first_run.clone()) {
            (Command::FirstRunAction(Act::ShowAgain), _) => {
                sit.first_run = Some(Step::One);
                sit.confirm_in_panel = None;
            }
            (Command::FirstRunAction(Act::Chime(id)), Some(Step::One)) => {
                sit.confirm_in_panel = None;
                if let Some(row) = sit.speaker(id).filter(|r| !r.state.grey()) {
                    if !row.can_chime || matches!(row.state, RowState::Plain | RowState::MayBeInUse) {
                        sit.first_run = Some(Step::Two { speaker: id.clone(), level: row.can_chime.then_some(25), limit: 100 });
                    } else if row.state == RowState::PlayingElse {
                        sit.confirm_in_panel = Some(Confirm::ChimeBusy { speaker: id.clone(), who: None });
                    }
                }
            }
            (Command::Confirm { yes }, Some(Step::One)) => {
                if let Some(Confirm::ChimeBusy { speaker, .. }) = sit.confirm_in_panel.take() {
                    if *yes {
                        if let Some(row) = sit.speakers.iter_mut().find(|s| s.id == speaker) { row.state = RowState::Plain; }
                        sit.first_run = Some(Step::Two { speaker, level: Some(25), limit: 100 });
                    }
                }
            }
            (Command::FirstRunAction(Act::ChimeVolume(v)), Some(Step::Two { speaker, limit, level: Some(_) })) => {
                sit.first_run = Some(Step::Two { speaker, level: Some((*v).min(limit)), limit });
            }
            (Command::FirstRunAction(Act::Heard | Act::UseWithoutChecking), Some(Step::Two { speaker, level, .. })) => {
                for row in &mut sit.speakers { row.is_place = row.id == speaker; }
                sit.spot = Spot::Speaker(speaker.clone());
                scene.volume.place = level.map(super::facts::Level::at);
                sit.first_run = Some(Step::Three { speaker });
            }
            (Command::FirstRunAction(Act::ChooseAnother), Some(Step::Two { .. })) => sit.first_run = Some(Step::One),
            (Command::FirstRunAction(Act::StartAtSignIn(on)), _) => scene.start_at_sign_in = *on,
            (Command::FirstRunAction(Act::Close | Act::Done), _) => {
                sit.first_run = None;
                sit.confirm_in_panel = None;
            }
            _ => return,
        }
        self.publish(scene);
    }

    pub fn received(&self) -> Vec<(Surface, Command)> {
        self.commands.lock().unwrap().clone()
    }
}

impl Port for ScriptedEngine {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.port.snapshot()
    }
    fn watch(&self, watcher: Watcher) -> Watch {
        self.port.watch(watcher)
    }
    fn send(&self, from: Surface, command: Command) -> Ticket {
        use super::situation::{Popup, Shown};
        match (&command, from) {
            (Command::WindowClosed, Surface::Panel) if !self.close_explained.swap(true, Ordering::Relaxed) => {
                let mut scene = self.snapshot().scene.clone();
                let seq = self.popup_seq.fetch_add(1, Ordering::Relaxed) + 1;
                scene.situation.popups.popup = Some(Shown { what: Popup::WindowClosed, seq, since_ms: 0 });
                *self.close_popup_at.lock().unwrap() = Some(std::time::Instant::now() + std::time::Duration::from_millis(crate::engine::place::numbers::POPUP_WITH_BUTTON_MS));
                self.publish(scene);
            }
            (Command::PanelShown(true), Surface::Panel) | (Command::ClosePopup, Surface::Notification) => {
                let mut scene = self.snapshot().scene.clone();
                scene.situation.popups.popup = None;
                self.publish(scene);
            }
            (Command::FirstRunAction(_) | Command::Confirm { .. }, Surface::Panel | Surface::Settings) => self.first_run(&command),
            _ => {}
        }
        self.commands.lock().unwrap().push((from, command));
        let (reply, ticket) = ticket();
        reply.send(Outcome {
            result: Ok(Effect::Done),
            snapshot: self.snapshot(),
        });
        ticket
    }
    fn level(&self) -> f32 {
        self.port.level()
    }
}
