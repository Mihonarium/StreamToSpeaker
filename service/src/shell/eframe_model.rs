//! For tests: how eframe 0.29 keeps a window in step with the builder the
//! shell gives it every frame, with egui's own `ViewportBuilder::patch`.
//!
//! eframe keeps the builder a window was made with. On every frame of an
//! immediate viewport it patches that copy with the builder egui holds for
//! the viewport (the one passed to `show_viewport_immediate` that frame;
//! for the root, egui's default), and queues the commands the patch returns:
//! once as the frame starts (`render_immediate_viewport`), and once more in
//! each of the two `handle_viewport_output` calls after it, the child's and
//! the root's, which run what is queued. The commands the shell sent in the
//! frame run last, at the end of the root's pass (egui hands a child's
//! commands to the outermost viewport). See eframe's `glow_integration.rs`
//! (`initialize_or_update_viewport`, `render_immediate_viewport`,
//! `handle_viewport_output`) and egui's `Context::end_pass`.

use egui::{ViewportBuilder, ViewportCommand};

/// One window as eframe drives it.
pub struct Window {
    /// eframe's copy of the builder.
    kept: ViewportBuilder,
    /// Whether the window is on screen.
    pub visible: bool,
    /// Every command that reached the window, by frame.
    pub frames: Vec<Vec<ViewportCommand>>,
    /// Model a library whose `patch` compares `visible` with the kept
    /// `visible` (egui 0.29.1 compares it with `active`).
    right_comparison: bool,
}

impl Window {
    /// The window as made from `builder` on its first frame.
    pub fn made(builder: ViewportBuilder, right_comparison: bool) -> Self {
        let visible = builder.visible.unwrap_or(true);
        Window { kept: builder, visible, frames: Vec::new(), right_comparison }
    }

    fn patch(&mut self, new: &ViewportBuilder) -> Vec<ViewportCommand> {
        let before = self.kept.visible;
        let (mut commands, recreate) = self.kept.patch(new.clone());
        assert!(!recreate, "the builder changed in a way that makes eframe make the window again: {new:?}");
        if self.right_comparison {
            commands.retain(|c| !matches!(c, ViewportCommand::Visible(_)));
            if let Some(v) = new.visible.filter(|v| Some(*v) != before) {
                self.kept.visible = Some(v);
                commands.push(ViewportCommand::Visible(v));
            }
        }
        commands
    }

    fn run(&mut self, commands: Vec<ViewportCommand>, ran: &mut Vec<ViewportCommand>) {
        for c in commands {
            if let ViewportCommand::Visible(v) = c {
                self.visible = v;
            }
            ran.push(c);
        }
    }

    /// One frame of an immediate viewport: `held` is the builder egui holds
    /// for it (for a window the shell shows, what the shell passed this
    /// frame), `sent` what the shell sent the window in the frame. `first`:
    /// the frame that made it, which starts with no patch.
    pub fn frame(&mut self, held: &ViewportBuilder, sent: Vec<ViewportCommand>, first: bool) {
        let mut ran = Vec::new();
        let mut queued = if first { Vec::new() } else { self.patch(held) };
        queued.extend(self.patch(held));
        self.run(queued, &mut ran);
        let mut queued = self.patch(held);
        queued.extend(sent);
        self.run(queued, &mut ran);
        self.frames.push(ran);
    }
}
