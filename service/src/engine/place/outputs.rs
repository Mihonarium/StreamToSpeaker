//! The PC's own output: the output Windows used last, other than the app's
//! device, while it is connected; else the built-in speakers, else
//! headphones, else a display's speakers (and else any other output, so
//! that a PC whose only output is, say, a USB sound card still plays).

use crate::model::facts::OutputKind;
use crate::model::state::PcOutputId;

use super::io::OutputWorld;

/// The PC's own output among `outputs` (connected now), given the outputs
/// used last, most recent first. `None`: this PC has nothing to play on.
pub fn pc_own_output(used_last: &[PcOutputId], outputs: &[OutputWorld]) -> Option<PcOutputId> {
    let connected = |id: &PcOutputId| outputs.iter().any(|o| &o.id == id);
    if let Some(id) = used_last.iter().find(|id| connected(id)) {
        return Some(id.clone());
    }
    let of_kind = |kind: OutputKind| outputs.iter().find(|o| o.kind == kind).map(|o| o.id.clone());
    of_kind(OutputKind::Speakers)
        .or_else(|| of_kind(OutputKind::Headphones))
        .or_else(|| of_kind(OutputKind::Display))
        .or_else(|| of_kind(OutputKind::Other))
}

/// `id` becomes the most recently used output.
pub fn used(list: &mut Vec<PcOutputId>, id: &PcOutputId, keep: usize) {
    list.retain(|o| o != id);
    list.insert(0, id.clone());
    list.truncate(keep);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(id: &str, kind: OutputKind) -> OutputWorld {
        OutputWorld { id: PcOutputId::new(id), kind, name: id.to_string() }
    }

    #[test]
    fn the_pcs_own_output_is_the_last_used_then_builtin_then_headphones_then_display() {
        let speakers = out("speakers", OutputKind::Speakers);
        let headphones = out("headphones", OutputKind::Headphones);
        let tv = out("tv", OutputKind::Display);
        let usb = out("usb", OutputKind::Other);
        let all = vec![tv.clone(), headphones.clone(), speakers.clone(), usb.clone()];
        let id = |s: &str| PcOutputId::new(s);
        assert_eq!(pc_own_output(&[id("tv")], &all), Some(id("tv")), "the one used last");
        assert_eq!(pc_own_output(&[id("gone"), id("headphones")], &all), Some(id("headphones")), "used last and connected");
        assert_eq!(pc_own_output(&[], &all), Some(id("speakers")), "then the built-in speakers");
        assert_eq!(pc_own_output(&[], &[tv.clone(), headphones.clone()]), Some(id("headphones")), "then headphones");
        assert_eq!(pc_own_output(&[], &[usb.clone(), tv.clone()]), Some(id("tv")), "then a display");
        assert_eq!(pc_own_output(&[], &[usb]), Some(id("usb")), "then anything");
        assert_eq!(pc_own_output(&[id("speakers")], &[]), None, "nothing to play on");
        let mut list = vec![id("a"), id("b")];
        used(&mut list, &id("b"), 2);
        assert_eq!(list, vec![id("b"), id("a")]);
        used(&mut list, &id("c"), 2);
        assert_eq!(list, vec![id("c"), id("b")]);
    }
}
