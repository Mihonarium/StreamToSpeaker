//! A registration belongs to one set of bindings for this run of the engine.
use super::{PlaceDriver, PlaceInput, Surface};
use crate::model::{panel, situation::{Doing, Standing}};
use crate::platform::{DelayKeys, DelayPress, Hotkeys, KeyRegistration, KeyStatus};
use crossbeam_channel::Sender;
use std::sync::Arc;

pub(crate) struct DelayShortcuts {
    hotkeys: Arc<dyn Hotkeys>,
    tx: Sender<PlaceInput>,
    active: Option<(DelayKeys, KeyRegistration)>,
    generation: u64,
    stopped: bool,
    recording: bool,
}

impl DelayShortcuts {
    pub(super) fn new(hotkeys: Arc<dyn Hotkeys>, tx: Sender<PlaceInput>) -> Self {
        Self { hotkeys, tx, active: None, generation: 0, stopped: false, recording: false }
    }

    pub(crate) fn status(&self) -> [KeyStatus; 3] {
        self.active.as_ref().map_or([KeyStatus::Registered; 3], |(_, r)| r.rows())
    }

    pub(crate) fn accepts(&self, keys: &DelayKeys) -> bool {
        if !self.recording || self.stopped { return false; }
        let probe = self.hotkeys.register(keys, Arc::new(|_| {}));
        DelayPress::ALL.iter().zip(probe.status).all(|(press, status)| {
            keys.0[usize::from(press.add)].is_empty() || status == KeyStatus::Registered
        })
    }

    pub(crate) fn record(&mut self, recording: bool, cfg: &crate::user_config::UserConfig) {
        self.recording = recording;
        self.sync(cfg);
    }

    fn sync(&mut self, cfg: &crate::user_config::UserConfig) {
        let wanted = (cfg.delay_shortcuts && !self.recording && !self.stopped).then_some(&cfg.delay_keys);
        if self.active.as_ref().map(|(keys, _)| keys) == wanted {
            return;
        }
        self.active = None;
        self.generation += 1;
        let generation = self.generation;
        self.active = wanted.map(|keys| {
            let tx = self.tx.clone();
            let reg = self.hotkeys.register(
                keys,
                Arc::new(move |press| {
                    let _ = tx.send(PlaceInput::Shortcut { generation, press });
                }),
            );
            (keys.clone(), reg)
        });
    }
}

impl PlaceDriver {
    pub(super) fn stop_shortcuts(&mut self) {
        let mut shortcuts = self.shortcuts.lock().unwrap();
        shortcuts.stopped = true;
        shortcuts.active = None;
    }

    pub(super) fn sync_shortcuts(&self) {
        let cfg = self.app.user_config.lock().unwrap();
        self.shortcuts.lock().unwrap().sync(&cfg);
    }

    pub(super) fn press_shortcut(&mut self, generation: u64, press: DelayPress) {
        self.sync_shortcuts();
        let shortcuts = self.shortcuts.lock().unwrap();
        if shortcuts.generation != generation || shortcuts.active.is_none() {
            return;
        }
        drop(shortcuts);
        let (scene, ours) = self.scene_and_output();
        let Some(id) = scene.situation.place_speaker() else {
            return;
        };
        if scene.situation.doing != Doing::Playing
            || scene.situation.standing.iter().any(|s| matches!(s, Standing::PausedAtSpeaker { .. }))
            || self.core.connected_speaker() != Some(id)
            || self.session_of(id).is_none()
        {
            return;
        }
        if !ours.is_some_and(|id| {
            self.platform.sessions.peak(&id) > super::place::numbers::LEVEL_THRESHOLD
        }) {
            return;
        }
        let panel::About::Delay(controls) = panel::delay_about(&scene, id) else {
            return;
        };
        let row = &controls.rows[usize::from(press.add)];
        if row.enabled {
            let button = if press.large { &row.large } else { &row.small };
            let _ = self.command(Surface::Shortcut, &button.command);
        }
    }
}
