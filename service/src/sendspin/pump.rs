//! A reader thread per connection that turns the channel into a queue of
//! events, so the protocol state machine can wait with timeouts without
//! ever leaving a half-read frame on the socket.
//!
//! The one thing the pump must do itself is the key switch of an in-band
//! re-handshake: frames after the switch point are encrypted under the new
//! keys, and the pump is the only party that decrypts. A hook runs inline
//! for every `noise/handshake` JSON message that arrives in transport mode.

use crossbeam_channel::{unbounded, Receiver};
use log::debug;
use serde_json::Value;
use std::time::Instant;

use super::channel::{msg_type, ChannelReader, Incoming};
use super::handshake::ResolvedPsk;

#[derive(Debug)]
pub enum Event {
    Json { value: Value, received_at: Instant },
    Binary { data: Vec<u8>, received_at: Instant },
    /// A re-handshake finished inside the pump; keys are switched.
    Rehandshake(Result<(ResolvedPsk, [u8; 32]), String>),
    /// The connection ended (clean close or error text).
    Closed(Option<String>),
}

/// Called with the reader and the `noise/handshake` message. Returns the
/// event to emit, or `None` to pass the message through as plain JSON.
pub type HandshakeHook = Box<dyn FnMut(&mut ChannelReader, &Value) -> Option<Event> + Send>;

pub fn spawn_pump(mut reader: ChannelReader, name: &str, mut hook: HandshakeHook) -> Receiver<Event> {
    let (tx, rx) = unbounded();
    let _ = std::thread::Builder::new().name(format!("sendspin-rx-{}", name)).spawn(move || loop {
        let ev = match reader.recv() {
            Ok(Incoming::Json { value, received_at, .. }) => {
                if msg_type(&value) == "noise/handshake" && reader.is_encrypted() {
                    match hook(&mut reader, &value) {
                        Some(ev) => ev,
                        None => Event::Json { value, received_at },
                    }
                } else {
                    Event::Json { value, received_at }
                }
            }
            Ok(Incoming::Binary { data, received_at }) => Event::Binary { data, received_at },
            Ok(Incoming::Closed) => {
                let _ = tx.send(Event::Closed(None));
                return;
            }
            Err(e) => {
                debug!("sendspin reader ended: {:#}", e);
                let _ = tx.send(Event::Closed(Some(format!("{:#}", e))));
                return;
            }
        };
        let failed = matches!(&ev, Event::Rehandshake(Err(_)));
        if tx.send(ev).is_err() || failed {
            // Receiver gone, or keys are now out of sync: stop reading.
            return;
        }
    });
    rx
}
