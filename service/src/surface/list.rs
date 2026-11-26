use super::{
    grid::*,
    links::{draw_link, link},
    panel::{Drawn, PanelUi},
};
use crate::{
    model::{
        panel::{Act, Fold, ListPart, Mark, OutputRow, PcRow, Row, RowIcon},
        Command,
    },
    ui::{
        icon::Icon,
        look::look_as,
        theme::{focus_ring, Theme},
    },
};
use egui::{vec2, Id, Key, Pos2, Rect, Response, Sense, Ui, WidgetInfo, WidgetType};

pub(super) fn height(list: &ListPart, open: bool) -> f32 {
    let more = list
        .more
        .as_ref()
        .map_or(0, |f| 1 + if open { f.rows.len() } else { 0 });
    2.0 * TIGHT
        + ROW * (1 + list.rows.len() + more) as f32
        + SHORT_ROW * (list.this_pc.outputs.len() + usize::from(list.search.is_some())) as f32
}
fn icon(kind: RowIcon) -> Icon {
    match kind {
        RowIcon::Pc => Icon::Laptop,
        RowIcon::Headphones => Icon::Headphones,
        RowIcon::Display => Icon::Monitor,
        RowIcon::Speaker => Icon::Speaker,
        RowIcon::Group => Icon::Group,
        RowIcon::HomePodPair => Icon::Pair,
        RowIcon::Tv => Icon::Tv,
        RowIcon::MusicAssistant => Icon::Hub,
    }
}
fn arrow(open: bool) -> Icon {
    if open {
        Icon::ChevronUp
    } else {
        Icon::ChevronDown
    }
}

