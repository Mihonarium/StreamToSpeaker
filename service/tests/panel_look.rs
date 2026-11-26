//! Geometry from the painter, hit targets from the flyout, expectations from
//! the model. Drive egui with the same pointer and keyboard events as a window.
use egui::{pos2, vec2, Event, Key, Modifiers, Pos2, Rect, Shape};
use stream_to_speaker::{
    model::{
        panel::*,
        samples::{self, Sample},
        Command,
    },
    surface::panel::{show, Control, Drawn, PanelUi, WIDTH},
    ui::{preview, theme::Mode},
};

const SCALES: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];

struct Text {
    words: String,
    rect: Rect,
    rows: Vec<String>,
    cut: bool,
}
struct Frame {
    out: Drawn,
    texts: Vec<Text>,
    focused: Vec<String>,
    circles: Vec<Pos2>,
    shapes: Vec<Shape>,
}
struct Window {
    ctx: egui::Context,
    view: Panel,
    state: PanelUi,
    scale: f32,
    frame: u32,
    cap: f32,
}
fn collect(shape: &Shape, texts: &mut Vec<Text>, circles: &mut Vec<Pos2>) {
    match shape {
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, texts, circles)),
        Shape::Text(t) if !t.galley.text().trim().is_empty() => texts.push(Text {
            words: t.galley.text().into(),
            rect: Rect::from_min_size(t.pos, t.galley.size()),
            rows: t.galley.rows.iter().map(|r| r.text()).collect(),
            cut: t.galley.elided,
        }),
        Shape::Circle(c) if (3.0..=5.0).contains(&c.radius) => circles.push(c.center),
        _ => {}
    }
}
impl Window {
    fn new(s: &Sample, scale: f32) -> Self {
        // The regular headless suite uses the app's built-in font stack.
        // PANEL_PREVIEW_FONTS runs these checks with fonts from that directory, such as Windows’ own.
        static FONTS: std::sync::OnceLock<egui::FontDefinitions> = std::sync::OnceLock::new();
        let fonts = FONTS
            .get_or_init(|| match std::env::var_os("PANEL_PREVIEW_FONTS") {
                Some(dir) => preview::fonts_from_dir(std::path::Path::new(&dir)),
                None => stream_to_speaker::fonts::definitions(&[], &mut |_| None).0,
            })
            .clone();
        let mut state = PanelUi::default();
        state.more_open = s.name == "fold-open";
        Self {
            ctx: preview::context(fonts, Mode::Light),
            view: panel(&s.scene),
            scale,
            state,
            frame: 0,
            cap: f32::INFINITY,
        }
    }
    fn frame(&mut self, events: Vec<Event>) -> Frame {
        self.frame += 1;
        let mut input = preview::input(vec2(WIDTH, 1800.0), self.scale, self.frame as f64 * 0.05);
        input.events = events;
        let mut drawn = None;
        let output = self.ctx.run(input, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    drawn = Some(show(ui, &self.view, &mut self.state, self.cap));
                });
        });
        let focused = output
            .platform_output
            .events
            .iter()
            .filter_map(|event| match event {
                egui::output::OutputEvent::FocusGained(info) => info.label.clone(),
                _ => None,
            })
            .collect();
        let mut circles = vec![];
        let mut texts = vec![];
        let shapes: Vec<_> = output.shapes.into_iter().map(|s| s.shape).collect();
        for shape in &shapes {
            collect(shape, &mut texts, &mut circles);
        }
        Frame {
            out: drawn.unwrap(),
            texts,
            focused,
            circles,
            shapes,
        }
    }
    fn settle(&mut self) -> Frame {
        self.frame(vec![]);
        self.frame(vec![]);
        self.frame(vec![])
    }
    fn click(&mut self, at: Pos2) -> Vec<Command> {
        [
            vec![Event::PointerMoved(at)],
            vec![press(at, true)],
            vec![press(at, false)],
        ]
        .into_iter()
        .flat_map(|ev| self.frame(ev).out.commands)
        .collect()
    }
}
fn press(at: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}
fn each(mut test: impl FnMut(&str, &mut Window)) {
    for s in samples::all() {
        for scale in SCALES {
            test(s.name, &mut Window::new(&s, scale));
        }
    }
}
fn named(name: &str, scale: f32) -> Window {
    Window::new(
        &Sample {
            name: "",
            scene: samples::named(name),
        },
        scale,
    )
}
fn spoken(a: &Act) -> &str {
    a.spoken.as_deref().unwrap_or(&a.label)
}
fn get<'a>(f: &'a Frame, name: &str) -> &'a Control {
    f.out
        .controls
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("missing control {name}"))
}
fn small_diff(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1.0
}

