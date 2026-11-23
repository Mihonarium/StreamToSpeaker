//! Pictures of the panel in every state the scripted engine knows, as it
//! looks when it opens; the `amount-open` ones with an amount typed in the
//! delay block's field.

use egui::{Id, Margin, Ui};

use super::{show, PanelState, WIDTH};
use crate::model::port::Port;
use crate::model::scripted::{Names, Scenario, ScriptedEngine};
use crate::ui::preview::{Scene, Stage};
use crate::ui::theme::Theme;

/// The margin around the panel in its pictures.
const MARGIN: f32 = 20.0;

/// What a scene keeps between the passes of one picture.
#[derive(Clone, Default)]
struct Kept {
    state: PanelState,
    last_natural: f32,
    stable: u32,
}

/// Draw the panel for `scenario` with `names`, framed as the flyout it is.
/// With `open_delay`, the panel first settles at the height it opens at,
/// then opens its delay: the list gives up the room and scrolls.
fn panel_scene(ui: &mut Ui, st: &mut Stage, scenario: &str, names: Names, open_delay: bool) {
    let _ = st;
    let Some(scenario) = Scenario::from_name(scenario) else { return };
    let engine = ScriptedEngine::with_names(scenario, names);
    let snap = engine.snapshot();
    let t = Theme::of(ui.ctx());
    let key = Id::new(("panel-scene", scenario.name(), format!("{names:?}"), open_delay));
    let mut kept: Kept = ui.ctx().data(|d| d.get_temp(key)).unwrap_or_else(|| Kept { state: PanelState::default(), ..Default::default() });
    // The amount field open, with an amount typed.
    if open_delay && kept.stable >= 2 && !kept.state.delay.amount_is_open() {
        kept.state.delay = super::delay_block::BlockState::with_amount("150");
    }
    let mut natural = 0.0;
    ui.add_space(MARGIN);
    ui.horizontal(|ui| {
        ui.add_space(MARGIN);
        egui::Frame::none()
            .fill(t.c.surface)
            .stroke(t.card_stroke())
            .rounding(8.0)
            .shadow(t.shadow())
            .inner_margin(Margin::ZERO)
            .show(ui, |ui| {
                ui.set_width(WIDTH);
                let out = show(ui, &snap, engine.level(), &mut kept.state);
                natural = out.natural_height;
            });
    });
    ui.add_space(MARGIN + 20.0);
    if (natural - kept.last_natural).abs() < 0.5 {
        kept.stable += 1;
    } else {
        kept.stable = 0;
        kept.last_natural = natural;
    }
    ui.ctx().data_mut(|d| d.insert_temp(key, kept));
}

macro_rules! scenes {
    ($( $f:ident => ($scenario:literal, $names:expr, $open:expr, $name:literal) ),* $(,)?) => {
        $( fn $f(ui: &mut Ui, st: &mut Stage) { panel_scene(ui, st, $scenario, $names, $open) } )*
        /// Every picture of the panel.
        pub fn scenes() -> Vec<Scene> {
            vec![ $( Scene { name: $name, size: [WIDTH + 2.0 * MARGIN, 0.0], draw: $f } ),* ]
        }
    };
}

