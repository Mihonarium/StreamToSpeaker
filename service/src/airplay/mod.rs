//! AirPlay senders: AirPlay 1 (RAOP) and AirPlay 2.
//!
//! Companion to the UPnP path (`upnp.rs` + `ssdp.rs`). Where UPnP relies
//! on the speaker to pull a WAV stream from our HTTP server, AirPlay is
//! a push protocol: we open an RTSP control connection and push RTP/UDP
//! packets of ALAC (compressed, Apple's encoder) at the receiver.
//!
//! ## AirPlay 1 (RAOP)
//!
//! For receivers that advertise `_raop._tcp`: AirPort Express, Sonos,
//! Apple TV, shairport-sync and the long tail of third-party receivers.
//! Audio is plaintext unless the receiver needs the RSA-wrapped AES key
//! (classic AirPort Express); the MFi (`et=4`) wrapped key is an opt-in
//! experiment. Password-protected receivers are answered with RTSP Digest.
//!
//! * [`discovery`] — mDNS browse for `_raop._tcp` and `_airplay._tcp`,
//!   building [`AirPlayRenderer`] records the user can pick from.
//! * [`rtsp`] — RTSP/1.0 client: opener (OPTIONS or `/auth-setup`) →
//!   ANNOUNCE → SETUP → RECORD → (audio in parallel) → TEARDOWN.
//! * [`rtp`] — RTP packetizer + audio sender thread.
//! * [`alac`] — the uncompressed-ALAC escape frame (a debug fallback; the
//!   default is real compressed ALAC).
//! * [`crypto`] — RSA-wrapped AES-128 session key, per-packet AES-CBC, the
//!   MFi key wrap, Digest auth.
//! * [`timing`] — NTP timing replies, sync packets, retransmits.
//! * [`session`] — High-level session lifecycle, mirroring
//!   `app::start_session` / `app::stop_session` for the UPnP path.
//!
//! ## AirPlay 2
//!
//! HomePods and AirPlay-2-only receivers: HomeKit pairing, encrypted
//! RTSP and audio, PTP timing (`ap2_*`, `pairing`, `hap_pairing`, `srp`,
//! `tlv8`).

pub mod alac;
#[cfg(windows)]
pub mod aac_mf;
pub mod ap2_crypto;
pub mod ap2_events;
pub mod ap2_health;
pub mod ap2_ptp;
pub mod ap2_realtime;
pub mod ap2_resend;
pub mod ap2_rtsp;
pub mod ap2_session;
pub mod crypto;
pub mod discovery;
pub mod dmap;
#[cfg(test)]
pub(crate) mod fake_raop;
pub mod hap_pairing;
pub mod pairing;
#[cfg(test)]
pub(crate) mod raop_transcripts;
pub mod rtp;
pub mod rtsp;
pub mod session;
pub mod srp;
#[cfg(test)]
pub(crate) mod srp_test_receiver;
#[cfg(test)]
pub(crate) mod test_peers;
pub mod timing;
pub mod tlv8;
pub mod untrusted_plist;

pub use ap2_session::{AirPlay2Session, AirPlay2SessionConfig, Ap2StartError};
pub use discovery::{AirPlayDiscoveryState, AirPlayRenderer, Transport};
pub use session::{AirPlaySession, AirPlaySessionConfig};
