//! AirPlay 1 / RAOP (Remote Audio Output Protocol) sender.
//!
//! Companion to the UPnP path (`upnp.rs` + `ssdp.rs`). Where UPnP relies
//! on the speaker to pull a WAV stream from our HTTP server, AirPlay is
//! a push protocol: we open an RTSP control connection, encrypt audio
//! per-session with AES-128-CBC (key wrapped under Apple's well-known
//! RSA public key), and push RTP/UDP packets of ALAC-framed PCM at the
//! receiver.
//!
//! ## Scope
//!
//! This is **AirPlay 1**, not 2. AP1 covers every shipping speaker that
//! advertises `_raop._tcp` mDNS — AirPort Express (all generations),
//! Sonos (all current models in their AirPlay 2 mode fall back to AP1
//! for senders that don't pair), Shairport-sync receivers, and the long
//! tail of 3rd-party receivers. It explicitly does **not** cover:
//!
//!   * HomePod / HomePod mini — those require AirPlay 2 with HomeKit
//!     pairing and a working FairPlay handshake. Tracked as future work.
//!   * AirPlay 2 multi-room — single device per session here.
//!
//! ## Modules
//!
//! * [`discovery`] — mDNS browse for `_raop._tcp.local.`, building
//!   [`AirPlayRenderer`] records the user can pick from.
//! * [`rtsp`] — RTSP/1.0 client implementing the OPTIONS → ANNOUNCE →
//!   SETUP → RECORD → (audio in parallel) → TEARDOWN flow.
//! * [`rtp`] — RTP packetizer + audio sender thread.
//! * [`alac`] — Builds uncompressed ALAC frames from 16-bit stereo PCM.
//! * [`crypto`] — RSA-wrapped AES-128 session key + per-packet AES-CBC.
//! * [`session`] — High-level session lifecycle, mirroring
//!   `app::start_session` / `app::stop_session` for the UPnP path.

pub mod alac;
pub mod crypto;
pub mod discovery;
pub mod rtp;
pub mod rtsp;
pub mod session;
pub mod timing;

pub use discovery::{spawn_airplay_discovery, AirPlayDiscoveryState, AirPlayRenderer, Transport};
pub use session::{AirPlaySession, AirPlaySessionConfig};