#[test]
fn part_heights_and_dividers() {
    each(|name, h| {
        let f = h.settle();
        let [place, list, about, links] = f.out.parts;
        assert_eq!(place.height(), 116.0, "{name}");
        assert_eq!(links.height(), 44.0, "{name}");
        assert_eq!(about.height(), 192.0, "{name} at {}", h.scale);
        for (a, b) in [(place, list), (list, about), (about, links)] {
            assert_eq!(a.bottom() + 1.0, b.top(), "{name}");
        }
        let divider = stream_to_speaker::ui::theme::Theme::of(&h.ctx).c.divider;
        let rules: Vec<_> = f
            .shapes
            .iter()
            .filter_map(|shape| match shape {
                Shape::Rect(r) if r.fill == divider && r.rect.width() > WIDTH - 1.0 => Some(r.rect),
                _ => None,
            })
            .collect();
        assert_eq!(rules.len(), 3, "{name} at {}", h.scale);
        for (rule, part) in rules.iter().zip(&f.out.parts) {
            let pixel = |y: f32| (y * h.scale).round() / h.scale;
            assert!((rule.top() - pixel(part.bottom())).abs() < 0.001);
            assert!((rule.bottom() - pixel(part.bottom() + 1.0)).abs() < 0.001);
        }
        for r in f.out.parts {
            assert_eq!(r.width(), 382.0);
        }
        assert_eq!(links.bottom() - place.top(), f.out.natural_height, "{name}");
    });
}

// Enumerate independently of the renderer, including disabled actions and twins.
fn expected(p: &Panel, open: bool, tab: bool) -> Vec<(String, bool, Option<Command>)> {
    let mut v = vec![];
    let action = |a: &Act, enabled| {
        (
            spoken(a).into(),
            enabled,
            Some(a.command.clone()).filter(|_| enabled),
        )
    };
    if let Some(vol) = &p.place.volume {
        let owner = vol.owner.clone().filter(|_| !vol.dimmed);
        v.push((
            vol.mute_spoken.clone(),
            owner.is_some(),
            owner.clone().map(|owner| Command::SetMuted {
                owner,
                muted: !vol.muted,
            }),
        ));
        v.push((vol.spoken.clone(), owner.is_some(), None));
    }
    if let Some(a) = &p.place.state.link {
        v.push(action(a, true));
    }
    let pc = &p.list.this_pc;
    if !tab || pc.marked {
        v.push((
            pc.name.clone(),
            tab || pc.command.is_some(),
            pc.command.clone(),
        ));
        if let Some(a) = &pc.arrow {
            v.push(action(a, true));
        }
    }
    if !tab {
        for r in &pc.outputs {
            v.push((r.name.clone(), true, Some(r.command.clone())));
        }
    }
    let row = |v: &mut Vec<_>, r: &Row| {
        if !tab || r.marked {
            v.push((
                r.spoken.clone(),
                tab || r.command.is_some(),
                r.command.clone(),
            ));
            if let Some(a) = &r.link {
                v.push(action(a, true));
            }
            v.push(action(&r.gear, true));
        }
    };
    for r in &p.list.rows {
        row(&mut v, r);
    }
    if let Some(m) = &p.list.more {
        if !tab {
            v.push((m.label.clone(), true, None));
        }
        if open {
            for r in &m.rows {
                row(&mut v, r);
            }
        }
    }
    if let Some(a) = p.list.search.as_ref().and_then(|s| s.link.as_ref()) {
        v.push(action(a, true));
    }
    match &p.about {
        About::Delay(d) => {
            if let DelayHead::Ways { lines, .. } = &d.head {
                for w in lines {
                    v.push((
                        w.spoken.clone(),
                        w.command.is_some() && !d.dimmed,
                        w.command.clone().filter(|_| !d.dimmed),
                    ));
                    if let Some(a) = &w.connecting {
                        v.push(action(a, true));
                    }
                }
            }
            for r in &d.rows {
                for a in [&r.small, &r.large] {
                    v.push(action(a, r.enabled && !d.dimmed));
                }
            }
        }
        About::Note(n) => {
            for a in [&n.button, &n.close].into_iter().flatten() {
                v.push(action(a, true));
            }
        }
        About::Question(q) => {
            v.push(action(&q.yes, true));
            v.push(action(&q.no, true));
        }
        About::Line { link: Some(a), .. } => v.push(action(a, true)),
        _ => {}
    }
    for a in &p.links {
        v.push(action(a, true));
    }
    v
}
#[test]
fn every_hit_target_sends_its_own_command() {
    each(|name, h| {
        let f = h.settle();
        let expected = expected(&h.view, h.state.more_open, false);
        assert_eq!(f.out.controls.len(), expected.len(), "{name}");
        let open = h.state.more_open;
        for (c, (label, enabled, cmd)) in f.out.controls.iter().zip(expected) {
            assert_eq!(
                (&c.name, c.enabled, &c.command),
                (&label, enabled, &cmd),
                "{name}"
            );
            // The slider's command depends on where the pointer lands.
            let mut cmd = cmd;
            if let Some(v) = h
                .view
                .place
                .volume
                .as_ref()
                .filter(|v| enabled && v.spoken == label)
            {
                if (i32::from(v.level) - 50).abs() > 3 && v.limit.unwrap_or(100).min(50) != v.level
                {
                    cmd = Some(Command::SetVolume {
                        owner: v.owner.clone().unwrap(),
                        level: v.limit.unwrap_or(100).min(50),
                    });
                }
            }
            assert_eq!(
                h.click(c.rect.center()),
                cmd.into_iter().collect::<Vec<_>>(),
                "{name}, {label} at {:?}",
                c.rect
            );
            if open != h.state.more_open {
                h.state.more_open = open;
                h.settle();
            }
        }
    });
}

