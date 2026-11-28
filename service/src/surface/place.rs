use super::{
    grid::*,
    links::link,
    panel::{Drawn, PanelUi},
};
use crate::model::{
    panel::{PlacePart, Sign},
    Command,
};
use crate::ui::{button::IconButton, icon::Icon, inside, theme::Theme, volume::Slider};
use egui::{vec2, Rect, Ui};
pub(super) fn draw(
    ui: &mut Ui,
    r: Rect,
    p: &PlacePart,
    state: &mut PanelUi,
    t: &Theme,
    out: &mut Drawn,
) {
    let origin = r.min;
    let output = p.output.as_ref().map(|s| format!(" · {s}"));
    let reserved = output
        .as_ref()
        .map_or(0.0, |s| width(ui, s, t.font(BODY)).min(OUTPUT_MIN));
    let n = one(
        ui,
        job(&p.name, t.strong(TITLE), t.c.text),
        RIGHT - LEFT - reserved,
    );
    let name_box = rect(origin, LEFT, TITLE_TOP, RIGHT - LEFT, TITLE_LINE);
    if let Some(s) = output {
        let x = LEFT + n.size().x.ceil();
        let g = one(ui, job(&s, t.font(BODY), t.c.text_secondary), RIGHT - x);
        let baseline = |g: &egui::Galley| g.rows[0].glyphs.first().map_or(0.0, |ch| ch.pos.y);
        let top = (name_box.center().y - n.size().y / 2.0).round() + baseline(&n) - baseline(&g);
        paint(
            ui,
            rect(origin, x, top - origin.y, RIGHT - x, g.size().y),
            g,
        );
    }
    paint(ui, name_box, n);
    let center = origin + vec2(ICON, STATE_TOP + BODY_LINE / 2.0);
    match p.state.sign {
        Sign::Spinner => spinner(ui, center, t.c.accent),
        Sign::SpinnerWarn => spinner(ui, center, t.c.amber),
        Sign::Warn => mark(ui, crate::model::panel::Mark::Warn, center, MARK, t),
        s => {
            let live = s == Sign::Bars;
            if ui.is_rect_visible(square(center, GLYPH)) {
                let time = if live { animate(ui) as f32 } else { 0.0 };
                let mut heights = [(0.67, 0.17), (0.91, 0.0), (0.79, 0.43), (1.07, 0.2)].map(
                    |(period, phase)| {
                        if live {
                            (GAP + INSET * ((time / period + phase) * std::f32::consts::TAU).sin())
                                .round()
                        } else {
                            BAR
                        }
                    },
                );
                if heights.is_sorted() || heights.windows(2).all(|w| w[0] >= w[1]) {
                    heights.swap(1, 2);
                }
                for (i, h) in heights.into_iter().enumerate() {
                    let bar = rect(
                        center,
                        -RADIO / 2.0 + i as f32 * (METER_BAR + NEAR),
                        TIGHT - h,
                        METER_BAR,
                        h,
                    );
                    ui.painter().rect_filled(
                        ui.painter().round_rect_to_pixels(bar),
                        RULE,
                        if live {
                            t.c.live
                        } else {
                            t.c.text_secondary.gamma_multiply(0.45)
                        },
                    );
                }
            }
        }
    }
    let w = p
        .state
        .link
        .as_ref()
        .map_or(0.0, |a| width(ui, &a.label, t.font(BODY)) + GAP);
    let cy = center.y - origin.y;
    let used = label(
        ui,
        line(origin, TEXT, RIGHT - w, cy, BODY_LINE),
        &p.state.text,
        t.font(BODY),
        t.c.text_secondary,
    );
    volume(ui, origin, p, state, t, out);
    if let Some(a) = &p.state.link {
        let x = TEXT + used + GAP;
        link(
            ui,
            line(origin, x, x + w - GAP, cy, LINK_HIT),
            0.0,
            a,
            BODY,
            t,
            out,
        );
    }
}

fn volume(
    ui: &mut Ui,
    origin: egui::Pos2,
    p: &PlacePart,
    state: &mut PanelUi,
    t: &Theme,
    out: &mut Drawn,
) {
    let Some(v) = &p.volume else {
        state.pending = None;
        return;
    };
    let enabled = !v.dimmed && v.owner.is_some();
    let cy = VOLUME_TOP + HIT / 2.0;
    let mute = square(origin + vec2(ICON, cy), HIT);
    let resp = inside(ui, mute, |u| {
        if !enabled {
            u.disable();
        }
        IconButton::new(
            if v.muted { Icon::Mute } else { Icon::Volume },
            &v.mute_spoken,
        )
        .size(HIT, SMALL_LINE)
        .enabled(enabled)
        .show(u)
    });
    let cmd = v.owner.clone().map(|owner| Command::SetMuted {
        owner,
        muted: !v.muted,
    });
    out.record(ui, &resp, &v.mute_spoken, enabled, cmd.as_ref());
    let track = line(origin, TEXT, END, cy, BODY_LINE);
    state.pending = state.pending.take().filter(|(id, value)| {
        enabled
            && v.owner.as_ref() == Some(id)
            && *value != v.level
            && *value <= v.limit.unwrap_or(100)
    });
    let level = state.pending.as_ref().map_or(v.level, |p| p.1);
    let mut slider = Slider::new(level.into(), 0.0..=100.0, &v.spoken)
        .faint(v.muted || !enabled)
        .flyout();
    if let Some(limit) = v.limit {
        slider = slider.tick(limit.into()).max_allowed(limit.into());
    }
    let set = inside(ui, track, |u| {
        if !enabled {
            u.disable();
        }
        u.push_id(&v.owner, |ui| slider.show(ui)).inner
    });
    if let (Some(owner), Some(level)) = (&v.owner, set.filter(|_| enabled)) {
        let level = level as u8;
        state.pending = Some((owner.clone(), level));
        out.commands.push(Command::SetVolume {
            owner: owner.clone(),
            level,
        });
    }
    out.controls.push(super::panel::Control {
        rect: track,
        name: v.spoken.clone(),
        command: None,
        enabled,
    });
    let number = state.pending.as_ref().map(|(_, value)| value.to_string());
    let number = number.as_deref().unwrap_or(&v.number);
    let w = width(ui, number, t.font(BODY)).min(RIGHT - END);
    label(
        ui,
        line(origin, RIGHT - w, RIGHT, cy, BODY_LINE),
        number,
        t.font(BODY),
        if enabled { t.c.text } else { t.c.text_disabled },
    );
}
