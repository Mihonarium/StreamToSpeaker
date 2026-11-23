//! The Settings window and its dialogs: setting up, pairing, tuning a
//! speaker and repair, in a window a person opens only when they need it.
//!
//! Navigation on the left (Speakers, General, Music Assistant, Privacy &
//! remote, Help, About, and Lab when it is turned on), pages of cards on
//! the right, in Windows 11's Settings shape. A change takes effect at once
//! and is kept: there is no Apply button, and a change that needs a
//! reconnect says so and does it.
//!
//! The window's contents are drawn into a window somebody else opened:
//! [`Window::show`] draws the whole window into its `Context`, each
//! [`Dialog`] draws one dialog into its own. They read and change the app
//! only through a [`SettingsPort`]; what they ask of the window around them
//! (open a dialog, open the panel) comes back as a [`Request`].

pub mod port;
pub mod real;
pub mod scripted;
pub(crate) mod words;

mod about;
mod dialogs;
mod general;
mod help;
mod lab;
mod music_assistant;
mod privacy;
mod qr;
mod scenes;
mod speaker;
mod speakers;
mod widgets;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use egui::{Key, Rect, Sense, Ui, Vec2, WidgetInfo, WidgetType};

pub use dialogs::{AddDialog, CodeDialog, Dialog, PasswordDialog, ReportDialog, SendspinDialog, ToneDialog};
pub use port::{Act, SettingsPort};
pub use scenes::scenes;

use crate::model::text::Lang;
use crate::ui::delay::DelayState;
use crate::ui::icon::{paint_mark, Icon};
use crate::ui::look::look_as;
use crate::ui::theme::{focus_ring, size, Theme};
use words::w;

/// The window's size when it opens, and the least it can be made: it fits
/// a 1366 × 768 laptop.
pub const DEFAULT_SIZE: [f32; 2] = [880.0, 640.0];
pub const MIN_SIZE: [f32; 2] = [640.0, 480.0];
/// Narrower than this, the navigation shows only its icons.
const NARROW: f32 = 800.0;
const NAV_W: f32 = 232.0;
const NAV_W_NARROW: f32 = 52.0;
/// The pages' cards are no wider than this, however wide the window.
const CONTENT_MAX: f32 = 1000.0;

/// The window's title, for the window around it.
pub fn title(lang: Lang) -> String {
    w!(lang, SetWindowTitle)
}

/// A page of the window.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Speakers,
    /// One speaker's page, by id.
    Speaker(String),
    General,
    MusicAssistant,
    Privacy,
    Help,
    /// *How it works*, on its first or second screen.
    Guide(u8),
    /// The playback check: *Can't hear it?*
    CantHear,
    /// The discovery check: *Can't find your speaker?*
    CantFind,
    About,
    Lab,
}

impl Page {
    /// The page of the navigation this page belongs to.
    fn section(&self) -> Section {
        match self {
            Page::Speakers | Page::Speaker(_) => Section::Speakers,
            Page::General => Section::General,
            Page::MusicAssistant => Section::MusicAssistant,
            Page::Privacy => Section::Privacy,
            Page::Help | Page::Guide(_) | Page::CantHear | Page::CantFind => Section::Help,
            Page::About => Section::About,
            Page::Lab => Section::Lab,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Speakers,
    General,
    MusicAssistant,
    Privacy,
    Help,
    About,
    Lab,
}

impl Section {
    fn page(self) -> Page {
        match self {
            Section::Speakers => Page::Speakers,
            Section::General => Page::General,
            Section::MusicAssistant => Page::MusicAssistant,
            Section::Privacy => Page::Privacy,
            Section::Help => Page::Help,
            Section::About => Page::About,
            Section::Lab => Page::Lab,
        }
    }

    fn icon(self) -> Icon {
        match self {
            Section::Speakers => Icon::Speaker,
            Section::General => Icon::Gear,
            Section::MusicAssistant => Icon::Hub,
            Section::Privacy => Icon::Shield,
            Section::Help => Icon::Help,
            Section::About => Icon::Info,
            Section::Lab => Icon::Flask,
        }
    }

    fn name(self, lang: Lang) -> String {
        match self {
            Section::Speakers => w!(lang, SetNavSpeakers),
            Section::General => w!(lang, SetNavGeneral),
            Section::MusicAssistant => w!(lang, MaName),
            Section::Privacy => w!(lang, SetNavPrivacy),
            Section::Help => w!(lang, SetNavHelp),
            Section::About => w!(lang, SetNavAbout),
            Section::Lab => w!(lang, SetNavLab),
        }
    }
}

/// What the window asks of the window around it.
#[derive(Debug)]
pub enum Request {
    /// Open this dialog (a small window of its own, centred on the
    /// screen). A second dialog waits until the open one is closed.
    Dialog(Dialog),
    /// Open the panel by the clock.
    OpenPanel,
}

/// The Settings window: which page shows, and what each page remembers
/// while the window is open.
#[derive(Debug, Default)]
pub struct Window {
    page: Option<Page>,
    /// Pages to go back to.
    back: Vec<Page>,
    pub(crate) st: PageState,
}

/// What pages remember between frames: open folds, text being typed, the
/// delay control's field.
#[derive(Debug, Default)]
pub(crate) struct PageState {
    pub delay: HashMap<String, DelayState>,
    /// Folds open (*Details*, …), by a name of their own.
    pub open: HashSet<String>,
    /// Text being typed, by field, until it is kept.
    pub typing: HashMap<String, String>,
    /// A shortcut whose keys are being recorded.
    pub recording: Option<usize>,
    /// What was just copied, and when (the button says *Copied*).
    pub copied: Option<(String, f64)>,
    /// The pairing token is shown in clear.
    pub token_shown: bool,
}

impl PageState {
    pub fn is_open(&self, fold: &str) -> bool {
        self.open.contains(fold)
    }

    pub fn toggle(&mut self, fold: &str) {
        if !self.open.remove(fold) {
            self.open.insert(fold.to_string());
        }
    }
}

/// What a page has to hand while it is drawn.
pub(crate) struct Cx<'p> {
    pub port: &'p dyn SettingsPort,
    pub lang: Lang,
    /// A page to go to after this frame.
    pub go: Option<Page>,
    /// Go back after this frame.
    pub back: bool,
    pub requests: Vec<Request>,
}

impl Cx<'_> {
    pub fn act(&self, act: Act) {
        self.port.act(act);
    }

