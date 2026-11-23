//! The dialogs: small windows of their own, one task each. Codes and
//! passwords a speaker asks for, Music Assistant's code, adding a speaker
//! by address, a speaker's tone, and the report.
//!
//! No dialog shows an example of a code, password or token people copy
//! from another device: fields start empty, and the TV's drawing has
//! empty slots.

use egui::{Color32, Key, Rect, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType};

use crate::equalizer::{EQ_BAND_HZ, EQ_PRESETS};
use crate::model::text::Lang;
use crate::ui::button::Button;
use crate::ui::icon::{paint_spinner, Icon};
use crate::ui::inputs::{Check, Choice, DropDown, TextField};
use crate::ui::look::look_as;
use crate::ui::notice::{info_bar, BarKind, InfoBar};
use crate::ui::text::{Mark, Para, Tone};
use crate::ui::theme::{focus_ring, radius, size, Theme};

use super::port::*;
use super::speakers::kind_icon;
use super::widgets::{body, buttons, mono, note, note_links};
use super::words::{self, w};
use super::PageState;

/// A dialog, with what it remembers while open.
#[derive(Debug)]
pub enum Dialog {
    Code(CodeDialog),
    Password(PasswordDialog),
    Sendspin(SendspinDialog),
    MaCode(MaCodeDialog),
    AddAddress(AddDialog),
    Tone(ToneDialog),
    Report(ReportDialog),
}

/// The space between a dialog's edge and its contents.
const PAD_X: f32 = 24.0;

impl Dialog {
    /// The dialog the app needs open now, if any (a speaker asks for its
    /// code or password while connecting; Music Assistant is pairing).
    pub fn wanted(port: &dyn SettingsPort) -> Option<Dialog> {
        Some(match port.wanted_dialog()? {
            Wanted::PairingCode(a) => Dialog::Code(CodeDialog { id: a.id, ..Default::default() }),
            Wanted::Password(a) => Dialog::Password(PasswordDialog { id: a.id, name: a.name, asked: true, connect: true, ..Default::default() }),
            Wanted::SendspinCode(a) => Dialog::Sendspin(SendspinDialog { id: a.id, ..Default::default() }),
            Wanted::MaCode(_) => Dialog::MaCode(MaCodeDialog::default()),
        })
    }

    /// The window title (the dialog's own title is inside it).
    pub fn window_title(lang: Lang) -> String {
        w!(lang, AppName)
    }

    /// The dialog's size in points.
    pub fn size(&self) -> [f32; 2] {
        match self {
            Dialog::Code(_) => [720.0, 400.0],
            Dialog::Password(_) => [520.0, 320.0],
            Dialog::Sendspin(_) => [560.0, 300.0],
            Dialog::MaCode(_) => [600.0, 340.0],
            Dialog::AddAddress(_) => [680.0, 420.0],
            Dialog::Tone(_) => [760.0, 470.0],
            Dialog::Report(_) => [840.0, 560.0],
        }
    }

    /// Draw the dialog into `ctx`, the context of the window opened for
    /// it. False once it should close.
    pub fn show(&mut self, ctx: &egui::Context, port: &dyn SettingsPort) -> bool {
        let t = Theme::of(ctx);
        egui::CentralPanel::default().frame(egui::Frame::none().fill(t.c.surface)).show(ctx, |ui| self.show_in(ui, port)).inner
    }

    /// Draw the dialog in `ui`, which has the dialog's whole size. False
    /// once it should close (done, cancelled, or no longer needed).
    pub fn show_in(&mut self, ui: &mut Ui, port: &dyn SettingsPort) -> bool {
        let lang = port.lang();
        let esc = ui.input(|i| i.key_pressed(Key::Escape));
        let keep = match self {
            Dialog::Code(d) => d.show(ui, port, lang),
            Dialog::Password(d) => d.show(ui, port, lang),
            Dialog::Sendspin(d) => d.show(ui, port, lang),
            Dialog::MaCode(d) => d.show(ui, port, lang),
            Dialog::AddAddress(d) => d.show(ui, port, lang),
            Dialog::Tone(d) => d.show(ui, port, lang),
            Dialog::Report(d) => d.show(ui, port, lang),
        };
        if esc && keep {
            self.cancel(port);
            return false;
        }
        keep
    }

    /// What closing the dialog with its × or Esc does.
    pub fn cancel(&mut self, port: &dyn SettingsPort) {
        match self {
            Dialog::Code(d) => port.act(Act::CancelPairing(d.id.clone())),
            Dialog::Password(d) if d.asked => port.act(Act::CancelPassword(d.id.clone())),
            Dialog::Sendspin(d) => port.act(Act::CancelSendspin(d.id.clone())),
            Dialog::MaCode(_) => port.act(Act::CancelMaPairing),
            Dialog::AddAddress(_) => port.act(Act::ResetProbe),
            _ => {}
        }
    }
}