/// The list always has a This PC row. Checked PC outputs aren't places.
#[derive(Clone, Copy)]
enum Entry<'a> {
    Pc(&'a PcRow),
    Output(&'a OutputRow),
    Speaker(&'a Row),
    Fold(&'a Fold),
}

impl Entry<'_> {
    fn id(self, list: Id) -> Id {
        match self {
            Self::Pc(_) => list.with("pc"),
            Self::Fold(_) => list.with("fold"),
            Self::Speaker(r) => list.with(("speaker", &r.place)),
            Self::Output(o) => match &o.command {
                Command::SwitchOutput(id) => list.with(("output", id)),
                _ => list.with(("output", &o.name)),
            },
        }
    }
    fn name(&self) -> &str {
        match self {
            Self::Pc(r) => &r.name,
            Self::Output(r) => &r.name,
            Self::Speaker(r) => &r.name,
            Self::Fold(f) => &f.label,
        }
    }
    fn marked(&self) -> bool {
        match self {
            Self::Pc(r) => r.marked,
            Self::Speaker(r) => r.marked,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Keyboard {
    last: Option<Id>,
    focus_place: bool,
    prefix: String,
    typed_at: f64,
}
impl Keyboard {
    pub(super) fn focus_place(&mut self) {
        self.focus_place = true;
        self.prefix.clear();
    }

    fn tab_row(&mut self, ui: &Ui, entries: &[Entry<'_>]) -> Id {
        let list = ui.id();
        let marked = entries
            .iter()
            .find(|r| r.marked())
            .unwrap_or(&entries[0])
            .id(list);
        if std::mem::take(&mut self.focus_place) {
            self.last = Some(marked);
            ui.memory_mut(|m| m.request_focus(marked));
            return marked;
        }
        let focused = ui.memory(|m| m.focused());
        let Some(i) = entries.iter().position(|r| Some(r.id(list)) == focused) else {
            self.prefix.clear();
            return self
                .last
                .filter(|id| entries.iter().any(|r| r.id(list) == *id))
                .unwrap_or(marked);
        };
        let mut to = i;
        ui.input(|input| {
            for event in &input.events {
                match event {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => match key {
                        Key::ArrowUp => to = to.saturating_sub(1),
                        Key::ArrowDown => to = (to + 1).min(entries.len() - 1),
                        Key::Home => to = 0,
                        Key::End => to = entries.len() - 1,
                        _ => {}
                    },
                    egui::Event::Text(text) => {
                        for ch in text.chars().filter(|ch| ch.is_alphabetic()) {
                            let letter = ch.to_lowercase().to_string();
                            if input.time - self.typed_at >= 1.0 || self.prefix == letter {
                                self.prefix.clear();
                            }
                            let start = to + usize::from(self.prefix.is_empty());
                            self.prefix.push_str(&letter);
                            self.typed_at = input.time;
                            if let Some(j) = (start..start + entries.len())
                                .map(|j| j % entries.len())
                                .find(|&j| {
                                    entries[j].name().to_lowercase().starts_with(&self.prefix)
                                })
                            {
                                to = j;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        let id = entries[to].id(list);
        if i != to {
            ui.memory_mut(|m| m.request_focus(id));
        }
        self.last = Some(id);
        id
    }
}

#[derive(Clone, Copy)]
struct RowFocus {
    id: Id,
    tab: bool,
}
impl RowFocus {
    fn hit(self, ui: &Ui, r: Rect, click: bool) -> Response {
        let resp = ui.interact(
            r,
            self.id,
            Sense {
                click,
                drag: false,
                focusable: self.tab,
            },
        );
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter {
                        vertical_arrows: true,
                        horizontal_arrows: true,
                        ..Default::default()
                    },
                )
            });
            // egui installs the filter once the widget had focus on the previous pass.
            if resp.gained_focus() {
                ui.ctx()
                    .request_discard("install the row's arrow-key filter");
            }
        }
        resp
    }
    fn action(self, ui: &Ui, r: Rect, suffix: &str) -> Response {
        ui.interact(
            r,
            self.id.with(suffix),
            Sense {
                focusable: self.tab,
                ..Sense::click()
            },
        )
    }
}

struct Item<'a> {
    label: &'a str,
    spoken: &'a str,
    tooltip: Option<&'a str>,
    marked: bool,
    command: Option<&'a Command>,
}
impl Item<'_> {
    fn plate(
        &self,
        ui: &mut Ui,
        r: Rect,
        focus: RowFocus,
        fold: bool,
        t: &Theme,
        out: &mut Drawn,
    ) -> bool {
        let active = self.command.is_some() || fold;
        let resp = focus.hit(ui, r, active);
        resp.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::SelectableLabel,
                active,
                self.marked,
                self.spoken,
            )
        });
        let l = look_as(ui, &resp, self.spoken);
        let r = ui.painter().round_rect_to_pixels(r);
        if self.marked {
            ui.painter().rect_filled(r, ROUND, t.c.selected);
        }
        if (active || self.marked) && l.hovered {
            ui.painter().rect_filled(
                r,
                ROUND,
                if active && l.pressed {
                    t.c.pressed
                } else {
                    t.c.hover
                },
            );
        }
        if self.marked {
            ui.painter().rect_filled(
                ui.painter().round_rect_to_pixels(Rect::from_min_size(
                    egui::pos2(r.left(), r.center().y - RADIO / 2.0),
                    vec2(BAR, RADIO),
                )),
                BAR / 2.0,
                t.c.accent,
            );
        }
        if l.focused {
            focus_ring(ui.painter(), r, ROUND, false, t);
        }
        out.record(ui, &resp, self.spoken, active, self.command);
        let clicked = resp.clicked() && active;
        if let Some(tip) = self.tooltip {
            resp.on_hover_text(tip);
        }
        clicked
    }
}
struct PlaceRow<'a> {
    item: Item<'a>,
    kind: RowIcon,
    grey: bool,
    text: Option<&'a str>,
    mark: Option<Mark>,
    link: Option<&'a Act>,
    action: Option<(&'a Act, Icon)>,
}
impl<'a> From<&'a Row> for PlaceRow<'a> {
    fn from(r: &'a Row) -> Self {
        Self {
            item: Item {
                label: &r.name,
                spoken: &r.spoken,
                tooltip: r.tooltip.as_deref(),
                marked: r.marked,
                command: r.command.as_ref(),
            },
            kind: r.icon,
            grey: r.grey,
            text: r.text.as_deref(),
            mark: r.mark,
            link: r.link.as_ref(),
            action: Some((&r.gear, Icon::Gear)),
        }
    }
}
impl<'a> From<&'a PcRow> for PlaceRow<'a> {
    fn from(r: &'a PcRow) -> Self {
        Self {
            item: Item {
                label: &r.name,
                spoken: &r.name,
                tooltip: r.tooltip.as_deref(),
                marked: r.marked,
                command: r.command.as_ref(),
            },
            kind: RowIcon::Pc,
            grey: false,
            text: Some(&r.output),
            mark: None,
            link: None,
            action: r.arrow.as_ref().map(|a| (a, arrow(!r.outputs.is_empty()))),
        }
    }
}
fn plate(origin: Pos2, h: f32) -> Rect {
    rect(origin, INSET, NEAR, EDGE - INSET, h - 2.0 * NEAR)
}
impl PlaceRow<'_> {
    fn draw(self, ui: &mut Ui, origin: Pos2, focus: RowFocus, t: &Theme, out: &mut Drawn) {
        self.item
            .plate(ui, plate(origin, ROW), focus, false, t, out);
        let cy = ROW / 2.0;
        icon(self.kind).paint_flyout(
            ui.painter(),
            square(origin + vec2(ICON, cy), GLYPH),
            if self.grey {
                t.c.text_disabled
            } else {
                t.c.text
            },
        );
        let link_w = self
            .link
            .map_or(0.0, |a| width(ui, &a.label, t.font(CAPTION)));
        let mark_w = self.mark.map_or(0.0, |_| {
            MARK + if self.text.is_some() || self.link.is_some() {
                TIGHT
            } else {
                0.0
            }
        });
        let text_end = END
            - link_w
            - if self.link.is_some() && self.text.is_some() {
                TIGHT
            } else {
                0.0
            };
        let text = self.text.map(|s| {
            one(
                ui,
                job(s, t.font(CAPTION), t.c.text_secondary),
                text_end - TEXT - NAME_MIN - GUTTER - mark_w,
            )
        });
        let text_w = text.as_ref().map_or(0.0, |g| g.size().x.ceil());
        let state_start = text_end - text_w - mark_w;
        let name_end = if state_start < END {
            state_start - GUTTER
        } else {
            // Without status text, the name can use the space up to the gear slot.
            ACTION - HIT / 2.0
        };
        label(
            ui,
            line(origin, TEXT, name_end, cy, BODY_LINE),
            self.item.label,
            if self.item.marked {
                t.strong(BODY)
            } else {
                t.font(BODY)
            },
            if self.grey {
                t.c.text_secondary
            } else {
                t.c.text
            },
        );
        if let Some(m) = self.mark {
            mark(ui, m, origin + vec2(state_start + MARK / 2.0, cy), MARK, t);
        }
        if let Some(g) = text {
            paint(
                ui,
                line(origin, text_end - text_w, text_end, cy, CAP_LINE),
                g,
            );
        }
        if let Some(a) = self.link {
            let words = line(origin, END - link_w, END, cy, LINK_HIT);
            draw_link(
                ui,
                focus.action(ui, words, "link"),
                words,
                a,
                CAPTION,
                t,
                out,
            );
        }
        if let Some((a, glyph)) = self.action {
            let gear = glyph == Icon::Gear;
            out.icon(
                ui,
                focus.action(ui, square(origin + vec2(ACTION, cy), HIT), "action"),
                a,
                glyph,
                if gear { GEAR } else { MARK },
                if gear && !self.item.marked {
                    t.c.text_disabled
                } else {
                    t.c.text_secondary
                },
                t,
            );
        }
    }
}
fn rows(
    ui: &mut Ui,
    origin: Pos2,
    entries: &[Entry<'_>],
    list_id: Id,
    tab: Id,
    search: Option<&crate::model::panel::SearchLine>,
    state: &mut PanelUi,
    t: &Theme,
    out: &mut Drawn,
) {
    let mut y = origin.y;
    // Stable row IDs also distinguish two gears with the same spoken name.
    let mut next = |h: f32, draw: &mut dyn FnMut(&mut Ui, Pos2)| {
        let o = egui::pos2(origin.x, y.round());
        let r = Rect::from_min_size(o, vec2(WIDTH, h));
        let mut u = ui.new_child(egui::UiBuilder::new().max_rect(r));
        // Register offscreen actions too: Tab can focus one and scroll it into view.
        draw(&mut u, o);
        y += h;
    };
    for entry in entries {
        let id = entry.id(list_id);
        let focus = RowFocus { id, tab: id == tab };
        match entry {
            Entry::Pc(_) => unreachable!("This PC is pinned above the scroll area"),
            Entry::Speaker(row) => next(ROW, &mut |u, o| {
                PlaceRow::from(*row).draw(u, o, focus, t, out)
            }),
            Entry::Output(row) => next(SHORT_ROW, &mut |ui, o| {
                let r = plate(o, SHORT_ROW);
                let resp = focus.hit(ui, r, true);
                resp.widget_info(|| {
                    WidgetInfo::selected(WidgetType::SelectableLabel, true, row.marked, &row.name)
                });
                let l = look_as(ui, &resp, &row.name);
                let r = ui.painter().round_rect_to_pixels(r);
                if l.hovered {
                    ui.painter().rect_filled(
                        r,
                        ROUND,
                        if l.pressed { t.c.pressed } else { t.c.hover },
                    );
                }
                if l.focused {
                    focus_ring(ui.painter(), r, ROUND, false, t);
                }
                if row.marked {
                    Icon::Check.paint(
                        ui.painter(),
                        square(o + vec2(ICON, SHORT_ROW / 2.0), MARK),
                        t.c.accent,
                    );
                }
                label(
                    ui,
                    line(o, TEXT, RIGHT, SHORT_ROW / 2.0, SMALL_LINE),
                    &row.name,
                    t.font(SMALL),
                    t.c.text,
                );
                out.record(ui, &resp, &row.name, true, Some(&row.command));
                resp.on_hover_text(&row.tooltip);
            }),
            Entry::Fold(f) => next(ROW, &mut |u, o| {
                let open = state.more_open;
                let it = Item {
                    label: &f.label,
                    spoken: &f.label,
                    marked: false,
                    command: None,
                    tooltip: None,
                };
                let cy = ROW / 2.0;
                if it.plate(u, plate(o, ROW), focus, true, t, out) {
                    state.more_open = !open;
                    u.ctx().request_repaint();
                }
                Icon::Group.paint_flyout(
                    u.painter(),
                    square(o + vec2(ICON, cy), GLYPH),
                    t.c.text_secondary,
                );
                label(
                    u,
                    line(o, TEXT, END, cy, BODY_LINE),
                    &f.label,
                    t.font(BODY),
                    t.c.text,
                );
                arrow(open).paint(
                    u.painter(),
                    square(o + vec2(ACTION, cy), MARK),
                    t.c.text_secondary,
                );
            }),
        }
    }
    if let Some(s) = search {
        next(SHORT_ROW, &mut |u, o| {
            search_line(u, o, s, t, out);
        });
    }
}
fn search_line(u: &mut Ui, o: Pos2, s: &crate::model::panel::SearchLine, t: &Theme, out: &mut Drawn) {
    let cy = SHORT_ROW / 2.0;
    if s.spinner {
        spinner(u, o + vec2(ICON, cy), t.c.accent);
    }
    let w = s
        .link
        .as_ref()
        .map_or(0.0, |a| width(u, &a.label, t.font(CAPTION)) + TIGHT);
    let used = label(
        u,
        line(o, TEXT, RIGHT - w, cy, CAP_LINE),
        &s.text,
        t.font(CAPTION),
        t.c.text_secondary,
    );
    if let Some(a) = &s.link {
        let x = TEXT + used + TIGHT;
        link(
            u,
            line(o, x, x + w - TIGHT, cy, LINK_HIT),
            0.0,
            a,
            CAPTION,
            t,
            out,
        );
    }
}

/// The welcome page has the panel's rows and keyboard navigation, with
/// each available row acting as a chime button.
#[derive(Default)]
pub(crate) struct ChimePicker {
    keyboard: Keyboard,
}

impl ChimePicker {
    pub(crate) fn show(
        &mut self,
        ui: &mut Ui,
        snap: &crate::model::Snapshot,
        mut question: impl FnMut(&mut Ui, &crate::model::SpeakerId),
    ) -> Vec<Command> {
        use crate::model::{command::FirstRunAction, panel::{speaker_row, SearchLine}, text::t};
        let scene = &snap.scene;
        let theme = Theme::of(ui.ctx());
        let rows: Vec<_> = scene.speakers.iter().map(|s| {
            let mut row = speaker_row(scene, s);
            row.spoken = t!(snap.lang, ChimeOnSpeaker, speaker = &row.name);
            row.tooltip = Some(row.spoken.clone());
            row.marked = false;
            row.command = (!row.grey).then(|| Command::FirstRunAction(FirstRunAction::Chime(s.id.clone())));
            row
        }).collect();
        let entries: Vec<_> = rows.iter().map(Entry::Speaker).collect();
        let tab = (!rows.is_empty()).then(|| self.keyboard.tab_row(ui, &entries));
        let mut out = Drawn { commands: Vec::new(), controls: Vec::new(), parts: [Rect::NOTHING; 4], natural_height: 0.0 };
        for ((row, entry), speaker) in rows.iter().zip(entries).zip(&scene.speakers) {
            let id = entry.id(ui.id());
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::hover());
            let mut place = PlaceRow::from(row);
            place.action = None;
            place.draw(ui, r.min - vec2(LEFT, 0.0), RowFocus { id, tab: Some(id) == tab }, &theme, &mut out);
            if matches!(&scene.situation.confirm_in_panel, Some(crate::model::situation::Confirm::ChimeBusy { speaker: target, .. }) if target == &speaker.id) {
                question(ui, &speaker.id);
            }
        }
        let search = crate::model::panel::search_line(scene).unwrap_or_else(|| SearchLine {
            text: if rows.is_empty() { t!(snap.lang, Searching) } else { t!(snap.lang, LookingForMore) },
            spinner: true, link: None,
        });
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), SHORT_ROW), Sense::hover());
        search_line(ui, r.min - vec2(LEFT, 0.0), &search, &theme, &mut out);
        out.commands
    }
}

