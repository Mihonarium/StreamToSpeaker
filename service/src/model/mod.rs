//! The scene, its surface views, and the port for commands and publications.

pub mod by_clock;
pub mod command;
pub mod facts;
pub mod fake;
pub mod notes;
pub mod panel;
pub mod port;
pub mod scene;
pub mod samples;
pub mod situation;
pub mod state;
pub mod text;
pub mod view;

#[cfg(test)]
mod scene_tests;
#[cfg(test)]
mod view_tests;


pub use command::{
    Command, DelayOutcome, DelayStep, Effect, NoticeKind, Open, Outcome, Refusal, Surface,
    Ticket, VolumeOwner,
};
pub use port::{Port, PortCore, Watch, Watcher};
pub use view::View as Snapshot;
pub use state::{PcOutputId, Place, SpeakerId};
pub use text::{Key, Lang};