/// Draw a dialog's body, padded; returns the footer to draw its buttons
/// in, a band of the card colour along the bottom.
fn frame(ui: &mut Ui, body: impl FnOnce(&mut Ui)) -> Ui {
    let t = Theme::of(ui.ctx());
    let full = ui.max_rect();
    let foot_h = 72.0;
    let body_rect = Rect::from_min_max(full.min, egui::pos2(full.right(), full.bottom() - foot_h));
    let foot_rect = Rect::from_min_max(egui::pos2(full.left(), full.bottom() - foot_h), full.max);
    ui.allocate_rect(full, Sense::hover());
    let mut b = ui.new_child(egui::UiBuilder::new().max_rect(body_rect.shrink2(Vec2::new(PAD_X, 0.0))).layout(egui::Layout::top_down(egui::Align::Min)));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut b, |ui| {
        ui.add_space(20.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        body(ui);
        ui.add_space(12.0);
    });
    ui.painter().rect_filled(foot_rect, 0.0, t.c.card);
    ui.painter().rect_filled(Rect::from_min_size(foot_rect.min, Vec2::new(foot_rect.width(), 1.0)), 0.0, t.c.divider);
    let mut f = ui.new_child(egui::UiBuilder::new().max_rect(foot_rect.shrink2(Vec2::new(PAD_X, 20.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    f.spacing_mut().item_spacing.x = 8.0;
    f
}

fn title(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.add(egui::Label::new(egui::RichText::new(text).font(t.strong(size::DIALOG)).color(t.c.text)).wrap());
}

/// Buttons at the right end of the footer, in reading order.
fn at_right<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add).inner
}

/// An error bar in a dialog.
fn error(ui: &mut Ui, text: &str) {
    let _ = info_bar(ui, &InfoBar { kind: BarKind::Error, title: None, text, action: None });
}

/// A small caps label over a column (*ON YOUR APPLE TV*).
fn column_label(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.label(egui::RichText::new(text.to_uppercase()).font(t.strong(size::CAPTION)).color(t.c.text_secondary).extra_letter_spacing(0.8));
}

// -------------------------------------------------------------------------
// A code shown on a TV
// -------------------------------------------------------------------------

/// *Pair with Den TV*: the TV shows a 4-digit code; type it here.
#[derive(Debug, Default)]
pub struct CodeDialog {
    pub id: String,
    typed: String,
    /// The stage last seen, to clear the boxes when a code was wrong.
    last: Option<CodeStage>,
    focus: bool,
}

impl CodeDialog {
    pub fn new(id: &str) -> Self {
        CodeDialog { id: id.into(), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let ask = match port.wanted_dialog() {
            Some(Wanted::PairingCode(a)) if a.id == self.id => a,
            // Paired (or given up elsewhere): the connection goes on.
            _ => return false,
        };
        if self.last != Some(ask.stage) {
            if ask.stage == CodeStage::Wrong || ask.stage == CodeStage::Showing {
                self.typed.clear();
                self.focus = true;
            }
            self.last = Some(ask.stage);
        }
        let mut keep = true;
        let name = ask.name.clone();
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetPairWith, speaker = &name));
                ui.add_space(4.0);
                ui.spacing_mut().item_spacing.x = 24.0;
                ui.columns(2, |cols| {
                    {
                        let ui = &mut cols[0];
                        egui::Frame::none().fill(super::widgets::tint(ui)).rounding(radius::CARD).inner_margin(16.0).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.set_min_height(220.0);
                            column_label(ui, &w!(lang, SetOnYourTv));
                            ui.add_space(6.0);
                            tv_drawing(ui, ui.available_width().min(200.0));
                            ui.add_space(6.0);
                            body(ui, &w!(lang, SetLookAtTv, speaker = &name));
                        });
                    }
                    {
                        let ui = &mut cols[1];
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 8.0;
                            ui.add_space(16.0);
                            column_label(ui, &w!(lang, SetOnThisPc));
                            match ask.stage {
                                CodeStage::Starting => busy(ui, &w!(lang, SetAskingForCode, speaker = &name)),
                                CodeStage::Checking => busy(ui, &w!(lang, SetCheckingCode)),
                                CodeStage::NoAnswer => {
                                    error(ui, &w!(lang, SetNoAnswerPairing, speaker = &name));
                                    if Button::standard(&w!(lang, SetTryAgain)).show(ui).clicked() {
                                        port.act(Act::NewCode(self.id.clone()));
                                    }
                                }
                                CodeStage::TimedOut => {
                                    error(ui, &w!(lang, SetTimeRanOut));
                                    if Button::primary(&w!(lang, SetNewCode)).show(ui).clicked() {
                                        port.act(Act::NewCode(self.id.clone()));
                                    }
                                }
                                CodeStage::Showing | CodeStage::Wrong => {
                                    if ask.stage == CodeStage::Wrong {
                                        error(ui, &w!(lang, SetCodeWrong, speaker = &name));
                                    }
                                    body(ui, &w!(lang, SetTypeCode, speaker = &name));
                                    if let Some(code) = digit_boxes(ui, &mut self.typed, 4, &mut self.focus, &w!(lang, SetCodeName)) {
                                        port.act(Act::SubmitCode { id: self.id.clone(), code });
                                    }
                                    let left = ask.seconds_left.map(|s| words::left(lang, s));
                                    let line = match left {
                                        Some(l) => w!(lang, SetGoesInBy, left = &l),
                                        None => w!(lang, SetGoesIn),
                                    };
                                    if note_links(ui, &line, &[&w!(lang, SetNewCode)]).is_some() {
                                        port.act(Act::NewCode(self.id.clone()));
                                    }
                                }
                            }
                        });
                    }
                });
        });
        {
            let ui = &mut foot;
                note(ui, &w!(lang, SetPairOnce));
                at_right(ui, |ui| {
                    if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                        port.act(Act::CancelPairing(self.id.clone()));
                        keep = false;
                    }
                });
        }
        keep
    }
}