fn buttons(p: &Panel, f: &Frame) -> Vec<Rect> {
    let acts: Vec<_> = match &p.about {
        About::Delay(d) => d.rows.iter().flat_map(|r| [&r.small, &r.large]).collect(),
        About::Note(n) => n.button.iter().collect(),
        About::Question(q) => vec![&q.yes, &q.no],
        _ => vec![],
    };
    acts.into_iter().map(|a| get(f, spoken(a)).rect).collect()
}
#[test]
fn control_columns_and_amount_rows() {
    each(|name, h| {
        let f = h.settle();
        let p = &h.view;
        let plate = |r: Rect| assert_eq!((r.left(), r.right()), (4.0, 378.0), "{name} {r:?}");
        let pixel = |x: f32| (x * h.scale).round() / h.scale;
        let side = |r: Rect| {
            assert_eq!(
                (r.center().x, r.size()),
                (358.0, vec2(32.0, 32.0)),
                "{name}"
            )
        };
        if let Some(v) = &p.place.volume {
            assert_eq!(get(&f, &v.mute_spoken).rect.center().x, 24.0);
        }
        let pc = &p.list.this_pc;
        plate(get(&f, &pc.name).rect);
        if let Some(a) = &pc.arrow {
            side(get(&f, spoken(a)).rect);
        }
        for r in &pc.outputs {
            plate(get(&f, &r.name).rect);
        }
        for r in p.list.rows.iter().chain(
            p.list
                .more
                .iter()
                .filter(|_| h.state.more_open)
                .flat_map(|m| &m.rows),
        ) {
            plate(get(&f, &r.spoken).rect);
            side(get(&f, &r.gear.label).rect);
        }
        if let Some(m) = &p.list.more {
            plate(get(&f, &m.label).rect);
        }
        let b = buttons(p, &f);
        match &p.about {
            About::Delay(_) => {
                for (i, r) in b.iter().enumerate() {
                    assert!((r.width() - 56.0).abs() < 0.001 && (r.height() - 28.0).abs() < 0.001);
                    assert!((r.right() - pixel([304.0, 366.0][i % 2])).abs() < 0.001);
                }
                assert_eq!(b[0].top(), b[1].top());
                assert_eq!(b[2].top(), b[3].top());
                assert!((b[2].top() - b[0].bottom() - 12.0).abs() < 0.001);
            }
            About::Note(n) => {
                if !b.is_empty() {
                    assert!(
                        (b[0].left() - pixel(46.0)).abs() < 0.001
                            && (b[0].height() - 32.0).abs() < 0.001
                    );
                }
                if let Some(a) = &n.close {
                    side(get(&f, spoken(a)).rect);
                }
            }
            About::Question(_) => {
                assert!((b[0].left() - pixel(46.0)).abs() < 0.001);
                assert!(b.iter().all(|r| (r.height() - 32.0).abs() < 0.001));
                assert_eq!(b[0].top(), b[1].top());
                assert!((b[1].left() - b[0].right() - 8.0).abs() < 0.001);
            }
            _ => {}
        }
    });
}
#[test]
fn text_runs_share_the_grid() {
    each(|name, h| {
        let f = h.settle();
        let b = buttons(&h.view, &f);
        let mut starts = vec![16.0, 46.0];
        if let About::Delay(DelayControls {
            head: DelayHead::Ways { lines, .. },
            ..
        }) = &h.view.about
        {
            let font = stream_to_speaker::ui::theme::Theme::of(&h.ctx).font(13.0);
            let widest = lines
                .iter()
                .map(|w| {
                    h.ctx.fonts(|fonts| {
                        fonts
                            .layout_no_wrap(w.name.clone(), font.clone(), egui::Color32::WHITE)
                            .size()
                            .x
                            .ceil()
                    })
                })
                .fold(50.0, f32::max);
            starts.push(46.0 + widest + 8.0);
        }
        let mut fragments: Vec<_> = f
            .texts
            .iter()
            .filter(|t| {
                !b.iter().any(|r| r.contains(t.rect.center()))
                    && !(f.out.parts[3].contains(t.rect.center())
                        && t.words == h.view.links[1].label)
            })
            .collect();
        fragments.sort_by(|a, b| a.rect.left().total_cmp(&b.rect.left()));
        while !fragments.is_empty() {
            let first = fragments.remove(0);
            let mut r = first.rect;
            let mut text = first.words.clone();
            while let Some(i) = fragments.iter().position(|t| {
                (t.rect.center().y - r.center().y).abs() <= 4.0
                    && t.rect.left() >= r.right() - 1.0
                    && t.rect.left() - r.right() <= 14.0
            }) {
                let t = fragments.remove(i);
                r = r.union(t.rect);
                text.push_str(&t.words);
            }
            assert!(
                starts.iter().any(|x| small_diff(*x, r.left()))
                    || [236.0, 334.0, 366.0]
                        .iter()
                        .any(|x| small_diff(*x, r.right())),
                "{name}: {text:?} at {r:?}"
            );
        }
    });
}
fn clean(f: &Frame, p: &Panel, name: &str, orphans: bool) {
    let bounds = f.out.parts[0].union(f.out.parts[3]);
    let buttons = buttons(p, f);
    let ways_label = match &p.about {
        About::Delay(DelayControls {
            head: DelayHead::Ways { label, .. },
            ..
        }) => Some(label),
        _ => None,
    };
    for (i, t) in f.texts.iter().enumerate() {
        assert!(
            bounds.expand(0.6).contains_rect(t.rect),
            "{name} {:?} spills at {:?}",
            t.words,
            t.rect
        );
        for r in &buttons {
            if r.contains(t.rect.center()) {
                assert!(
                    r.expand(0.6).contains_rect(t.rect),
                    "{name}: {:?} spills from button",
                    t.words
                );
            }
        }
        for u in &f.texts[i + 1..] {
            let overlap = t.rect.intersect(u.rect);
            assert!(
                overlap.width() <= 1.0 || overlap.height() <= 1.0,
                "{name}: {:?} {:?} overlaps {:?} {:?}",
                t.words,
                t.rect,
                u.words,
                u.rect
            );
        }
        if f.out.parts[..2].iter().any(|r| r.contains(t.rect.center()))
            || ways_label == Some(&t.words)
        {
            assert_eq!(t.rows.len(), 1, "{name}: {:?} wrapped", t.words);
        }
        if orphans && t.rows.len() > 1 && t.words.split_whitespace().count() >= 3 {
            assert!(
                t.rows.last().unwrap().split_whitespace().count() >= 2,
                "{name}: {:?}",
                t.rows
            );
        }
    }
}
#[test]
fn bounds_wrapping_and_truncation() {
    let mut normal = Vec::new();
    each(|name, h| {
        let f = h.settle();
        clean(&f, &h.view, name, true);
        if h.scale == 1.0 {
            normal = f
                .texts
                .iter()
                .map(|t| (t.words.clone(), t.rows.len(), t.cut))
                .collect();
        } else {
            assert_eq!(f.texts.len(), normal.len(), "{name} at {}", h.scale);
            for (t, (words, lines, cut)) in f.texts.iter().zip(&normal) {
                assert_eq!(&t.words, words, "{name} at {}", h.scale);
                assert!(
                    t.rows.len() <= *lines,
                    "{name} at {}: {words} gained a line",
                    h.scale
                );
                if !*cut && t.cut {
                    // With the preview fonts this name ends at 344 points at
                    // 175%. Office-style gear hit targets start at 342.
                    assert_eq!(
                        (name, h.scale, words.as_str()),
                        (
                            "long-names",
                            1.75,
                            "Bedroom HomePod Stereo Pair by the Window"
                        ),
                        "new ellipsis"
                    );
                    let row = h.view.list.rows.iter().find(|r| r.name == *words).unwrap();
                    let font = stream_to_speaker::ui::theme::Theme::of(&h.ctx).font(14.0);
                    let width = h.ctx.fonts(|fonts| {
                        fonts
                            .layout_no_wrap(words.clone(), font, egui::Color32::WHITE)
                            .size()
                            .x
                    });
                    assert!(t.rect.left() + width > get(&f, &row.gear.label).rect.left());
                }
            }
        }
        // Both amount labels fit their small buttons at the English sizes.
        if let About::Delay(d) = &h.view.about {
            for r in &d.rows {
                for a in [&r.small, &r.large] {
                    assert!(
                        f.texts
                            .iter()
                            .any(|t| get(&f, spoken(a)).rect.contains(t.rect.center())
                                && t.words == a.label
                                && !t.cut),
                        "{name}: {}",
                        a.label
                    );
                }
            }
        }
    });
}
fn longer(s: &mut String) {
    let extra = "x".repeat(s.chars().count().div_ceil(3));
    s.insert_str(s.find(' ').unwrap_or(s.len()), &extra);
}
fn longer_act(a: &mut Act) {
    longer(&mut a.label);
    if let Some(s) = &mut a.spoken {
        longer(s);
    }
}
fn longer_figure(f: &mut Figured) {
    for s in [&mut f.before, &mut f.figure, &mut f.after] {
        longer(s);
    }
}
fn longer_row(r: &mut Row) {
    longer(&mut r.name);
    longer(&mut r.spoken);
    for s in [&mut r.text, &mut r.tooltip].into_iter().flatten() {
        longer(s);
    }
    if let Some(a) = &mut r.link {
        longer_act(a);
    }
    longer_act(&mut r.gear);
}
fn lengthen(p: &mut Panel) {
    longer(&mut p.place.name);
    longer(&mut p.place.state.text);
    if let Some(s) = &mut p.place.output {
        longer(s);
    }
    if let Some(a) = &mut p.place.state.link {
        longer_act(a);
    }
    if let Some(v) = &mut p.place.volume {
        for s in [&mut v.number, &mut v.spoken, &mut v.mute_spoken] {
            longer(s);
        }
    }
    let pc = &mut p.list.this_pc;
    longer(&mut pc.name);
    longer(&mut pc.output);
    if let Some(s) = &mut pc.tooltip {
        longer(s);
    }
    if let Some(a) = &mut pc.arrow {
        longer_act(a);
    }
    for o in &mut pc.outputs {
        longer(&mut o.name);
        longer(&mut o.tooltip);
    }
    for r in &mut p.list.rows {
        longer_row(r);
    }
    if let Some(m) = &mut p.list.more {
        longer(&mut m.label);
        for r in &mut m.rows {
            longer_row(r);
        }
    }
    if let Some(s) = &mut p.list.search {
        longer(&mut s.text);
        if let Some(a) = &mut s.link {
            longer_act(a);
        }
    }
    match &mut p.about {
        About::Delay(d) => {
            match &mut d.head {
                DelayHead::One(f) => longer_figure(f),
                DelayHead::Ways { label, lines } => {
                    longer(label);
                    for w in lines {
                        longer(&mut w.name);
                        longer(&mut w.spoken);
                        longer_figure(&mut w.gives);
                        if let Some(a) = &mut w.connecting {
                            longer_act(a);
                        }
                    }
                }
            }
            for r in &mut d.rows {
                longer(&mut r.question);
                longer(&mut r.verb);
                longer_act(&mut r.small);
                longer_act(&mut r.large);
            }
            if let Some(s) = &mut d.line {
                longer(s);
            }
        }
        About::Note(n) => {
            longer(&mut n.title);
            longer(&mut n.sentence);
            for a in [&mut n.button, &mut n.close].into_iter().flatten() {
                longer_act(a);
            }
        }
        About::Question(q) => {
            longer(&mut q.title);
            longer(&mut q.sentence);
            longer_act(&mut q.yes);
            longer_act(&mut q.no);
        }
        About::Line { text, link } => {
            longer(text);
            if let Some(a) = link {
                longer_act(a);
            }
        }
        About::Empty => {}
    }
    p.links.iter_mut().for_each(longer_act);
}
#[test]
fn translated_words_stay_in_their_rows_and_buttons() {
    each(|name, h| {
        lengthen(&mut h.view);
        let f = h.settle();
        clean(&f, &h.view, name, false);
    });
}
#[test]
fn volume_tracks_the_drag_and_obeys_owner_mute_and_limit() {
    for scale in SCALES {
        for name in [
            "kitchen-muted",
            "kitchen-volume-limit",
            "pc-plain",
            "pc-no-output",
            "music-assistant-playing",
        ] {
            let mut h = named(name, scale);
            let f = h.settle();
            let v = h.view.place.volume.clone().unwrap();
            let mute = get(&f, &v.mute_spoken).rect.center();
            assert_eq!(
                h.click(mute),
                v.owner
                    .clone()
                    .map(|owner| Command::SetMuted {
                        owner,
                        muted: !v.muted
                    })
                    .into_iter()
                    .collect::<Vec<_>>()
            );
            let y = get(&f, &v.spoken).rect.center().y;
            let start = pos2(46.0, y);
            h.frame(vec![Event::PointerMoved(start)]);
            let down = h.frame(vec![press(start, true)]);
            let expected = v
                .owner
                .clone()
                .filter(|_| !v.dimmed && v.level != 0)
                .map(|owner| Command::SetVolume { owner, level: 0 });
            assert_eq!(down.out.commands, expected.into_iter().collect::<Vec<_>>());
            let mut levels = vec![];
            for x in [60.0, 90.0, 120.0, 120.0, 180.0, 250.0, 334.0, 350.0] {
                let at = pos2(x, y);
                let drag = h.frame(vec![Event::PointerMoved(at)]);
                for cmd in drag.out.commands {
                    let Command::SetVolume { owner, level } = cmd else {
                        panic!("unexpected {cmd:?}")
                    };
                    assert_eq!(Some(&owner), v.owner.as_ref());
                    assert!(level <= v.limit.unwrap_or(100));
                    let thumb_x =
                        ((46.0 + 288.0 * f32::from(level) / 100.0) * scale).round() / scale;
                    assert!(
                        drag.circles.contains(&pos2(thumb_x, y)),
                        "{name}: thumb still behind the pointer"
                    );
                    levels.push(level);
                }
                assert!(
                    h.frame(vec![]).out.commands.is_empty(),
                    "duplicate value during drag"
                );
            }
            assert!(h
                .frame(vec![press(pos2(350.0, y), false)])
                .out
                .commands
                .is_empty());
            if v.dimmed {
                assert!(levels.is_empty());
            } else {
                assert!(levels.len() >= 3, "{name}: {levels:?}");
                assert!(levels.windows(2).all(|a| a[0] < a[1]));
                assert_eq!(levels.last(), Some(&v.limit.unwrap_or(100)));
            }
        }
    }
}
#[test]
fn opening_the_fold_exposes_working_rows() {
    for scale in SCALES {
        let mut h = named("fold-closed", scale);
        let m = h.view.list.more.clone().unwrap();
        let f = h.settle();
        assert!(f.out.controls.iter().all(|c| c.name != m.rows[0].spoken));
        assert!(h.click(get(&f, &m.label).rect.center()).is_empty());
        assert!(h.state.more_open);
        let f = h.settle();
        for r in m.rows {
            assert_eq!(
                h.click(get(&f, &r.spoken).rect.center()),
                vec![r.command.unwrap()]
            );
        }
        assert!(h.click(get(&f, &m.label).rect.center()).is_empty());
        assert!(!h.state.more_open);
    }
}
#[test]
fn short_window_pins_pc_and_scrolls_to_the_last_speaker() {
    for scale in SCALES {
        let mut h = named("many-speakers", scale);
        h.cap = 640.0;
        let f = h.settle();
        let parts = f.out.parts;
        assert_eq!(parts[3].bottom() - parts[0].top(), 640.0);
        assert_eq!((parts[2].height(), parts[3].height()), (192.0, 44.0));
        assert!(f.out.natural_height > 640.0);
        let pc = get(&f, &h.view.list.this_pc.name).rect;
        assert_eq!(pc.top() - parts[1].top(), 8.0);
        let last = h.view.list.rows.last().unwrap().clone();
        assert!(f.out.controls.iter().all(|c| c.name != last.spoken));
        h.frame(vec![Event::PointerMoved(parts[1].center())]);
        let mut after = h.frame(vec![Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -1200.0),
            modifiers: Modifiers::NONE,
        }]);
        for _ in 0..16 {
            after = h.frame(vec![]);
        }
        assert_eq!(after.out.parts, parts);
        assert_eq!(get(&after, &h.view.list.this_pc.name).rect, pc);
        let row = get(&after, &last.spoken);
        assert!(parts[1].contains_rect(row.rect));
        assert_eq!(h.click(row.rect.center()), vec![last.command.unwrap()]);
        // Drag the thumb back through its own gutter; the pinned row stays put.
        let start = pos2(380.0, parts[1].bottom() - 8.0);
        h.frame(vec![Event::PointerMoved(start)]);
        h.frame(vec![press(start, true)]);
        let top = pos2(start.x, parts[1].top() + 45.0);
        h.frame(vec![Event::PointerMoved(top)]);
        h.frame(vec![press(top, false)]);
        let back = h.settle();
        assert_eq!(get(&back, &h.view.list.this_pc.name).rect, pc);
        assert!(back
            .out
            .controls
            .iter()
            .any(|c| c.name == h.view.list.rows[0].spoken));
        h.cap = 300.0;
        assert_eq!(h.settle().out.parts[1].height(), 152.0);
    }
}
#[test]
fn tab_reaches_an_action_and_space_and_enter_activate_it() {
    for key in [Key::Space, Key::Enter] {
        let mut h = named("kitchen-playing", 1.0);
        let f = h.settle();
        let keys = |key| {
            [true, false].map(|pressed| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            })
        };
        h.frame(Vec::from(keys(Key::Tab)));
        let a = h.frame(Vec::from(keys(key))).out.commands;
        assert_eq!(a, vec![f.out.controls[0].command.clone().unwrap()]);
    }
}
#[test]
fn tab_visits_the_enabled_controls_in_reading_order() {
    each(|name, h| {
        if name == "many-speakers" {
            h.cap = 640.0;
        }
        let order = expected(&h.view, h.state.more_open, true);
        h.settle();
        for (label, _, _) in order.into_iter().filter(|(_, enabled, _)| *enabled) {
            let key = |pressed| Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            };
            let mut focused = h.frame(vec![key(true)]).focused;
            focused.extend(h.frame(vec![key(false)]).focused);
            assert_eq!(focused, [label], "{name}");
        }
    });
}
#[test]
fn a_long_question_grows_and_both_answers_stay_on_one_line() {
    for scale in SCALES {
        let mut h = named("question", scale);
        let About::Question(q) = &mut h.view.about else {
            panic!()
        };
        q.sentence = q.sentence.repeat(9);
        q.yes.label = q.yes.label.repeat(8);
        q.no.label = q.no.label.repeat(48);
        let f = h.settle();
        clean(&f, &h.view, "long question", true);
        let b = buttons(&h.view, &f);
        assert!(f.out.parts[2].height() > 192.0);
        assert_eq!(b[0].top(), b[1].top());
        assert!((b[1].right() - (365.0 * scale).round() / scale).abs() < 0.001);
        for r in b {
            assert!(f
                .texts
                .iter()
                .any(|t| r.contains(t.rect.center()) && t.cut && t.rows.len() == 1));
        }
    }
}

