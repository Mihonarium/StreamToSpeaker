//! Pictures of the dialogs that keep a window of their own, drawn from the
//! scripted port, in the states worth checking by eye. Run them with
//! the preview tool (`dialog-*`).

use egui::Ui;

use crate::ui::preview::{Scene, Stage};

use super::dialogs::{CodeDialog, Dialog, MaCodeDialog, PasswordDialog, ReportDialog, SendspinDialog};
use super::port::*;
use super::scripted::{ScriptedSettings, State};

/// Every scene of the dialogs.
pub fn scenes() -> Vec<Scene> {
    vec![
        Scene { name: "dialog-pairing-code", size: [720.0, 400.0], draw: pairing_code },
        Scene { name: "dialog-pairing-code-wrong", size: [720.0, 420.0], draw: pairing_wrong },
        Scene { name: "dialog-pairing-code-timed-out", size: [720.0, 400.0], draw: pairing_timed_out },
        Scene { name: "dialog-password", size: [520.0, 320.0], draw: password },
        Scene { name: "dialog-password-wrong", size: [520.0, 360.0], draw: password_wrong },
        Scene { name: "dialog-sendspin-code", size: [560.0, 300.0], draw: sendspin },
        Scene { name: "dialog-sendspin-identity", size: [560.0, 360.0], draw: sendspin_identity },
        Scene { name: "dialog-music-assistant-code", size: [600.0, 340.0], draw: ma_code },
        Scene { name: "dialog-report", size: [840.0, 560.0], draw: report },
    ]
}

/// `d` drawn with the scripted app changed by `f`.
fn dialog(ui: &mut Ui, mut d: Dialog, f: impl FnOnce(&mut State)) {
    let port = ScriptedSettings::new();
    port.set(f);
    let _ = d.show_in(ui, &port);
}

fn code_ask(stage: CodeStage) -> impl FnOnce(&mut State) {
    move |s: &mut State| s.wanted = Some(Wanted::PairingCode(CodeAsk { id: "den".into(), name: "Den TV".into(), stage, seconds_left: Some(102) }))
}

fn pairing_code(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Code(CodeDialog::new("den")), code_ask(CodeStage::Showing));
}

fn pairing_wrong(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Code(CodeDialog::new("den")), code_ask(CodeStage::Wrong));
}

fn pairing_timed_out(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Code(CodeDialog::new("den")), code_ask(CodeStage::TimedOut));
}

fn password(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Password(PasswordDialog::change("office", "Office")), |_| ());
}

fn password_wrong(ui: &mut Ui, _: &mut Stage) {
    let mut d = PasswordDialog::change("office", "Office");
    d.id = "office".into();
    dialog(ui, Dialog::Password(d), |s| {
        s.wanted = Some(Wanted::Password(PasswordAsk { id: "office".into(), name: "Office".into(), wrong: true, once: false }));
    });
}

fn sendspin_ask(identity_changed: bool) -> impl FnOnce(&mut State) {
    move |s: &mut State| {
        s.wanted = Some(Wanted::SendspinCode(SendspinAsk {
            id: "desk".into(),
            name: "Desk Speaker".into(),
            live_code: true,
            seconds_left: None,
            identity_changed,
            lost: false,
            wrong: false,
        }))
    }
}

fn sendspin(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Sendspin(SendspinDialog::new("desk")), sendspin_ask(false));
}

fn sendspin_identity(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Sendspin(SendspinDialog::new("desk")), sendspin_ask(true));
}

fn ma_code(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::MaCode(MaCodeDialog::default()), |s| {
        s.wanted = Some(Wanted::MaCode(MaCodeShow { code: "482917".into(), this_pc: "DESKTOP-7Q".into(), seconds_left: None }));
    });
}

fn report(ui: &mut Ui, _: &mut Stage) {
    dialog(ui, Dialog::Report(ReportDialog::with_text("Bedroom plays for about two seconds, then stops. Only the left one plays.")), |_| ());
}