/// A TV drawn with four empty slots on its screen.
fn tv_drawing(ui: &mut Ui, width: f32) {
    let h = width * 0.6;
    let (r, _) = ui.allocate_exact_size(Vec2::new(width, h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, 8.0, Color32::from_rgb(0x2a, 0x2a, 0x2a));
    let screen = r.shrink(6.0);
    p.rect_filled(screen, 5.0, Color32::from_rgb(0x12, 0x12, 0x12));
    let slot = Vec2::new(width * 0.14, h * 0.32);
    let gap = width * 0.05;
    let total = slot.x * 4.0 + gap * 3.0;
    for i in 0..4 {
        let x = screen.center().x - total / 2.0 + i as f32 * (slot.x + gap);
        let s = Rect::from_min_size(egui::pos2(x, screen.center().y - slot.y / 2.0), slot);
        p.rect_stroke(s, 4.0, Stroke::new(1.5_f32, Color32::from_gray(0x8a)));
    }
}

/// A turning ring and words: something is happening.
fn busy(ui: &mut Ui, text: &str) {
    let t = Theme::of(ui.ctx());
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
        paint_spinner(ui.painter(), r, t.c.accent, ui.input(|i| i.time));
        ui.label(egui::RichText::new(text).font(t.font(size::BODY)).color(t.c.text));
    });
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
}

/// `count` boxes, one digit each, filled as the person types; the code
/// when the last box is filled.
fn digit_boxes(ui: &mut Ui, typed: &mut String, count: usize, focus: &mut bool, name: &str) -> Option<String> {
    let t = Theme::of(ui.ctx());
    let side = 52.0;
    let gap = 12.0;
    let width = count as f32 * side + (count - 1) as f32 * gap;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, side + 6.0), Sense::hover());
    // One field under the boxes takes the keys.
    let edit = egui::TextEdit::singleline(typed).char_limit(count).frame(false).text_color(Color32::TRANSPARENT).desired_width(width);
    let resp = super::super::ui::inside(ui, rect, |ui| ui.add_sized(rect.size(), edit));
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, name));
    if *focus {
        resp.request_focus();
        *focus = false;
    }
    typed.retain(|c| c.is_ascii_digit());
    let l = look_as(ui, &resp, name);
    let p = ui.painter();
    let digits: Vec<char> = typed.chars().collect();
    for i in 0..count {
        let b = Rect::from_min_size(egui::pos2(rect.left() + i as f32 * (side + gap), rect.top()), Vec2::new(side, side));
        p.rect_filled(b, radius::CONTROL, t.c.control);
        p.rect_stroke(b.shrink(0.5), radius::CONTROL, Stroke::new(1.0_f32, t.c.control_border));
        let active = l.focused && i == digits.len().min(count - 1);
        let edge = if active { t.c.accent } else { t.c.control_strong };
        p.rect_filled(Rect::from_min_max(egui::pos2(b.left(), b.bottom() - if active { 2.0 } else { 1.0 }), b.max), 0.0, edge);
        if let Some(c) = digits.get(i) {
            let g = p.layout_no_wrap(c.to_string(), t.strong(24.0), t.c.text);
            p.galley(b.center() - g.size() / 2.0, g, t.c.text);
        }
    }
    if digits.len() == count {
        let code: String = digits.iter().collect();
        typed.clear();
        return Some(code);
    }
    None
}

// -------------------------------------------------------------------------
// An AirPlay password
// -------------------------------------------------------------------------

/// *Password for Office*.
#[derive(Debug, Default)]
pub struct PasswordDialog {
    pub id: String,
    name: String,
    /// The speaker asked while connecting (rather than *Change* on its
    /// page).
    asked: bool,
    /// *Connect* connects with the password (otherwise it is kept for next
    /// time).
    connect: bool,
    typed: String,
    shown: bool,
    focus_done: bool,
}

impl PasswordDialog {
    /// *Change* on a speaker's page: the password is kept for next time.
    pub fn change(id: &str, name: &str) -> Self {
        PasswordDialog { id: id.into(), name: name.into(), ..Default::default() }
    }