fn key_event(key: Key, modifiers: Modifiers, pressed: bool) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    }
}
impl Window {
    fn key(&mut self, key: Key, modifiers: Modifiers) -> Frame {
        self.frame(vec![
            key_event(key, modifiers, true),
            key_event(key, modifiers, false),
        ])
    }
    fn at(&self, f: &Frame, name: &str) {
        let id = self.ctx.memory(|m| m.focused()).expect("keyboard focus");
        let response = self.ctx.read_response(id).unwrap();
        assert_eq!(
            response.rect,
            get(f, name).rect,
            "focus should be on {name}"
        );
    }
    fn tab_to(&mut self, name: &str, modifiers: Modifiers) {
        self.key(Key::Tab, modifiers);
        let after = self.frame(vec![]);
        self.at(&after, name);
    }
}

#[test]
fn row_navigation_type_ahead_and_activation() {
    let mut h = named("twins", 1.0);
    h.settle();
    let v = h.view.place.volume.clone().unwrap();
    let rows = h.view.list.rows.clone();
    h.tab_to(&v.mute_spoken, Modifiers::NONE);
    h.tab_to(&v.spoken, Modifiers::NONE);
    h.tab_to(&rows[2].spoken, Modifiers::NONE);
    for (key, name) in [
        (Key::ArrowDown, &rows[3].spoken),
        (Key::End, &rows.last().unwrap().spoken),
        (Key::ArrowDown, &rows.last().unwrap().spoken),
        (Key::Home, &h.view.list.this_pc.name.clone()),
    ] {
        let f = h.key(key, Modifiers::NONE);
        h.at(&f, name);
    }
    for key in [Key::ArrowUp, Key::ArrowRight, Key::ArrowLeft] {
        let f = h.key(key, Modifiers::NONE);
        h.at(&f, &h.view.list.this_pc.name);
    }
    for text in ["o", "f"] {
        let f = h.frame(vec![Event::Text(text.into())]);
        h.at(&f, "Office");
    }
    let f = h.key(Key::Enter, Modifiers::NONE);
    assert_eq!(f.out.commands, [rows[4].command.clone().unwrap()]);
    h.at(&f, "Office");
    h.frame += 20; // start a new prefix after a second
    let f = h.key(Key::Home, Modifiers::NONE);
    h.at(&f, &h.view.list.this_pc.name);
    for r in [&rows[2], rows.last().unwrap()] {
        let f = h.frame(vec![Event::Text("k".into())]);
        h.at(&f, &r.spoken);
    }
    h.tab_to(&v.spoken, Modifiers::SHIFT);
    h.tab_to(&rows.last().unwrap().spoken, Modifiers::NONE);

    let mut h = named("fold-closed", 1.0);
    let fold = h.view.list.more.clone().unwrap();
    h.state.focus_place();
    h.settle();
    let f = h.key(Key::End, Modifiers::NONE);
    h.at(&f, &fold.label);
    for (key, open) in [(Key::Enter, true), (Key::Space, false)] {
        assert!(h.key(key, Modifiers::NONE).out.commands.is_empty());
        assert_eq!(h.state.more_open, open);
        let f = h.frame(vec![]);
        h.at(&f, &fold.label);
        assert_eq!(
            f.out.controls.iter().any(|c| c.name == fold.rows[0].spoken),
            open
        );
    }
}