    pub fn go(&mut self, page: Page) {
        self.go = Some(page);
    }

    pub fn dialog(&mut self, d: Dialog) {
        self.requests.push(Request::Dialog(d));
    }
}

impl Window {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open on `page` (*Settings* opens on Speakers, *Can't hear it?* on
    /// the playback check, *Pair…* on the speaker's page).
    pub fn open(&mut self, page: Page) {
        let current = self.page();
        if current != page {
            self.back.push(current);
        }
        self.page = Some(page);
    }

    pub fn page(&self) -> Page {
        self.page.clone().unwrap_or(Page::Speakers)
    }

    /// Draw the whole window into `ctx`, the context of the window opened
    /// for it.
    pub fn show(&mut self, ctx: &egui::Context, port: &dyn SettingsPort) -> Vec<Request> {
        let t = Theme::of(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(t.c.surface))
            .show(ctx, |ui| self.show_in(ui, port))
            .inner
    }

    /// Draw the window in `ui`, which has the window's whole size.
    pub fn show_in(&mut self, ui: &mut Ui, port: &dyn SettingsPort) -> Vec<Request> {
        let lang = port.lang();
        let header = port.header();
        let mut cx = Cx { port, lang, go: None, back: false, requests: Vec::new() };
        // Lab can disappear while it shows (turned off in About).
        if !header.lab && self.page() == Page::Lab {
            self.page = Some(Page::About);
        }
        let full = ui.max_rect();
        let narrow = full.width() < NARROW;
        let nav_w = if narrow { NAV_W_NARROW } else { NAV_W };
        let nav_rect = Rect::from_min_size(full.min, Vec2::new(nav_w, full.height()));
        let content_rect = Rect::from_min_max(egui::pos2(full.left() + nav_w, full.top()), full.max);
        ui.allocate_rect(full, Sense::hover());

        let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        self.nav(&mut nav_ui, &mut cx, &header.status, header.lab, narrow);

        let mut content_ui = ui.new_child(egui::UiBuilder::new().max_rect(content_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        let page = self.page();
        egui::ScrollArea::vertical()
            .id_salt(("settings-page", page_key(&page)))
            .auto_shrink([false, false])
            .show(&mut content_ui, |ui| {
                let pad = egui::Margin { left: if narrow { 16.0 } else { 8.0 }, right: 28.0, top: 4.0, bottom: 20.0 };
                egui::Frame::none().inner_margin(pad).show(ui, |ui| {
                    let w = ui.available_width().min(CONTENT_MAX);
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 4.0;
                    self.page_body(ui, &mut cx, &page);
                });
            });

        // Back: Alt+← or the mouse's back button, as Windows' Settings.
        let back_key = ui.input(|i| {
            let alt_left = i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::ArrowLeft, pressed: true, modifiers, .. } if modifiers.alt));
            alt_left || i.pointer.button_pressed(egui::PointerButton::Extra1)
        });
        if back_key || cx.back {
            self.go_back();
        } else if let Some(p) = cx.go.take() {
            self.open(p);
        }
        cx.requests
    }

    fn go_back(&mut self) {
        let here = self.page();
        while let Some(p) = self.back.pop() {
            if p != here {
                self.page = Some(p);
                return;
            }
        }
        // Nothing to go back to: a sub-page goes to its section.
        let up = here.section().page();
        if up != here {
            self.page = Some(up);
        }
    }