    /// *Enter password…* where a speaker needs its password: *Connect*
    /// connects with it.
    pub fn to_connect(id: &str, name: &str) -> Self {
        PasswordDialog { id: id.into(), name: name.into(), connect: true, ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let ask = match port.wanted_dialog() {
            Some(Wanted::Password(a)) if a.id == self.id => Some(a),
            _ if self.asked => return false,
            _ => None,
        };
        let wrong = ask.as_ref().is_some_and(|a| a.wrong);
        let once = ask.as_ref().is_some_and(|a| a.once);
        let name = ask.as_ref().map_or(self.name.clone(), |a| a.name.clone());
        let mut keep = true;
        let mut submit = false;
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetPasswordFor, speaker = &name));
                body(ui, &w!(lang, SetAsksPassword, speaker = &name));
                if wrong {
                    error(ui, &w!(lang, SetPasswordWrong));
                }
                ui.horizontal(|ui| {
                    let show_label = if self.shown { w!(lang, SetHide) } else { w!(lang, SetShow) };
                    let field_w = ui.available_width() - 60.0;
                    let r = TextField::new(&mut self.typed, &w!(lang, SetPasswordTitle)).password(!self.shown).width(field_w).show(ui);
                    if !self.focus_done {
                        r.request_focus();
                        self.focus_done = true;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.typed.is_empty() {
                        submit = true;
                    }
                    if Button::link(&show_label).show(ui).clicked() {
                        self.shown = !self.shown;
                    }
                });
                note(ui, &if once { w!(lang, SetPasswordOnce) } else { w!(lang, SetPasswordKept) });
        });
        {
            let ui = &mut foot;
                at_right(ui, |ui| {
                    if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                        if self.asked {
                            port.act(Act::CancelPassword(self.id.clone()));
                        }
                        keep = false;
                    }
                    let label = if self.connect { w!(lang, SetConnect) } else { w!(lang, SetSavePassword) };
                    if Button::primary(&label).enabled(!self.typed.is_empty()).default_action(true).show(ui).clicked() {
                        submit = true;
                    }
                });
        }
        if submit {
            port.act(Act::SubmitPassword { id: self.id.clone(), password: std::mem::take(&mut self.typed), connect: self.connect });
            // Asked while connecting: stays open until the speaker says
            // whether it worked. From the page: done.
            if !self.asked {
                keep = false;
            }
        }
        keep
    }
}

// -------------------------------------------------------------------------
// A Sendspin speaker's code
// -------------------------------------------------------------------------

/// *Pair with Desk Speaker*: a code the speaker shows or says, or its
/// token.
#[derive(Debug, Default)]
pub struct SendspinDialog {
    pub id: String,
    typed: String,
    /// Typing the token instead of a code.
    token: bool,
    focus_done: bool,
}

impl SendspinDialog {
    pub fn new(id: &str) -> Self {
        SendspinDialog { id: id.into(), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let ask = match port.wanted_dialog() {
            Some(Wanted::SendspinCode(a)) if a.id == self.id => a,
            _ => return false,
        };
        let token = self.token || !ask.live_code;
        let name = ask.name.clone();
        let mut keep = true;
        let mut submit = false;
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetPairWith, speaker = &name));
                if ask.identity_changed {
                    let _ = info_bar(ui, &InfoBar { kind: BarKind::Warning, title: None, text: &w!(lang, SetIdentityChanged, speaker = &name), action: None });
                } else if ask.lost {
                    marked_line(ui, Mark::Info, &w!(lang, SetSendspinForgot, speaker = &name));
                }
                if ask.wrong {
                    error(ui, &w!(lang, SetSendspinWrong));
                }
                body(ui, &if token { w!(lang, SetPasteToken, speaker = &name) } else { w!(lang, SetSpeakerShowsCode, speaker = &name) });
                let field = if token { w!(lang, SetTokenName) } else { w!(lang, SetCodeName) };
                let r = TextField::new(&mut self.typed, &field).show(ui);
                if !self.focus_done {
                    r.request_focus();
                    self.focus_done = true;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.typed.trim().is_empty() {
                    submit = true;
                }
                ui.horizontal(|ui| {
                    if let Some(s) = ask.seconds_left.filter(|_| !token) {
                        note(ui, &words::left(lang, s));
                    }
                    if ask.live_code {
                        at_right(ui, |ui| {
                            let label = if token { w!(lang, SetUseCodeInstead) } else { w!(lang, SetNoCodeToken) };
                            if Button::link(&label).show(ui).clicked() {
                                self.token = !self.token;
                                self.typed.clear();
                                self.focus_done = false;
                            }
                        });
                    }
                });
        });
        {
            let ui = &mut foot;
                at_right(ui, |ui| {
                    if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                        port.act(Act::CancelSendspin(self.id.clone()));
                        keep = false;
                    }
                    if Button::primary(&w!(lang, SetPair)).enabled(!self.typed.trim().is_empty()).default_action(true).show(ui).clicked() {
                        submit = true;
                    }
                });
        }
        if submit {
            port.act(Act::SubmitSendspin { id: self.id.clone(), code: self.typed.trim().to_string() });
            self.typed.clear();
        }
        keep
    }
}

fn marked_line(ui: &mut Ui, mark: Mark, text: &str) {
    let _ = Para::new(text).mark(mark).size(size::SECONDARY).tone(Tone::Primary).show(ui);
}

// -------------------------------------------------------------------------
// Music Assistant's code
// -------------------------------------------------------------------------

/// *Pair this PC with Music Assistant*: the code to type there. Closes by
/// itself when pairing finishes.
#[derive(Debug, Default)]
pub struct MaCodeDialog {
    st: PageState,
}

