//! The facts used by the panel and the views by the clock.

use std::borrow::Cow;

use super::facts::{
    Applies, DelayRange, DelaySteps, NetworkSeen, PcOutput, Progress, SearchFacts, SpeakerDelay, SpeakerKind,
    VolumeFacts,
};
use super::situation::{ConnKind, RowState, Situation, Spot, WayChoice};
use super::state::{PcOutputId, SpeakerId};
use super::text::{t, Lang};

/// Everything the surfaces show, as facts without words.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub lang: Lang,
    pub situation: Situation,
    /// The Windows sign-in entry, read while the final welcome page is visible.
    pub start_at_sign_in: bool,
    /// Windows' enabled outputs, in list order.
    pub outputs: Vec<PcOutput>,
    /// Whether the list of outputs under This PC is open.
    pub outputs_open: bool,
    /// Visible speakers, in list order.
    pub speakers: Vec<SpeakerSeen>,
    pub volume: VolumeFacts,
    /// Progress of a delay change at the current place.
    pub progress: Progress,
    pub steps: DelaySteps,
    pub search: SearchFacts,
    /// The kind of the network the app looks for speakers on; `None` while
    /// there are several or none.
    pub network: Option<ConnKind>,
    /// This PC's network connections, with the names Windows has for them.
    pub networks: Vec<NetworkSeen>,
    pub repair: RepairNow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeakerSeen {
    pub id: SpeakerId,
    pub name: String,
    pub kind: SpeakerKind,
    /// Available ways in order of increasing delay.
    pub ways: Vec<WaySeen>,
    /// The active way or the one a click will use.
    pub way: WayChoice,
    /// Last AirPlay choice when the speaker offers both.
    pub airplay: Option<WayChoice>,
    /// The identity the speaker uses for its AirPlay password.
    pub airplay_id: Option<SpeakerId>,
    pub used_here: bool,
    pub last_used: Option<u64>,
    /// Shown only to distinguish two rows with the same name.
    pub address: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaySeen {
    pub way: WayChoice,
    pub delay: SpeakerDelay,
    pub range: DelayRange,
    pub applies: Applies,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairNow {
    Unavailable,
    NotAsked,
    TurnedOff,
    TurnOnFailed,
    Running,
    Worked,
    DidntWork,
    NotApproved,
}

impl SpeakerSeen {
    pub(super) fn display_name(&self, lang: Lang) -> Cow<'_, str> {
        if self.id.is_music_assistant() {
            Cow::Owned(t!(lang, MaName))
        } else {
            Cow::Borrowed(&self.name)
        }
    }

    pub(super) fn row_name(&self, scene: &Scene) -> String {
        let name = self.display_name(scene.lang);
        if let Some(address) = &self.address {
            if scene
                .speakers
                .iter()
                .any(|other| other.id != self.id && other.display_name(scene.lang) == name)
            {
                return t!(
                    scene.lang,
                    RowWithAddress,
                    name = name.as_ref(),
                    address = address
                );
            }
        }
        name.into_owned()
    }

    /// The AirPlay way a speaker that is on its own way can be switched to:
    /// the one it last used by AirPlay, else the steadier one it offers.
    pub fn airplay_way(&self) -> Option<WayChoice> {
        if self.way != WayChoice::Native {
            return None;
        }
        let offers = |way: WayChoice| self.ways.iter().any(|w| w.way == way);
        self.airplay
            .filter(|way| offers(*way))
            .or_else(|| [WayChoice::AirPlaySteady, WayChoice::AirPlayShort].into_iter().find(|way| offers(*way)))
    }

    pub(super) fn row_state(&self, scene: &Scene) -> RowState {
        scene
            .situation
            .speaker(&self.id)
            .map_or(RowState::Plain, |s| s.state)
    }
}

impl RowState {
    pub(crate) fn grey(self) -> bool {
        matches!(self, Self::NotFound | Self::RepairFirst)
    }
}

impl Scene {
    pub(crate) fn speaker(&self, id: &SpeakerId) -> Option<&SpeakerSeen> {
        self.speakers.iter().find(|s| &s.id == id)
    }

    pub fn speaker_name(&self, id: &SpeakerId) -> String {
        if id.is_music_assistant() {
            t!(self.lang, MaName)
        } else {
            self.speaker(id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| t!(self.lang, KindSpeaker))
        }
    }

    /// The place in the middle of a sentence.
    pub(super) fn place_name(&self) -> String {
        match &self.situation.spot {
            Spot::ThisPc(_) => t!(self.lang, ThisPcInSentence),
            Spot::Speaker(id) => self.speaker_name(id),
        }
    }

    pub(super) fn output(&self, id: Option<&PcOutputId>) -> Option<&PcOutput> {
        id.and_then(|id| self.outputs.iter().find(|output| &output.id == id))
    }

    /// The name an output is shown by: its own name ("Speakers"), or
    /// Windows' full name, which adds the device, while another output
    /// has the same name.
    pub(crate) fn output_label(&self, output: &PcOutput) -> String {
        let shared = self.outputs.iter().filter(|o| o.name == output.name).count() > 1;
        if output.name.is_empty() || (shared && !output.friendly.is_empty()) {
            output.friendly.clone()
        } else {
            output.name.clone()
        }
    }
}
