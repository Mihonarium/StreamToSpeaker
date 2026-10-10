//! Sendspin support — Music Assistant's synchronized multi-room audio
//! protocol (<https://github.com/Sendspin/spec>), in two roles:
//!
//! * **Source client** ([`source`]): we advertise ourselves as a Sendspin
//!   client with the `source@v1` role. Music Assistant's "Sendspin Source"
//!   plugin then lists this PC as a Live Input that can play on any of its
//!   players.
//! * **Server** ([`server`]): we discover Sendspin players (ESPHome
//!   speakers, the reference players) and push timestamped audio to them
//!   ourselves, so they appear in the speaker list without Music Assistant.
//!
//! Layers, bottom up: [`ws`] (RFC 6455 framing) → [`noise`] (KKpsk2
//! transport) → [`handshake`] (init exchange + Noise messages) →
//! [`channel`]/[`pump`] (typed messages, fragmentation, re-handshake key
//! switch) → [`proto`] (message shapes for both wire dialects) → the role
//! state machines. Pairing: [`pairing`] (+ [`cpace`] PAKE, [`keys`]
//! tokens), persisted via [`store`]. Clock sync for the client role:
//! [`time_filter`].

pub mod channel;
pub mod cpace;
pub mod handshake;
pub mod keys;
pub mod mdns;
pub mod noise;
pub mod pairing;
pub mod proto;
pub mod resample;
pub mod server;
pub mod flac;
pub mod pump;
pub mod source;
pub mod store;
pub mod time_filter;
pub mod ws;

#[cfg(test)]
mod pairing_vectors;

use std::sync::OnceLock;
use std::time::Instant;

use store::SendspinConfig;

/// Persistence for identities and pairing records. `update` runs the
/// closure under the config lock and saves when it returns `true`.
pub trait SendspinStore: Send + Sync {
    fn snapshot(&self) -> SendspinConfig;
    fn update(&self, f: &mut dyn FnMut(&mut SendspinConfig) -> bool);
}

/// In-memory store (tests, tooling).
#[derive(Default)]
pub struct MemoryStore(pub std::sync::Mutex<SendspinConfig>);

impl SendspinStore for MemoryStore {
    fn snapshot(&self) -> SendspinConfig {
        self.0.lock().unwrap().clone()
    }

    fn update(&self, f: &mut dyn FnMut(&mut SendspinConfig) -> bool) {
        let mut g = self.0.lock().unwrap();
        f(&mut g);
    }
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Our monotonic clock in microseconds — the "client clock" of our source
/// role and the "server clock" of our server role.
pub fn now_us() -> i64 {
    instant_to_us(Instant::now())
}

pub fn instant_to_us(t: Instant) -> i64 {
    let e = epoch();
    if t >= e {
        t.duration_since(e).as_micros() as i64
    } else {
        -(e.duration_since(t).as_micros() as i64)
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// This computer's name (Windows `COMPUTERNAME`, else the host name).
pub fn machine_name() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }
    "Stream To Speaker".to_string()
}