impl MaCodeDialog {
    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let show = match port.wanted_dialog() {
            Some(Wanted::MaCode(s)) => s,
            _ => return false,
        };
        let mut keep = true;
        let t = Theme::of(ui.ctx());
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetMaPairTitle));
                ui.spacing_mut().item_spacing.x = 24.0;
                ui.columns(2, |cols| {
                    {
                        let ui = &mut cols[0];
                        ui.vertical(|ui| {
                            column_label(ui, &w!(lang, SetInMa));
                            body(ui, &w!(lang, SetMaPairStep1));
                            body(ui, &w!(lang, SetMaPairStep2, pc = &show.this_pc));
                            body(ui, &w!(lang, SetMaPairStep3));
                        });
                    }
                    {
                        let ui = &mut cols[1];
                        ui.vertical(|ui| {
                            column_label(ui, &w!(lang, SetOnThisPc));
                            let g = ui.painter().layout_no_wrap(super::music_assistant::spaced_code(&show.code), t.strong(34.0), t.c.text);
                            let (r, _) = ui.allocate_exact_size(g.size(), Sense::hover());
                            ui.painter().galley(r.min, g, t.c.text);
                            super::widgets::copy_button(ui, &mut self.st, "ma-code", &w!(lang, SetCopyCode), &w!(lang, SetCopied), &show.code);
                            let waiting = match show.seconds_left {
                                Some(s) => w!(lang, SetWaitingForMaLeft, left = &words::left(lang, s)),
                                None => w!(lang, SetWaitingForMa),
                            };
                            busy(ui, &waiting);
                        });
                    }
                });
        });
        {
            let ui = &mut foot;
                note(ui, &w!(lang, SetClosesItself));
                at_right(ui, |ui| {
                    if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                        port.act(Act::CancelMaPairing);
                        keep = false;
                    }
                });
        }
        keep
    }
}

// -------------------------------------------------------------------------
// Adding a speaker by address
// -------------------------------------------------------------------------

/// *Add a speaker by address*: one field and *Find*; the details by hand
/// behind a link.
#[derive(Debug, Default)]
pub struct AddDialog {
    address: String,
    name: String,
    by_hand: bool,
    airplay: bool,
    port: String,
    description: String,
    /// The way chosen when two answered.
    way: Option<Way>,
    focus_done: bool,
}

