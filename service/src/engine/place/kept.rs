//! What the core keeps across a restart of the app, of Windows or of the
//! PC: one record the engine stores with the settings.
//!
//! Every field has a default, so a record written by an older version reads
//! (what it lacks takes its default), and an older version reads a newer
//! one (it ignores fields it doesn't know; the settings file is read value
//! by value, so a value it can't read costs only that value).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::situation::{Note, WallTime, WayChoice};
use crate::model::state::{PcOutputId, SpeakerId};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    /// The place: `None` is This PC.
    pub place: Option<SpeakerId>,
    /// The output chosen under This PC, if one was.
    pub pc_output: Option<PcOutputId>,
    /// Headphones that took the sound from a speaker and are still
    /// connected: when they go, the note names that speaker.
    pub headphones: Option<KeptHeadphones>,
    /// The speaker the app's device in Windows is named after.
    pub named: Option<SpeakerId>,
    /// Per speaker, the way last chosen.
    pub ways: BTreeMap<SpeakerId, WayChoice>,
    /// The note that says why the sound is on this PC, and when it arose.
    /// It stands until a person switches or closes it.
    pub note: Option<KeptNote>,
    /// A note that follows from the state alone (headphones that are
    /// connected, a speaker that can't play while nothing plays) and that a
    /// person closed: it stays closed while the state implies the same.
    pub closed_note: Option<Note>,
    /// Speakers that can't show what they are doing and disconnected this
    /// PC (*May be in use*), and when.
    pub disconnected: BTreeMap<SpeakerId, WallTime>,
    /// When real sound last played.
    pub last_sound: Option<WallTime>,
    /// When the place last changed.
    pub last_switch: Option<WallTime>,
    /// Per speaker whose level can't be read, the level last set from this
    /// PC and when it last played there.
    pub levels: BTreeMap<SpeakerId, KeptLevel>,
    /// First run is over (it shows at each start until then).
    pub first_run_done: bool,
    /// The first-close explanation has been shown.
    pub window_close_explained: bool,
    /// Windows' outputs used last, other than the app's device, most
    /// recent first.
    pub last_outputs: Vec<PcOutputId>,
    /// Speakers whose screen-reader confirmation was answered with a
    /// switch.
    pub screen_reader_answered: BTreeSet<SpeakerId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeptNote {
    pub note: Note,
    pub at: WallTime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptHeadphones {
    pub output: PcOutputId,
    /// The speaker that was the place before them.
    pub before: SpeakerId,
    /// Windows' name for the output, for the note when it has gone.
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptLevel {
    pub level: u8,
    /// When it last played there from this PC (or the level was set).
    pub at: WallTime,
}