#[test]
fn tab_visits_only_the_focused_rows_actions_in_both_directions() {
    let mut h = named("kitchen-playing", 1.25);
    h.state.focus_place();
    h.settle();
    let garage = h.view.list.rows[1].clone();
    let f = h.key(Key::ArrowUp, Modifiers::NONE);
    h.at(&f, &garage.spoken);
    let why = spoken(garage.link.as_ref().unwrap());
    let About::Delay(DelayControls {
        head: DelayHead::Ways { lines, .. },
        ..
    }) = &h.view.about
    else {
        panic!()
    };
    let airplay = lines[1].spoken.clone();
    for name in [why, &garage.gear.label, &airplay] {
        h.tab_to(name, Modifiers::NONE);
    }
    for name in [&garage.gear.label, why, &garage.spoken] {
        h.tab_to(name, Modifiers::SHIFT);
    }
    h.tab_to(
        &h.view.place.volume.clone().unwrap().spoken,
        Modifiers::SHIFT,
    );
}

#[test]
fn focus_place_and_scroll_the_keyboard_row_into_view() {
    let mut h = named("many-speakers", 1.75);
    h.cap = 640.0;
    h.state.focus_place();
    let f = h.frame(vec![]);
    h.at(&f, &h.view.list.this_pc.name);
    let last = &h.view.list.rows.last().unwrap().spoken.clone();
    assert!(f.out.controls.iter().all(|c| &c.name != last));
    h.key(Key::End, Modifiers::NONE);
    let f = h.settle();
    h.at(&f, last);
    assert!(f.out.parts[1].contains_rect(get(&f, last).rect));
    h.state.focus_place();
    let f = h.frame(vec![]);
    h.at(&f, &h.view.list.this_pc.name);

    let mut h = named("pc-outputs-open", 1.0);
    h.state.focus_place();
    h.settle();
    for o in h.view.list.this_pc.outputs.clone() {
        let f = h.key(Key::ArrowDown, Modifiers::NONE);
        h.at(&f, &o.name);
        let f = h.key(Key::Space, Modifiers::NONE);
        assert_eq!(f.out.commands, [o.command]);
        h.at(&f, &o.name);
    }
    h.view.list.this_pc.marked = false;
    h.state.focus_place();
    let f = h.frame(vec![]);
    h.at(&f, &h.view.list.this_pc.name);
}