impl AddDialog {
    /// With the fields for the details by hand open (for pictures).
    pub fn by_hand_open() -> Self {
        AddDialog { by_hand: true, airplay: true, ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let probe = port.probe();
        if probe == Probe::Added {
            port.act(Act::ResetProbe);
            return false;
        }
        let mut keep = true;
        let t = Theme::of(ui.ctx());
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetAddTitle));
                let _ = Para::new(&w!(lang, SetAddLead)).size(size::BODY).show(ui);
                let label_w = 130.0;
                let mut find = false;
                ui.horizontal(|ui| {
                    ui.allocate_ui(Vec2::new(label_w, 32.0), |ui| {
                        ui.set_min_width(label_w);
                        ui.label(egui::RichText::new(w!(lang, SetAddress)).font(t.font(size::BODY)).color(t.c.text));
                    });
                    let field_w = ui.available_width() - 80.0;
                    let r = TextField::new(&mut self.address, &w!(lang, SetAddress)).hint(&w!(lang, SetAddressHint)).width(field_w).show(ui);
                    if !self.focus_done {
                        r.request_focus();
                        self.focus_done = true;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        find = true;
                    }
                    let checking = matches!(probe, Probe::Checking { .. });
                    if Button::primary(&w!(lang, SetFind)).enabled(!self.address.trim().is_empty() && !checking).show(ui).clicked() {
                        find = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.allocate_ui(Vec2::new(label_w, 32.0), |ui| {
                        ui.set_min_width(label_w);
                        ui.label(egui::RichText::new(w!(lang, SetNameOptional)).font(t.font(size::BODY)).color(t.c.text));
                    });
                    let _ = TextField::new(&mut self.name, &w!(lang, SetNameOptional)).show(ui);
                });
                if find && !self.address.trim().is_empty() {
                    self.way = None;
                    port.act(Act::Find { address: self.address.trim().to_string(), name: self.name.trim().to_string() });
                }
                match &probe {
                    Probe::Checking { address } => busy(ui, &w!(lang, SetCheckingAddress, address = address)),
                    Probe::Nothing { address } => {
                        let _ = Para::new(&w!(lang, SetNothingAt, address = address)).mark(Mark::Error).tone(Tone::Red).size(size::SECONDARY).show(ui);
                    }
                    Probe::Invalid(why) => {
                        let _ = Para::new(why).mark(Mark::Error).tone(Tone::Red).size(size::SECONDARY).show(ui);
                    }
                    Probe::Found { address, ways } => self.found(ui, port, lang, address, ways),
                    Probe::Idle | Probe::Added => {}
                }
                ui.add_space(4.0);
                if !self.by_hand {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if Button::link(&w!(lang, SetByHand)).show(ui).clicked() {
                            self.by_hand = true;
                        }
                        note(ui, &w!(lang, SetByHandWhat));
                    });
                } else {
                    self.by_hand_fields(ui, port, lang, label_w);
                }
        });
        {
            let ui = &mut foot;
                at_right(ui, |ui| {
                    if Button::standard(&w!(lang, SetClose)).show(ui).clicked() {
                        port.act(Act::ResetProbe);
                        keep = false;
                    }
                });
        }
        keep
    }

    fn found(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang, address: &str, ways: &[Found]) {
        let t = Theme::of(ui.ctx());
        let Some(first) = ways.first() else { return };
        let way = self.way.unwrap_or(first.way);
        let chosen = ways.iter().find(|f| f.way == way).unwrap_or(first);
        egui::Frame::none().fill(t.c.card).stroke(Stroke::new(1.0_f32, t.c.green)).rounding(radius::CARD).inner_margin(14.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                let (tile, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                ui.painter().rect_filled(tile, radius::CARD, t.c.hover);
                kind_icon(&chosen.kind).paint(ui.painter(), Rect::from_center_size(tile.center(), Vec2::splat(22.0)), t.c.text);
                let add_long = w!(lang, SetAddNamed, speaker = &chosen.name);
                let add_short = w!(lang, SetAddButton);
                let text_w = (ui.available_width() - 160.0).max(160.0);
                ui.allocate_ui(Vec2::new(text_w, 40.0), |ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        super::widgets::strong(ui, &w!(lang, SetFoundNamed, speaker = &chosen.name), size::BODY);
                        if ways.len() == 1 {
                            note(ui, &w!(lang, SetFoundLine, kind = &words::kind(lang, &chosen.kind), address = address, delay = &words::about(lang, chosen.delay_ms)));
                        } else {
                            note(ui, &w!(lang, SetFoundTwoWays));
                        }
                    });
                });
                at_right(ui, |ui| {
                    if Button::primary(super::widgets::fitting(ui, &add_long, &add_short)).accessible_name(add_long.clone()).show(ui).clicked() {
                        port.act(Act::AddFound { way, name: chosen.name.clone() });
                    }
                });
            });
            if ways.len() > 1 {
                ui.add_space(6.0);
                for f in ways {
                    let label = match f.way {
                        Way::Upnp => w!(lang, SetWayRecommended, way = &words::kind(lang, &f.kind)),
                        Way::AirPlay => w!(lang, SetWayPlain, way = &w!(lang, KindAirPlay)),
                    };
                    if Choice::new(f.way == way, &label).description(&words::about(lang, f.delay_ms)).show(ui) {
                        self.way = Some(f.way);
                    }
                }
            }
        });
    }

    fn by_hand_fields(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang, label_w: f32) {
        let t = Theme::of(ui.ctx());
        let options = [w!(lang, KindAirPlay), w!(lang, SetUpnpKind)];
        let refs: Vec<&str> = options.iter().map(String::as_str).collect();
        let label = |ui: &mut Ui, text: &str| {
            ui.allocate_ui(Vec2::new(label_w, 32.0), |ui| {
                ui.set_min_width(label_w);
                ui.label(egui::RichText::new(text).font(t.font(size::BODY)).color(t.c.text));
            });
        };
        ui.horizontal(|ui| {
            label(ui, &w!(lang, SetConnectsBy));
            if let Some(i) = DropDown::new("add-by-hand-kind", &w!(lang, SetConnectsBy), &refs, if self.airplay { 0 } else { 1 }).show(ui) {
                self.airplay = i == 0;
            }
        });
        if self.airplay {
            ui.horizontal(|ui| {
                label(ui, &w!(lang, SetPort));
                let _ = TextField::new(&mut self.port, &w!(lang, SetPort)).hint("7000").width(120.0).show(ui);
            });
        } else {
            ui.horizontal(|ui| {
                label(ui, &w!(lang, SetDescriptionAddress));
                let _ = TextField::new(&mut self.description, &w!(lang, SetDescriptionAddress)).show(ui);
            });
        }
        ui.horizontal(|ui| {
            ui.add_space(label_w + 8.0);
            let ready = if self.airplay { !self.address.trim().is_empty() } else { !self.description.trim().is_empty() };
            if Button::standard(&w!(lang, SetAddButton)).enabled(ready).show(ui).clicked() {
                port.act(Act::AddByHand {
                    airplay: self.airplay,
                    host: self.address.trim().to_string(),
                    port: self.port.trim().parse().ok(),
                    description: self.description.trim().to_string(),
                    name: self.name.trim().to_string(),
                });
            }
        });
    }
}

// -------------------------------------------------------------------------
// Tone
// -------------------------------------------------------------------------

/// *Kitchen's tone*: the ten bands and the preamp, with presets; live and
/// kept as they change.
#[derive(Debug)]
pub struct ToneDialog {
    pub id: String,
}