pub(super) fn draw(
    ui: &mut Ui,
    r: Rect,
    list: &ListPart,
    state: &mut PanelUi,
    t: &Theme,
    out: &mut Drawn,
) {
    let list_id = ui.id();
    let mut entries = vec![Entry::Pc(&list.this_pc)];
    entries.extend(list.this_pc.outputs.iter().map(Entry::Output));
    entries.extend(list.rows.iter().map(Entry::Speaker));
    if let Some(fold) = &list.more {
        entries.push(Entry::Fold(fold));
        if state.more_open {
            entries.extend(fold.rows.iter().map(Entry::Speaker));
        }
    }
    let tab = state.keyboard.tab_row(ui, &entries);
    let pc_id = entries[0].id(list_id);
    PlaceRow::from(&list.this_pc).draw(
        ui,
        r.min + vec2(0.0, TIGHT),
        RowFocus {
            id: pc_id,
            tab: pc_id == tab,
        },
        t,
        out,
    );
    let content_h = height(list, state.more_open) - ROW - 2.0 * TIGHT;
    let viewport = rect(
        r.min,
        0.0,
        TIGHT + ROW,
        WIDTH,
        r.height() - ROW - 2.0 * TIGHT,
    );
    if content_h <= viewport.height() {
        rows(
            ui,
            viewport.min,
            &entries[1..],
            list_id,
            tab,
            list.search.as_ref(),
            state,
            t,
            out,
        );
        return;
    }
    let mut scroll = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(viewport)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    scroll.set_clip_rect(ui.clip_rect().intersect(viewport));
    let area = egui::ScrollArea::vertical()
        .id_salt("speaker-scroll")
        .max_height(viewport.height())
        .auto_shrink([false, false])
        .drag_to_scroll(false)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show_viewport(&mut scroll, |u, visible| {
            // The pan gesture and scrollbar leave keyboard focus on the row.
            let pan = u.interact(
                viewport,
                u.id().with("pan"),
                Sense {
                    focusable: false,
                    ..Sense::drag()
                },
            );
            if pan.dragged() {
                u.scroll_with_delta_animation(
                    pan.drag_delta(),
                    egui::style::ScrollAnimation::none(),
                );
            }
            let (body, _) = u.allocate_exact_size(vec2(WIDTH, content_h), Sense::hover());
            rows(
                u,
                body.min,
                &entries[1..],
                list_id,
                tab,
                list.search.as_ref(),
                state,
                t,
                out,
            );
            scroll_bar(u, viewport, content_h, visible.top(), t);
        });
    if area.state.offset.y > 0.0 {
        for i in 0..TIGHT as u8 {
            let alpha = 0.08 * (1.0 - f32::from(i) / TIGHT);
            ui.painter().rect_filled(
                ui.painter()
                    .round_rect_to_pixels(rect(viewport.min, 0.0, i.into(), WIDTH, RULE)),
                0.0,
                crate::ui::theme::translucent(0, alpha),
            );
        }
    }
}

