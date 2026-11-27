//! Pictures of the flyout as it opens from the notification area.
use super::{
    grid::*,
    panel::{name, show, PanelUi},
};
use crate::{
    model::{
        panel::{panel, About, DelayHead, Panel},
        samples,
    },
    ui::{
        look::Force,
        preview::{Scene, Stage},
        theme::Theme,
    },
};
use egui::{vec2, Rect, Sense, Ui};
const SIZE: [f32; 2] = [WIDTH + 2.0 * (MARGIN + RULE), 0.0];
fn draw(ui: &mut Ui, stage: &mut Stage, sample: &str, forced: Option<(Target, Force)>) {
    let p = panel(&samples::named(sample));
    let t = Theme::of(ui.ctx());
    let mut state = PanelUi::default();
    state.more_open = sample == "fold-open";
    let origin = ui.cursor().min;
    let frame = rect(
        origin,
        MARGIN + RULE,
        MARGIN + RULE,
        WIDTH,
        ui.available_height(),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(frame)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let frame = egui::Frame::none()
        .fill(t.c.surface)
        .stroke(t.card_stroke())
        .rounding(GAP)
        .shadow(t.shadow())
        .show(&mut child, |u| {
            show(
                u,
                &p,
                &mut state,
                if sample == "many-speakers" {
                    640.0
                } else {
                    f32::INFINITY
                },
            )
        });
    if let Some((target, force)) = forced {
        let name = target.name(&p);
        let r = frame
            .inner
            .controls
            .iter()
            .find(|c| c.name == name)
            .expect("visible preview target")
            .rect;
        match force {
            Force::Hover => stage.hover_named(r, name),
            Force::Press => stage.press_named(r, name),
            Force::Focus => stage.focus_named(r, name),
        }
    }
    ui.allocate_rect(
        Rect::from_min_max(
            origin,
            frame.response.rect.max + vec2(MARGIN + RULE, MARGIN + RULE),
        ),
        Sense::hover(),
    );
}
macro_rules! pictures {
    ($($name:literal),* $(,)?) => {
        vec![$(Scene {
            name: concat!("flyout-", $name),
            size: SIZE,
            draw: |ui, stage| draw(ui, stage, $name, None),
        }),*]
    };
}
#[derive(Clone, Copy)]
enum Target {
    Office,
    Kitchen,
    Gear,
    Amount,
    Way,
    Link,
    NoteButton,
    Close,
}
impl Target {
    fn name(self, p: &Panel) -> &str {
        match self {
            Self::Office => &p.list.rows.last().unwrap().spoken,
            Self::Kitchen => &p.list.rows.iter().find(|r| r.marked).unwrap().spoken,
            Self::Gear => &p.list.rows.last().unwrap().gear.label,
            Self::Link => name(&p.links[1]),
            Self::Amount | Self::Way => {
                let About::Delay(d) = &p.about else {
                    unreachable!()
                };
                if matches!(self, Self::Amount) {
                    return name(&d.rows[0].small);
                }
                let DelayHead::Ways { lines, .. } = &d.head else {
                    unreachable!()
                };
                &lines[1].spoken
            }
            Self::NoteButton | Self::Close => {
                let About::Note(n) = &p.about else {
                    unreachable!()
                };
                name(
                    if matches!(self, Self::Close) {
                        n.close.as_ref()
                    } else {
                        n.button.as_ref()
                    }
                    .unwrap(),
                )
            }
        }
    }
}

macro_rules! looks {
    ($($name:literal => ($target:ident, $force:ident)),* $(,)?) => {
        [$(Scene {
            name:concat!("flyout-look-",$name),size:SIZE,
            draw:|ui,stage| {
                let target=Target::$target;
                let sample=if matches!(target,Target::NoteButton|Target::Close) {"pc-kitchen-stopped-responding"} else {"kitchen-playing"};
                draw(ui,stage,sample,Some((target,Force::$force)));
            }
        }),*]
    }
}
/// One picture per sample and per forced control look.
pub fn scenes() -> Vec<Scene> {
    let mut all = samples::with_sample_names!(pictures);
    macro_rules! apps {
        ($($name:literal),*) => { [$(Scene {
            name: $name, size: SIZE, draw: |ui, stage| draw(ui, stage, $name, None),
        }),*] };
    }
    all.extend(samples::with_sample_names!(apps_panel, apps));
    all.extend(looks! {
        "row-pointed"=>(Office,Hover),
        "row-pressed"=>(Office,Press),
        "marked-row-pointed"=>(Kitchen,Hover),
        "gear-pointed"=>(Gear,Hover),
        "amount-pointed"=>(Amount,Hover),
        "amount-pressed"=>(Amount,Press),
        "way-pointed"=>(Way,Hover),
        "link-pointed"=>(Link,Hover),
        "note-button-pointed"=>(NoteButton,Hover),
        "close-pointed"=>(Close,Hover),
        "focus-row"=>(Office,Focus),
        "focus-amount"=>(Amount,Focus),
        "focus-link"=>(Link,Focus),
        "focus-gear"=>(Gear,Focus),
    });
    all
}
