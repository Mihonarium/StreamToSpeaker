//! The behaviour, rule by rule. Each table row sets a moment up, does one
//! thing, and checks what the core asks the engine to do and what every
//! surface would show. A small stand-in for the engine answers Windows'
//! and the players' requests the way they usually go; time is injected and
//! nothing sleeps.

mod rig;

mod edges;
mod headphones;
mod moments;
mod never_by_itself;
mod popups;
mod start;
mod surfaces;
mod switches;
mod trouble;