    fn page_body(&mut self, ui: &mut Ui, cx: &mut Cx, page: &Page) {
        match page {
            Page::Speakers => speakers::page(ui, cx, &mut self.st),
            Page::Speaker(id) => speaker::page(ui, cx, &mut self.st, id),
            Page::General => general::page(ui, cx, &mut self.st),
            Page::MusicAssistant => music_assistant::page(ui, cx, &mut self.st),
            Page::Privacy => privacy::page(ui, cx, &mut self.st),
            Page::Help => help::page(ui, cx),
            Page::Guide(n) => help::guide(ui, cx, *n),
            Page::CantHear => help::cant_hear(ui, cx),
            Page::CantFind => help::cant_find(ui, cx),
            Page::About => about::page(ui, cx, &mut self.st),
            Page::Lab => lab::page(ui, cx, &mut self.st),
        }
    }

    /// The navigation: the app's mark, name and state, then the pages.
    fn nav(&mut self, ui: &mut Ui, cx: &mut Cx, status: &str, lab: bool, narrow: bool) {
        let t = Theme::of(ui.ctx());
        let lang = cx.lang;
        let width = ui.available_width();
        ui.spacing_mut().item_spacing.y = 4.0;
        // The app: mark, name, the state sentence.
        let (head, _) = ui.allocate_exact_size(Vec2::new(width, 62.0), Sense::hover());
        let mark = Rect::from_min_size(head.min + Vec2::new(if narrow { 11.0 } else { 18.0 }, 16.0), Vec2::splat(30.0));
        paint_mark(ui.painter(), mark, t.c.mark_ink, t.c.live, false);
        if !narrow {
            let text_x = mark.right() + 10.0;
            let text_w = head.right() - 12.0 - text_x;
            let name = ui.painter().layout(w!(lang, AppName), t.strong(size::BODY), t.c.text, text_w);
            let state = ui.painter().layout(status.to_owned(), t.font(size::CAPTION), t.c.text_secondary, text_w);
            let top = mark.center().y - (name.size().y + state.size().y) / 2.0;
            let name_h = name.size().y;
            ui.painter().galley(egui::pos2(text_x, top), name, t.c.text);
            ui.painter().galley(egui::pos2(text_x, top + name_h), state, t.c.text_secondary);
        }
        let mut sections = vec![Section::Speakers, Section::General, Section::MusicAssistant, Section::Privacy, Section::Help, Section::About];
        if lab {
            sections.push(Section::Lab);
        }
        let current = self.page().section();
        let mut responses = Vec::new();
        for s in &sections {
            let r = nav_item(ui, *s, *s == current, narrow, lang);
            if r.clicked() {
                cx.go(s.page());
            }
            responses.push(r);
        }
        // ↑ and ↓ move between the pages while one has the focus.
        if let Some(i) = responses.iter().position(|r| r.has_focus()) {
            let (up, down) = ui.input(|inp| (inp.key_pressed(Key::ArrowUp), inp.key_pressed(Key::ArrowDown)));
            let to = if up && i > 0 {
                Some(i - 1)
            } else if down && i + 1 < responses.len() {
                Some(i + 1)
            } else {
                None
            };
            if let Some(to) = to {
                responses[to].request_focus();
                cx.go(sections[to].page());
            }
        }
    }
}

/// One page in the navigation: a pill with the icon and the name, the
/// current one with a hover tint and an accent bar.
fn nav_item(ui: &mut Ui, s: Section, current: bool, narrow: bool, lang: Lang) -> egui::Response {
    let t = Theme::of(ui.ctx());
    let name = s.name(lang);
    let width = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, current, &name));
    let rect = Rect::from_min_max(outer.min + Vec2::new(6.0, 0.0), outer.max - Vec2::new(6.0, 0.0));
    let l = look_as(ui, &resp, &name);
    let p = ui.painter();
    let fill = if l.pressed {
        Some(t.c.pressed)
    } else if current || l.hovered {
        Some(t.c.hover)
    } else {
        None
    };
    if let Some(f) = fill {
        p.rect_filled(rect, 5.0, f);
    }
    if current {
        let bar = Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 9.0), egui::pos2(rect.left() + 3.0, rect.bottom() - 9.0));
        p.rect_filled(bar, 1.5, t.c.accent);
    }
    let icon = Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(16.0));
    s.icon().paint(p, icon, t.c.text);
    if !narrow {
        let g = p.layout_no_wrap(name.clone(), t.font(size::CONTROL), t.c.text);
        p.galley(egui::pos2(icon.right() + 12.0, rect.center().y - g.size().y / 2.0), g, t.c.text);
    }
    if l.focused {
        focus_ring(p, rect, 5.0, false, &t);
    }
    if narrow {
        resp.on_hover_text(name)
    } else {
        resp
    }
}

fn page_key(p: &Page) -> String {
    format!("{p:?}")
}
