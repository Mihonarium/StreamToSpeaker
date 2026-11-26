//! Wheel scrolling for one immediate viewport.

use egui::{vec2, Context, Event, MouseWheelUnit, Vec2};

#[derive(Clone, Copy, Default)]
pub(super) struct ScrollState {
    remaining: Vec2,
    last_fractional_line: Option<f64>,
    frame_time: Option<f64>,
    delta: Vec2,
}

impl ScrollState {
    /// Set the pass's scroll before the widgets read it. Like a click, the
    /// frame's scroll goes to its first pass: a later pass of the same frame
    /// gets none, since the scroll areas kept what the first pass did.
    pub(super) fn apply(&mut self, ctx: &Context) {
        let line_speed = ctx.options(|o| o.line_scroll_speed);
        ctx.input_mut(|i| {
            *self = self.frame(
                &i.raw.events,
                i.time,
                i.stable_dt,
                line_speed,
                i.screen_rect().height(),
            );
            i.smooth_scroll_delta = self.delta;
        });
        if self.remaining != Vec2::ZERO {
            ctx.request_repaint();
        }
    }

    fn frame(
        mut self,
        events: &[Event],
        time: f64,
        stable_dt: f32,
        line_speed: f32,
        screen_height: f32,
    ) -> Self {
        if self.frame_time == Some(time) {
            self.delta = Vec2::ZERO;
            return self;
        }
        self.frame_time = Some(time);
        self.delta = Vec2::ZERO;
        for event in events {
            let Event::MouseWheel {
                unit,
                delta,
                modifiers,
            } = event
            else {
                continue;
            };
            if modifiers.ctrl || modifiers.mac_cmd || modifiers.command {
                continue;
            }
            let mut points = match unit {
                MouseWheelUnit::Point => *delta,
                MouseWheelUnit::Line => *delta * line_speed,
                MouseWheelUnit::Page => *delta * screen_height,
            };
            if modifiers.shift {
                points = vec2(points.x + points.y, 0.0);
            }
            let touchpad = match unit {
                MouseWheelUnit::Point => points.length() < 8.0,
                MouseWheelUnit::Line => {
                    if delta.x.fract() != 0.0 || delta.y.fract() != 0.0 {
                        self.last_fractional_line = Some(time);
                    }
                    // A touchpad fling can pass through whole numbers.
                    self.last_fractional_line
                        .is_some_and(|last| time - last < 0.300)
                }
                MouseWheelUnit::Page => false,
            };
            if touchpad {
                self.delta += points;
            } else {
                self.remaining += points;
            }
        }

        if self.remaining != Vec2::ZERO {
            let factor = egui::emath::exponential_smooth_factor(0.90, 0.1, stable_dt.min(0.1));
            for axis in 0..2 {
                let left = self.remaining[axis];
                let applied = if left.abs() < 1.0 {
                    left
                } else {
                    left * factor
                };
                self.delta[axis] += applied;
                self.remaining[axis] -= applied;
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, RawInput};
    use MouseWheelUnit::{Line, Page, Point};

    fn wheel(unit: MouseWheelUnit, delta: Vec2, modifiers: Modifiers) -> Event {
        Event::MouseWheel {
            unit,
            delta,
            modifiers,
        }
    }

    fn line(y: f32) -> Event {
        wheel(Line, vec2(0.0, y), Modifiers::NONE)
    }

    fn frame(state: ScrollState, events: &[Event], time: f64) -> ScrollState {
        state.frame(events, time, 1.0 / 60.0, 40.0, 600.0)
    }

    #[test]
    fn a_notch_drains_over_several_frames_and_adds_up_to_one_line() {
        let mut state = frame(ScrollState::default(), &[line(-1.0)], 0.0);
        let mut total = state.delta;
        for n in 1..60 {
            assert!(state.delta.y < 0.0 && state.delta.y > -40.0);
            state = frame(state, &[], n as f64 / 60.0);
            total += state.delta;
            if state.remaining == Vec2::ZERO {
                assert!(n > 1);
                break;
            }
        }
        assert_eq!(state.remaining, Vec2::ZERO);
        assert!((total.y + 40.0).abs() < 0.0001);
        assert_eq!(frame(state, &[], 1.0).delta, Vec2::ZERO);
    }

    #[test]
    fn fractional_lines_and_the_whole_lines_in_their_fling_pass_in_full() {
        let event = wheel(Line, vec2(-1.5, 0.25), Modifiers::NONE);
        let fractional = frame(ScrollState::default(), &[event], 0.0);
        assert_eq!(fractional.delta, vec2(-60.0, 10.0));
        let whole = frame(fractional, &[line(1.0)], 0.299);
        assert_eq!(whole.delta, vec2(0.0, 40.0));
        assert_eq!(whole.remaining, Vec2::ZERO);
        let notch = frame(whole, &[line(1.0)], 0.301);
        assert!(notch.delta.y > 0.0 && notch.delta.y < 40.0);
    }

    #[test]
    fn short_point_deltas_pass_alongside_the_smoothed_wheel() {
        let events = [
            wheel(Page, vec2(0.5, -1.0), Modifiers::SHIFT),
            wheel(Point, vec2(0.0, 8.0), Modifiers::NONE),
            wheel(Point, vec2(2.0, 3.0), Modifiers::SHIFT),
        ];
        let state = ScrollState::default().frame(&events, 0.0, 0.5, 40.0, 600.0);
        let factor = egui::emath::exponential_smooth_factor(0.90, 0.1, 0.1);
        assert_eq!(state.delta, vec2(5.0 - 300.0 * factor, 8.0 * factor));
        assert_eq!(
            state.remaining,
            vec2(-300.0, 8.0) - vec2(-300.0, 8.0) * factor
        );
    }

    #[test]
    fn zoom_wheels_leave_scrolling_alone() {
        let events = [Modifiers::CTRL, Modifiers::MAC_CMD, Modifiers::COMMAND]
            .map(|modifiers| wheel(Line, vec2(0.0, 0.5), modifiers));
        let state = frame(ScrollState::default(), &events, 0.0);
        assert_eq!(state.delta, Vec2::ZERO);
        assert_eq!(state.remaining, Vec2::ZERO);
        assert_eq!(state.last_fractional_line, None);
    }

    #[test]
    fn a_frame_scrolls_in_its_first_pass_only() {
        let ctx = Context::default();
        let mut state = ScrollState::default();
        let input = RawInput {
            time: Some(1.0),
            events: vec![line(1.0)],
            ..Default::default()
        };
        let mut passes = Vec::new();
        let _ = ctx.run(input, |ctx| {
            state.apply(ctx);
            passes.push((ctx.input(|i| i.smooth_scroll_delta), state.remaining));
            if passes.len() == 1 {
                ctx.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                ctx.request_discard("run the layout again");
            }
        });
        assert_eq!(passes.len(), 2);
        assert!(passes[0].0.y > 0.0 && passes[0].1.y > 0.0);
        assert_eq!(passes[1], (Vec2::ZERO, passes[0].1));
    }
}
