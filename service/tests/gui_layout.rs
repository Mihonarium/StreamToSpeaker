//! Headless layout tests for the shapes that have regressed in the field:
//! the notice strips (donation / update / error) and the status banner.
//! Lays each out inside a ScrollArea + Frame the way gui.rs does, runs one
//! egui frame on the CPU at three window widths, and inspects the emitted
//! shapes: the frame must stay a strip, buttons must never overlap text,
//! and text must never be squashed into a one-word-per-line column.

use egui::{Color32, Rect, RichText, Shape, Vec2};
use stream_to_speaker::gui_layout::{banner_row, notice_row, sp};

const FRAME_FILL: Color32 = Color32::from_rgb(1, 2, 3);
const BTN_FILL: Color32 = Color32::from_rgb(4, 5, 6);
const WIDTHS: [f32; 3] = [580.0, 720.0, 900.0]; // min window, default, roomy

fn button(ui: &mut egui::Ui, label: &str, w: f32) {
    ui.add_sized([w, 32.0], egui::Button::new(label).fill(BTN_FILL));
}

struct Out {
    frame: Rect,
    buttons: Vec<Rect>,
    texts: Vec<(String, Rect)>,
}

/// One frame of the app's page structure with `content` inside a strip.
fn layout(width: f32, content: impl Fn(&mut egui::Ui)) -> Out {
    let ctx = egui::Context::default();
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(width, 800.0))),
        ..Default::default()
    };
    let mut out = None;
    for _ in 0..2 {
        // Fonts/galleys settle on the first frame; measure the second.
        out = Some(ctx.run(input.clone(), |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::default().inner_margin(sp::M))
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_max_width(ui.available_width() - sp::M);
                        ui.label("header stand-in");
                        ui.add_space(sp::XS);
                        egui::Frame::none()
                            .fill(FRAME_FILL)
                            .inner_margin(egui::Margin::symmetric(sp::M, sp::S))
                            .show(ui, |ui| content(ui));
                        ui.add_space(sp::M);
                        ui.label("next card stand-in");
                    });
                });
        }));
    }
    let out = out.unwrap();
    let (mut frame, mut buttons, mut texts) = (None, vec![], vec![]);
    for cs in &out.shapes {
        match &cs.shape {
            Shape::Rect(r) if r.fill == FRAME_FILL => frame = Some(r.rect),
            Shape::Rect(r) if r.fill == BTN_FILL => buttons.push(r.rect),
            Shape::Text(t) => {
                texts.push((t.galley.text().to_string(), Rect::from_min_size(t.pos, t.galley.size())))
            }
            _ => {}
        }
    }
    Out { frame: frame.expect("strip frame painted"), buttons, texts }
}

fn text_rect(o: &Out, contains: &str) -> Rect {
    o.texts
        .iter()
        .find(|(t, _)| t.contains(contains))
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("no text containing {contains:?}"))
}

fn assert_no_overlap(o: &Out, text: Rect, what: &str) {
    for b in &o.buttons {
        let x = b.intersect(text);
        assert!(
            !(b.intersects(text) && x.width() > 1.0 && x.height() > 1.0),
            "{what}: button {b:?} overlaps text {text:?}"
        );
    }
}

#[test]
fn donation_strip_stays_a_strip_at_every_width() {
    for w in WIDTHS {
        let o = layout(w, |ui| {
            let msg = RichText::new(
                "☕  Stream To Speaker is free and open source. Donate to support its development.",
            );
            notice_row(ui, msg, 96.0 + 84.0 + sp::XS, |ui| {
                button(ui, "Not now", 84.0);
                button(ui, "Donate", 96.0);
            });
        });
        assert!(o.frame.height() < 120.0, "w={w}: frame blew up to {:?}", o.frame);
        assert!(o.frame.width() <= w, "w={w}: frame wider than the window");
        let text = text_rect(&o, "free and open");
        assert_no_overlap(&o, text, &format!("w={w}"));
        assert!(text.height() < 14.0 * 2.6, "w={w}: text wrapped to {} px (squashed?)", text.height());
        assert_eq!(o.buttons.len(), 2);
    }
}

#[test]
fn three_button_update_strip_stacks_when_narrow_and_inlines_when_wide() {
    let build = |ui: &mut egui::Ui| {
        let msg = RichText::new("⬆  Stream To Speaker v0.1.5 is available — you have v0.1.4.");
        notice_row(ui, msg, 100.0 + 130.0 + 72.0 + 2.0 * sp::XS, |ui| {
            button(ui, "Later", 72.0);
            button(ui, "Skip this version", 130.0);
            button(ui, "Download", 100.0);
        });
    };
    for w in WIDTHS {
        let o = layout(w, build);
        let text = text_rect(&o, "is available");
        assert!(o.frame.height() < 120.0, "w={w}: frame blew up to {:?}", o.frame);
        assert_no_overlap(&o, text, &format!("w={w}"));
        assert_eq!(o.buttons.len(), 3);
        let stacked = o.buttons.iter().all(|b| b.top() >= text.bottom() - 1.0);
        if w < 640.0 {
            assert!(stacked, "w={w}: expected the buttons below the message");
        } else {
            assert!(!stacked, "w={w}: expected an inline row");
        }
    }
}

#[test]
fn status_banner_headline_wraps_instead_of_going_vertical() {
    const HEADLINE: &str = "Streaming to Living Room Sonos Beam";
    for w in WIDTHS {
        let o = layout(w, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("⏵").size(34.0));
                ui.add_space(sp::S);
                let col = Some((140.0, |ui: &mut egui::Ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        button(ui, "Disable streaming", 140.0);
                    });
                    ui.add_space(sp::XS);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        button(ui, "Resync", 72.0);
                        ui.label(RichText::new("Audio not working?").size(11.0));
                    });
                }));
                banner_row(ui, col, |ui| {
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(RichText::new(HEADLINE).size(16.0).strong()).wrap());
                    ui.add(egui::Label::new(RichText::new("192.168.1.23  ·  100 packets/sec").size(12.0)).wrap());
                });
            });
        });
        let head = text_rect(&o, "Streaming to");
        assert!(o.frame.height() < 140.0, "w={w}: banner blew up to {:?}", o.frame);
        assert_no_overlap(&o, head, &format!("w={w} headline"));
        let lines = (head.height() / 16.0).round() as u32;
        assert!(lines <= 2, "w={w}: headline is {lines} lines — squashed into a column");
        if w >= 720.0 {
            assert_eq!(lines, 1, "w={w}: headline should fit on one line");
        }
        assert!(head.width() > 150.0, "w={w}: headline only {} px wide", head.width());
        assert_eq!(o.buttons.len(), 2);
    }
}

#[test]
fn banner_row_without_buttons_gives_text_the_full_width() {
    let o = layout(720.0, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("?").size(34.0));
            ui.add_space(sp::S);
            banner_row(ui, None::<(f32, fn(&mut egui::Ui))>, |ui| {
                ui.add(egui::Label::new(RichText::new("No speaker selected").size(16.0)).wrap());
                ui.add(egui::Label::new(RichText::new("Pick a speaker from the list below to start streaming.").size(12.0)).wrap());
            });
        });
    });
    let head = text_rect(&o, "No speaker");
    assert!(o.frame.height() < 120.0);
    assert!((head.height() / 16.0).round() as u32 == 1);
    assert!(o.buttons.is_empty());
}
