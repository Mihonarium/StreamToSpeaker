//! The app's model: the states of the PC's sound and the words every
//! surface shows for them.
//!
//! One engine state, one sentence, the same words on every surface. The
//! engine computes [`Facts`] (what is true: the state, the speakers, the
//! levels, the delay numbers); [`build`] turns them into a [`Snapshot`],
//! which holds every surface's words; the surfaces (the panel, the tray,
//! notifications, the shortcut's confirmation, the phone remote, the command
//! line) only draw it, and send [`Command`]s back through a [`Port`].
//!
//! Plain Rust: no windows, no I/O, no clock. Times come in as values.

pub mod command;
pub mod delay;
pub mod facts;
pub mod places;
pub mod port;
pub mod rules;
pub mod scripted;
pub mod snapshot;
pub mod state;
pub mod surfaces;
pub mod text;

#[cfg(test)]
mod tests;

pub use command::{
    Command, DelayOutcome, DelayStep, Effect, NoticeKind, Open, Outcome, Refusal, Surface, Ticket,
    VolumeOwner,
};
pub use facts::Facts;
pub use port::{Port, PortCore, Watch, Watcher};
pub use snapshot::{build, Snapshot};
pub use state::{NowState, PcOutputId, Place, SpeakerId};
pub use text::{Key, Lang};
