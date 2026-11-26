//! Pop-ups by the clock: at most one, and one confirmation, none while the
//! panel is open. When the panel opens, the pop-up goes (the panel says the
//! same) and a confirmation moves under its speaker's row.

use super::*;

impl Core {
    /// Shows `popup` by the clock, unless the panel is open or the Settings
    /// switch turns such pop-ups off. A newer one replaces an older one.
    pub(super) fn raise_popup(&mut self, popup: Popup, w: &World, now: Clock) {
        if self.panel_open || (popup.governed_by_switch() && !w.settings.notes) {
            return;
        }
        let lasts = if popup.has_button() { POPUP_WITH_BUTTON_MS } else { POPUP_MS };
        let seq = self.next_seq();
        self.popup = Some((Shown { seq, what: popup, since_ms: now.mono_ms }, now.mono_ms + lasts));
    }

    /// The note goes (a switch, its ×, its speaker removed), and with it a
    /// pop-up that shows it.
    pub(super) fn clear_note(&mut self) {
        self.note = None;
        if matches!(&self.popup, Some((Shown { what: Popup::Note(_), .. }, _))) {
            self.popup = None;
        }
    }

    pub(super) fn keep_no_speaker_popup(&mut self) {
        if self.popup.as_ref().is_some_and(|(p, _)| p.what == Popup::NoSpeakerChosen) {
            self.popup = None;
            self.no_speaker_chosen = true;
        }
    }

    pub(super) fn panel_opened(&mut self) {
        self.keep_no_speaker_popup();
        self.popup = None;
        if let Some(c) = &mut self.confirm {
            c.by_clock = false;
        }
        if let Some((_, shown)) = &mut self.pick_failed {
            *shown = true;
        }
    }

    pub(super) fn panel_closed(&mut self) {
        self.no_speaker_chosen = false;
        if self.pick_failed.as_ref().is_some_and(|(p, shown)| *shown && !self.first_run_failure(p)) {
            self.pick_failed = None;
        }
        // A confirmation in the panel goes with it: nothing changed.
        if self.confirm.as_ref().is_some_and(|c| !c.by_clock) {
            self.confirm = None;
        }
    }

    pub(super) fn expire_popups(&mut self, now: Clock) {
        if self.popup.as_ref().is_some_and(|(_, until)| now.mono_ms >= *until) {
            self.popup = None;
        }
    }
}
