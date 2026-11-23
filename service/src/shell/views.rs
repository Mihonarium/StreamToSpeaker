//! The rule for reopening the panel on the view it closed on.

/// What the panel remembers of the views it closed on, and when it opens
/// on them again: within a minute of closing, or whenever one of them
/// holds text that wasn't used yet.
pub struct ViewMemory<T> {
    closed: Option<(Vec<T>, f64)>,
}

impl<T> Default for ViewMemory<T> {
    fn default() -> Self {
        ViewMemory { closed: None }
    }
}

/// How long after closing the panel reopens on the same view, in seconds.
pub const REOPEN_WITHIN: f64 = 60.0;

impl<T> ViewMemory<T> {
    /// The panel closed at `now` showing `views` (bottom first).
    pub fn closed(&mut self, views: Vec<T>, now: f64) {
        self.closed = (!views.is_empty()).then_some((views, now));
    }

    /// The panel opens at `now` on its everyday content: the views to show
    /// again, if any. `unused_text` says whether a view holds text that
    /// wasn't used yet.
    pub fn reopen(&mut self, now: f64, unused_text: impl Fn(&T) -> bool) -> Vec<T> {
        match self.closed.take() {
            Some((views, at)) if now - at < REOPEN_WITHIN || views.iter().any(&unused_text) => views,
            _ => Vec::new(),
        }
    }

    /// The panel opens on a view someone asked for: what was remembered
    /// goes.
    pub fn forget(&mut self) {
        self.closed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_reopens_on_its_view_within_a_minute_or_while_a_field_holds_text() {
        let mut m: ViewMemory<(&str, bool)> = ViewMemory::default();
        m.closed(vec![("settings", false), ("about", false)], 100.0);
        assert_eq!(m.reopen(150.0, |v| v.1), vec![("settings", false), ("about", false)]);
        // Reopened once: the next opening is the everyday view unless it
        // closed on a view again.
        assert!(m.reopen(151.0, |v| v.1).is_empty());
        m.closed(vec![("missing", false)], 100.0);
        assert!(m.reopen(160.0 + REOPEN_WITHIN, |v| v.1).is_empty(), "after a minute: the everyday view");
        m.closed(vec![("missing", true)], 100.0);
        assert_eq!(m.reopen(100.0 + 3600.0, |v| v.1), vec![("missing", true)], "an address typed and not added yet");
        m.closed(vec![("settings", false)], 100.0);
        m.forget();
        assert!(m.reopen(101.0, |v| v.1).is_empty());
        // Closed on the everyday view: nothing to reopen.
        m.closed(Vec::new(), 100.0);
        assert!(m.reopen(101.0, |v| v.1).is_empty());
    }
}