#[test]
fn track_press_thumb_grab_and_pending_readout() {
    for scale in SCALES {
        let mut h = named("kitchen-playing", scale);
        let f = h.settle();
        let v = h.view.place.volume.clone().unwrap();
        let track = get(&f, &v.spoken).rect;
        let at = |value: u8| {
            pos2(
                ((46.0 + 2.88 * f32::from(value)) * scale).round() / scale,
                track.center().y,
            )
        };
        let cmd = |level| Command::SetVolume {
            owner: v.owner.clone().unwrap(),
            level,
        };
        let check_number = |f: &Frame, value: u8| {
            let number = f
                .texts
                .iter()
                .find(|t| {
                    t.rect.left() > track.right() && t.rect.y_range().contains(track.center().y)
                })
                .unwrap();
            assert_eq!(number.words, value.to_string());
        };
        assert!(h.click(at(v.level)).is_empty());
        let p = at(70);
        h.frame(vec![Event::PointerMoved(p)]);
        let f = h.frame(vec![press(p, true)]);
        assert_eq!(f.out.commands, [cmd(70)]);
        check_number(&f, 70);
        let f = h.frame(vec![press(p, false)]);
        assert!(f.out.commands.is_empty());
        check_number(&f, 70);
        let grab = p + vec2(3.0, 0.0);
        h.frame(vec![Event::PointerMoved(grab)]);
        assert!(h.frame(vec![press(grab, true)]).out.commands.is_empty());
        for (delta, level) in [(20.0, 77), (40.0, 84)] {
            let f = h.frame(vec![Event::PointerMoved(grab + vec2(delta, 0.0))]);
            assert_eq!(f.out.commands, [cmd(level)]);
            check_number(&f, level);
            assert!(f.circles.contains(&at(level)));
        }
        let f = h.frame(vec![press(grab + vec2(40.0, 0.0), false)]);
        assert!(f.out.commands.is_empty());
        check_number(&f, 84);
        for value in [70, 84, 28] {
            let v = h.view.place.volume.as_mut().unwrap();
            v.level = value;
            v.number = value.to_string();
            let f = h.frame(vec![]);
            check_number(&f, if value == 70 { 84 } else { value });
        }
        for (dimmed, owner) in [(false, None), (true, v.owner.clone())] {
            let v = h.view.place.volume.as_mut().unwrap();
            v.owner = owner;
            v.dimmed = dimmed;
            h.settle();
            assert!(h.click(p).is_empty());
        }
    }
}

#[test]
fn pointing_at_offices_words_leaves_its_gear_at_rest() {
    let mut h = named("kitchen-playing", 1.0);
    let f = h.settle();
    let gear = get(&f, "Office’s settings: tone, delay, name").rect;
    let row = get(&f, "Office").rect;
    let shapes_inside = |f: &Frame| {
        f.shapes
            .iter()
            .filter(|s| {
                gear.expand(3.0).contains_rect(s.visual_bounding_rect())
                    && s.visual_bounding_rect().is_positive()
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let rest = shapes_inside(&f);
    assert!(rest.iter().any(|s| matches!(s, Shape::Path(_))));
    let pointed = h.frame(vec![Event::PointerMoved(pos2(54.0, row.center().y))]);
    assert!(pointed
        .shapes
        .iter()
        .any(|s| matches!(s, Shape::Rect(r) if r.rect == row)));
    assert_eq!(shapes_inside(&pointed), rest);
    let pointed = h.frame(vec![Event::PointerMoved(gear.center())]);
    assert_ne!(shapes_inside(&pointed), rest);
    assert!(pointed
        .shapes
        .iter()
        .any(|s| matches!(s, Shape::Rect(r) if r.rect == gear)));
}