impl ToneDialog {
    pub fn new(id: &str) -> Self {
        ToneDialog { id: id.to_string() }
    }

    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let Some(v) = port.tone(&self.id) else { return false };
        let id = self.id.clone();
        let mut keep = true;
        let t = Theme::of(ui.ctx());
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetToneTitle, speaker = &v.name));
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(w!(lang, SetPreset)).font(t.font(size::BODY)).color(t.c.text));
                    let mut names: Vec<String> = EQ_PRESETS.iter().map(|p| preset_name(lang, p.name)).collect();
                    names.push(w!(lang, SetPresetCustom));
                    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                    let current = crate::equalizer::matching_preset(&v.eq.bands_db).and_then(|n| EQ_PRESETS.iter().position(|p| p.name == n)).unwrap_or(EQ_PRESETS.len());
                    if let Some(i) = DropDown::new(("tone-preset", &id), &w!(lang, SetPreset), &refs, current).min_width(240.0).show(ui) {
                        if i < EQ_PRESETS.len() {
                            port.act(Act::Tone(id.clone(), ToneAct::Preset(i)));
                        }
                    }
                    if let Some(h) = v.headroom_db {
                        at_right(ui, |ui| note(ui, &w!(lang, SetHeadroom, db = &super::speaker::db(lang, h))));
                    }
                });
                egui::Frame::none().fill(t.c.card).stroke(t.card_stroke()).rounding(radius::CARD).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let n = EQ_BANDS + 1;
                    let col = ui.available_width() / n as f32;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let pre = w!(lang, SetPreamp);
                        if let Some(db) = vertical(ui, col, v.eq.preamp_db, -12.0..=0.0, &super::speaker::db(lang, v.eq.preamp_db), &pre, &pre) {
                            port.act(Act::Tone(id.clone(), ToneAct::Preamp(db)));
                        }
                        for (i, hz) in EQ_BAND_HZ.iter().enumerate() {
                            let label = hz_label(*hz);
                            let name = w!(lang, SetBandName, band = &label);
                            if let Some(db) = vertical(ui, col, v.eq.bands_db[i], -12.0..=12.0, &super::speaker::db(lang, v.eq.bands_db[i]), &label, &name) {
                                port.act(Act::Tone(id.clone(), ToneAct::Band(i, db)));
                            }
                        }
                    });
                });
                note(ui, &w!(lang, SetToneDialogLine));
        });
        {
            let ui = &mut foot;
                if Button::standard(&w!(lang, SetUndo)).enabled(v.can_undo).show(ui).clicked() {
                    port.act(Act::Tone(id.clone(), ToneAct::Undo));
                }
                if Button::standard(&w!(lang, SetResetFlat)).show(ui).clicked() {
                    port.act(Act::Tone(id.clone(), ToneAct::Flat));
                }
                at_right(ui, |ui| {
                    if Button::primary(&w!(lang, SetClose)).default_action(true).show(ui).clicked() {
                        keep = false;
                    }
                });
        }
        keep
    }
}

fn hz_label(hz: f64) -> String {
    if hz >= 1000.0 {
        format!("{}k", (hz / 1000.0).round() as i64)
    } else {
        format!("{}", hz.round() as i64)
    }
}

fn preset_name(lang: Lang, name: &str) -> String {
    match name {
        "Flat" => w!(lang, SetPresetFlat),
        "Bass boost" => w!(lang, SetPresetBassBoost),
        "Bass cut" => w!(lang, SetPresetBassCut),
        "Treble boost" => w!(lang, SetPresetTrebleBoost),
        "Vocal" => w!(lang, SetPresetVoice),
        "Loudness" => w!(lang, SetPresetLoudness),
        "Small speaker" => w!(lang, SetPresetSmallSpeaker),
        other => other.to_string(),
    }
}

/// A vertical slider in a column `width` wide: the value above, the label
/// under it. The new value, in steps of 0.5.
fn vertical(ui: &mut Ui, width: f32, value: f32, range: std::ops::RangeInclusive<f32>, shown: &str, label: &str, name: &str) -> Option<f32> {
    let t = Theme::of(ui.ctx());
    let (col, _) = ui.allocate_exact_size(Vec2::new(width, 290.0), Sense::hover());
    let p = ui.painter().clone();
    let top = p.layout_no_wrap(shown.to_owned(), t.font(size::SECONDARY), t.c.text_secondary);
    p.galley(egui::pos2(col.center().x - top.size().x / 2.0, col.top()), top, t.c.text_secondary);
    let bottom = p.layout_no_wrap(label.to_owned(), t.font(size::SECONDARY), t.c.text);
    p.galley(egui::pos2(col.center().x - bottom.size().x / 2.0, col.bottom() - bottom.size().y), bottom, t.c.text);
    let track = Rect::from_center_size(egui::pos2(col.center().x, col.center().y), Vec2::new(24.0, 220.0));
    let resp = ui.interact(track, ui.id().with(("band", name)), Sense::click_and_drag());
    resp.widget_info(|| WidgetInfo::slider(true, value as f64, name));
    let (lo, hi) = (*range.start(), *range.end());
    let snap = |v: f32| ((v * 2.0).round() / 2.0).clamp(lo, hi);
    let (y0, y1) = (track.bottom() - 10.0, track.top() + 10.0);
    let y_of = |v: f32| y0 + (y1 - y0) * ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    let mut out = None;
    if let Some(pos) = resp.interact_pointer_pos() {
        if resp.dragged() || resp.clicked() {
            let f = ((y0 - pos.y) / (y0 - y1)).clamp(0.0, 1.0);
            let to = snap(lo + f * (hi - lo));
            if to != value {
                out = Some(to);
            }
        }
    }
    if resp.has_focus() {
        ui.memory_mut(|m| m.set_focus_lock_filter(resp.id, egui::EventFilter { vertical_arrows: true, ..Default::default() }));
        let steps: f32 = ui.input(|i| {
            let mut s = 0.0;
            if i.key_pressed(Key::ArrowUp) || i.key_pressed(Key::ArrowRight) {
                s += 1.0;
            }
            if i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowLeft) {
                s -= 1.0;
            }
            s
        });
        if steps != 0.0 {
            let to = snap(value + steps * 0.5);
            if to != value {
                out = Some(to);
            }
        }
    }
    let l = look_as(ui, &resp, name);
    p.rect_filled(Rect::from_center_size(track.center(), Vec2::new(4.0, track.height())), 2.0, t.c.track);
    let thumb = egui::pos2(track.center().x, y_of(value));
    p.circle_filled(thumb + Vec2::new(0.0, 0.5), 10.5, t.c.card_border);
    p.circle(thumb, 10.0, t.c.control, Stroke::new(1.0_f32, t.c.control_border));
    let dot = if l.pressed { 6.5 } else if l.hovered { 10.0 } else { 8.0 };
    p.circle_filled(thumb, dot / 2.0, t.c.accent);
    if l.focused {
        focus_ring(&p, track, 4.0, false, &t);
    }
    out
}