scenes! {
    on_this_pc => ("on-this-pc", Names::Usual, false, "panel-on-this-pc"),
    connecting => ("connecting", Names::Usual, false, "panel-connecting"),
    connecting_from_pc => ("connecting-from-pc", Names::Usual, false, "panel-connecting-from-pc"),
    playing => ("playing", Names::Usual, false, "panel-playing"),
    playing_delay_open => ("playing", Names::Usual, true, "panel-playing-amount-open"),
    playing_long => ("playing", Names::Long, false, "panel-playing-long-names"),
    playing_chinese => ("playing", Names::Chinese, false, "panel-playing-chinese-names"),
    playing_long_delay_open => ("playing", Names::Long, true, "panel-playing-long-names-amount-open"),
    muted => ("muted", Names::Usual, false, "panel-muted"),
    ready => ("ready", Names::Usual, false, "panel-ready"),
    ready_elsewhere => ("ready-elsewhere", Names::Usual, false, "panel-ready-elsewhere"),
    busy_question => ("busy-question", Names::Usual, false, "panel-busy-question"),
    not_taking => ("not-taking", Names::Usual, false, "panel-not-taking"),
    not_responding => ("not-responding", Names::Usual, false, "panel-not-responding"),
    stay_silent => ("stay-silent", Names::Usual, false, "panel-stay-silent"),
    fell_back => ("fell-back", Names::Usual, false, "panel-fell-back"),
    speaker_back => ("speaker-back", Names::Usual, false, "panel-speaker-back"),
    taken => ("taken", Names::Usual, false, "panel-taken"),
    needs_pairing => ("needs-pairing", Names::Usual, false, "panel-needs-pairing"),
    looking_for_speaker => ("looking-for-speaker", Names::Usual, false, "panel-looking-for-speaker"),
    cant_find => ("cant-find", Names::Usual, false, "panel-cant-find"),
    windows_switched => ("windows-switched", Names::Usual, false, "panel-windows-switched"),
    choose_speaker => ("choose-speaker", Names::Usual, false, "panel-choose-speaker"),
    windows_refused => ("windows-refused", Names::Usual, false, "panel-windows-refused"),
    ma_playing => ("music-assistant-playing", Names::Usual, false, "panel-music-assistant-playing"),
    ma_waiting => ("music-assistant-waiting", Names::Usual, false, "panel-music-assistant-waiting"),
    ma_away => ("music-assistant-away", Names::Usual, false, "panel-music-assistant-away"),
    ma_back => ("music-assistant-back", Names::Usual, false, "panel-music-assistant-back"),
    output_missing => ("output-missing", Names::Usual, false, "panel-output-missing"),
    sent_apps => ("sent-apps", Names::Usual, false, "panel-sent-apps"),
    no_app_sent => ("no-app-sent", Names::Usual, false, "panel-no-app-sent"),
    nothing_found => ("nothing-found", Names::Usual, false, "panel-nothing-found"),
    crowded_network => ("crowded-network", Names::Usual, false, "panel-crowded-network"),
    row_catalogue => ("row-catalogue", Names::Usual, false, "panel-row-catalogue"),
    row_catalogue_long => ("row-catalogue", Names::Long, false, "panel-row-catalogue-long-names"),
    row_catalogue_chinese => ("row-catalogue", Names::Chinese, false, "panel-row-catalogue-chinese-names"),
    applying_delay => ("applying-delay", Names::Usual, false, "panel-applying-delay"),
    delay_own => ("delay-own", Names::Usual, false, "panel-delay-own"),
    delay_removing => ("delay-removing", Names::Usual, false, "panel-delay-removing"),
    delay_at_limit => ("delay-at-limit", Names::Usual, false, "panel-delay-at-limit"),
    delay_adding => ("delay-adding", Names::Usual, false, "panel-delay-adding"),
    delay_going_back => ("delay-going-back", Names::Usual, false, "panel-delay-going-back"),
    delay_reconnect_pending => ("delay-reconnect-pending", Names::Usual, false, "panel-delay-reconnect-pending"),
    delay_not_accepted => ("delay-not-accepted", Names::Usual, false, "panel-delay-not-accepted"),
    delay_steady_remove => ("delay-steady-remove", Names::Usual, false, "panel-delay-steady-remove"),
    delay_switched_to_short => ("delay-switched-to-short", Names::Usual, false, "panel-delay-switched-to-short"),
    delay_fixed_on_steady => ("delay-fixed-on-steady", Names::Usual, false, "panel-delay-fixed-on-steady"),
    delay_sendspin => ("delay-sendspin", Names::Usual, false, "panel-delay-sendspin"),
    delay_dropped_out => ("delay-dropped-out", Names::Usual, false, "panel-delay-dropped-out"),
    delay_dropouts => ("delay-dropouts", Names::Usual, false, "panel-delay-dropouts"),
    notice_restart => ("notice-restart", Names::Usual, false, "panel-notice-restart"),
    notice_adapter_off => ("notice-adapter-off", Names::Usual, false, "panel-notice-adapter-off"),
    notice_refused => ("notice-refused", Names::Usual, false, "panel-notice-refused"),
    notice_lab => ("notice-lab", Names::Usual, false, "panel-notice-lab"),
    notice_update => ("notice-update", Names::Usual, false, "panel-notice-update"),
    notice_updated => ("notice-updated", Names::Usual, false, "panel-notice-updated"),
    notice_donation => ("notice-donation", Names::Usual, false, "panel-notice-donation"),
    first_run_pick => ("first-run-pick", Names::Usual, false, "panel-first-run-pick"),
    first_run_hear => ("first-run-hear", Names::Usual, false, "panel-first-run-hear"),
    first_run_find_it => ("first-run-find-it", Names::Usual, false, "panel-first-run-find-it"),
    twins => ("twins", Names::Usual, false, "panel-twins"),
    pc_outputs_closed => ("pc-outputs-closed", Names::Usual, false, "panel-pc-outputs-closed"),
    pc_outputs_open => ("pc-outputs-open", Names::Usual, false, "panel-pc-outputs-open"),
}
