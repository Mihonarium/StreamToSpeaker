//! What is true about the sound right now: one value the engine computes,
//! and every surface renders with the same words.
//!
//! Each variant carries exactly the facts its sentences need. Names are
//! looked up by id in the speaker list the engine supplies alongside, so a
//! renamed speaker reads the same everywhere at once.

use std::sync::Arc;


/// A speaker's stable id, as the engine keys it (a UPnP device id, an
/// AirPlay identifier, a Sendspin client id).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpeakerId(pub Arc<str>);

/// The id Music Assistant goes by wherever the app treats it as a speaker
/// (it is one, for where the sound plays). The same text as the engine's
/// own id for it.
pub const MUSIC_ASSISTANT_ID: &str = "music-assistant:input";

impl SpeakerId {
    pub fn new(id: &str) -> Self {
        SpeakerId(Arc::from(id))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Music Assistant, as a speaker.
    pub fn music_assistant() -> Self {
        SpeakerId::new(MUSIC_ASSISTANT_ID)
    }
    pub fn is_music_assistant(&self) -> bool {
        &*self.0 == MUSIC_ASSISTANT_ID
    }
}

/// Stored as its text.
impl serde::Serialize for SpeakerId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for SpeakerId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        Ok(SpeakerId(Arc::from(text)))
    }
}

/// A Windows sound output's endpoint id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PcOutputId(pub Arc<str>);

impl PcOutputId {
    pub fn new(id: &str) -> Self {
        PcOutputId(Arc::from(id))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stored as its text.
impl serde::Serialize for PcOutputId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PcOutputId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        Ok(PcOutputId(Arc::from(text)))
    }
}

/// A place the PC's sound can play: the person's choice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    ThisPc,
    Speaker(SpeakerId),
    MusicAssistant,
}

impl Place {
    /// The speaker this place is, Music Assistant included; `None` for This
    /// PC.
    pub fn speaker(&self) -> Option<SpeakerId> {
        match self {
            Place::ThisPc => None,
            Place::Speaker(id) => Some(id.clone()),
            Place::MusicAssistant => Some(SpeakerId::music_assistant()),
        }
    }

    /// The place a speaker id stands for (Music Assistant's id gives
    /// [`Place::MusicAssistant`]).
    pub fn of_speaker(id: &SpeakerId) -> Place {
        if id.is_music_assistant() {
            Place::MusicAssistant
        } else {
            Place::Speaker(id.clone())
        }
    }
}
/// What a speaker asks for before it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Secret {
    /// A one-time code shown on a TV or said by a speaker.
    Code,
    /// An AirPlay password.
    Password,
}
/// The tray icon's arcs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arcs {
    /// Sound on this PC.
    Ink,
    /// On a speaker, playing or ready.
    Green,
}

/// A problem badge, told apart by shape as well as colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    None,
    /// Amber "!": needs a look.
    Attention,
}