/// The thumb travels entirely in the outer gutter; wheel and keyboard scrolls
/// use the same ScrollArea offset as dragging it.
fn scroll_bar(ui: &mut Ui, viewport: Rect, content: f32, offset: f32, t: &Theme) {
    let h = viewport.height();
    let thumb_h = (h * h / content).round().max(BAR).min(h - RULE);
    let ratio = (h - thumb_h) / (content - h);
    let track = rect(viewport.min, EDGE, 0.0, BAR, h);
    let thumb = rect(track.min, 0.0, offset * ratio, BAR, thumb_h);
    ui.painter().rect_filled(
        ui.painter().round_rect_to_pixels(thumb),
        BAR / 2.0,
        t.c.control_strong.gamma_multiply(0.45),
    );
    let response = ui.interact(
        track,
        ui.id().with("scroll-thumb"),
        Sense {
            focusable: false,
            ..Sense::click_and_drag()
        },
    );
    let delta = if response.dragged() {
        response.drag_delta().y
    } else if let Some(p) = response
        .interact_pointer_pos()
        .filter(|p| response.clicked() && !thumb.contains(*p))
    {
        p.y - thumb.center().y
    } else {
        return;
    };
    ui.scroll_with_delta_animation(
        vec2(0.0, -delta / ratio),
        egui::style::ScrollAnimation::none(),
    );
}