// -------------------------------------------------------------------------
// The report
// -------------------------------------------------------------------------

/// *Report a problem*: what happened, what the report includes, a preview
/// of the exact text, and where it goes.
#[derive(Debug, Default)]
pub struct ReportDialog {
    what: String,
    parts: Option<ReportParts>,
    st: PageState,
    /// The last thing done with it, said under the preview.
    done: Option<String>,
}

impl ReportDialog {
    /// With *What happened?* already typed (for pictures).
    pub fn with_text(what: &str) -> Self {
        ReportDialog { what: what.into(), ..Default::default() }
    }

    fn show(&mut self, ui: &mut Ui, port: &dyn SettingsPort, lang: Lang) -> bool {
        let parts = *self.parts.get_or_insert_with(ReportParts::default);
        let mut text = port.report(parts);
        if !self.what.trim().is_empty() {
            text = format!("{}\n\n{text}", self.what.trim());
        }
        let mut keep = true;
        let t = Theme::of(ui.ctx());
        let mut foot = frame(ui, |ui| {
                title(ui, &w!(lang, SetReportProblem));
                body(ui, &w!(lang, SetWhatHappened));
                let edit = egui::TextEdit::multiline(&mut self.what).desired_rows(2).desired_width(f32::INFINITY).font(t.font(size::BODY));
                let r = ui.add(edit);
                r.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, w!(lang, SetWhatHappened)));
                ui.spacing_mut().item_spacing.x = 24.0;
                ui.columns(2, |cols| {
                    {
                        let ui = &mut cols[0];
                        ui.vertical(|ui| {
                            body(ui, &w!(lang, SetReportIncludes));
                            let p = self.parts.as_mut().unwrap();
                            if let Some(on) = Check::new(p.last_hour, &w!(lang, SetReportHour)).description(&w!(lang, SetReportHourLine)).show(ui) {
                                p.last_hour = on;
                            }
                            if let Some(on) = Check::new(p.speakers, &w!(lang, SetReportSpeakers)).show(ui) {
                                p.speakers = on;
                            }
                            if let Some(on) = Check::new(p.versions, &w!(lang, SetReportVersions)).show(ui) {
                                p.versions = on;
                            }
                            if let Some(on) = Check::new(p.log, &w!(lang, SetReportLog)).description(&w!(lang, SetReportLogLine)).show(ui) {
                                p.log = on;
                            }
                        });
                    }
                    {
                        let ui = &mut cols[1];
                        ui.vertical(|ui| {
                            body(ui, &w!(lang, SetPreview));
                            egui::Frame::none().fill(t.c.card).stroke(t.card_stroke()).rounding(radius::CARD).inner_margin(10.0).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                egui::ScrollArea::vertical().max_height(190.0).auto_shrink([false, true]).show(ui, |ui| mono(ui, &text));
                            });
                        });
                    }
                });
                note(ui, &w!(lang, SetNothingSent));
                if let Some(d) = &self.done {
                    let _ = Para::new(d).mark(Mark::Done).size(size::SECONDARY).show(ui);
                }
        });
        {
            let ui = &mut foot;
                at_right(ui, |ui| {
                    if Button::standard(&w!(lang, SetCancel)).show(ui).clicked() {
                        keep = false;
                    }
                    if Button::standard(&w!(lang, SetOpenIssue)).icon(Icon::External).show(ui).clicked() {
                        port.act(Act::OpenIssue(text.clone()));
                        self.done = Some(w!(lang, SetIssueOpened));
                    }
                    if Button::standard(&w!(lang, SetSaveReport)).show(ui).clicked() {
                        port.act(Act::SaveReport(text.clone()));
                        self.done = Some(w!(lang, SetReportSaved));
                    }
                    let before = self.st.copied.clone();
                    buttons(ui, |ui| super::widgets::copy_button(ui, &mut self.st, "report", &w!(lang, SetCopyReport), &w!(lang, SetCopied), &text));
                    if self.st.copied != before {
                        self.done = Some(w!(lang, SetReportCopied));
                    }
                });
        }
        keep
    }
}
