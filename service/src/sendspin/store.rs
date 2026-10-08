//! Persisted Sendspin state (identities, pairing records, settings),
//! stored as one nested object under `sendspin` in `config.json`.
//!
//! Two independent identities: one for our *source client* role (the
//! `client_id` Music Assistant pairs with) and one for our *server* role
//! (the `server_id` players pair with). Keys are raw X25519 private keys,
//! base64url — the same trust level as the AirPlay pairing seeds already
//! kept in this file.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::keys::{b64url, b64url_32};
use super::noise::{generate_private_key, x25519_public};

/// Most pairing records our client keeps (spec floor is 5).
pub const MAX_CLIENT_RECORDS: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SendspinConfig {
    /// Sendspin is opt-in: off (the default) means no Sendspin speaker
    /// discovery and no Music Assistant input.
    #[serde(default)]
    pub enabled: bool,
    /// Act as a Sendspin source for Music Assistant (advertise
    /// `_sendspin._tcp` and stream system audio when a server asks).
    #[serde(default)]
    pub source_enabled: bool,
    /// Name shown in Music Assistant; `None` = the computer's name.
    #[serde(default)]
    pub source_name: Option<String>,
    /// Our client identity (private key, base64url).
    #[serde(default)]
    pub client_key: Option<String>,
    /// Our per-install pairing PSK (base64url); shared with servers only
    /// inside a pairing token.
    #[serde(default)]
    pub pairing_psk: Option<String>,
    /// Long-term pairings with servers (we are the client).
    #[serde(default)]
    pub server_pairings: Vec<ServerPairing>,
    /// `server_id` of the server that last held a playback connection.
    #[serde(default)]
    pub last_playback_server: Option<String>,
    /// Our server identity (private key, base64url).
    #[serde(default)]
    pub server_key: Option<String>,
    /// Long-term pairings with players (we are the server), by `client_id`.
    #[serde(default)]
    pub player_pairings: HashMap<String, PlayerPairing>,
    /// Extra playback delay for Sendspin players, ms (0 = the player's
    /// own minimum buffer).
    #[serde(default)]
    pub player_extra_latency_ms: u32,
    /// Wrong pairing codes entered by servers since the last successful
    /// pairing (or since the user allowed pairing again).
    #[serde(default)]
    pub pairing_code_failures: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerPairing {
    pub server_id: String,
    pub psk: String,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub last_used_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayerPairing {
    pub psk: String,
    #[serde(default)]
    pub name: Option<String>,
    /// The speaker's mDNS instance name (lowercase) this identity was
    /// paired at: a peer at that name with another identity is refused
    /// until it is paired again.
    #[serde(default)]
    pub instance: Option<String>,
}

impl SendspinConfig {
    /// Our client private key, generating (and returning `true` for
    /// "changed, save me") on first use.
    pub fn ensure_client_key(&mut self) -> ([u8; 32], bool) {
        ensure_key(&mut self.client_key)
    }

    pub fn ensure_server_key(&mut self) -> ([u8; 32], bool) {
        ensure_key(&mut self.server_key)
    }

    pub fn ensure_pairing_psk(&mut self) -> ([u8; 32], bool) {
        ensure_key(&mut self.pairing_psk)
    }

    pub fn client_id(&mut self) -> (String, bool) {
        let (k, changed) = self.ensure_client_key();
        (b64url(&x25519_public(&k)), changed)
    }

    /// Insert or replace the record for `server_id`, evicting the least
    /// recently used one beyond [`MAX_CLIENT_RECORDS`].
    pub fn store_server_pairing(&mut self, rec: ServerPairing) {
        self.server_pairings.retain(|r| r.server_id != rec.server_id);
        self.server_pairings.push(rec);
        while self.server_pairings.len() > MAX_CLIENT_RECORDS {
            if let Some((idx, _)) = self
                .server_pairings
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.last_used_unix)
            {
                self.server_pairings.remove(idx);
            }
        }
    }

    pub fn remove_server_pairing_by_psk_id(&mut self, psk_id: &str) -> bool {
        let before = self.server_pairings.len();
        self.server_pairings.retain(|r| {
            b64url_32(&r.psk)
                .map(|k| super::keys::psk_id(&k) != psk_id)
                .unwrap_or(false)
        });
        before != self.server_pairings.len()
    }

    /// The `client_id` paired at a speaker's mDNS instance name.
    pub fn pinned_client_id(&self, instance: &str) -> Option<String> {
        let inst = instance.to_ascii_lowercase();
        self.player_pairings
            .iter()
            .find(|(_, p)| p.instance.as_deref() == Some(inst.as_str()))
            .map(|(id, _)| id.clone())
    }

    /// Pin `client_id`'s record to `instance`; a record pinned there for
    /// another identity is dropped (it was paired again). Returns whether
    /// anything changed.
    pub fn pin_player(&mut self, client_id: &str, instance: &str) -> bool {
        let inst = instance.to_ascii_lowercase();
        if !self.player_pairings.contains_key(client_id) {
            return false;
        }
        let before = self.player_pairings.len();
        self.player_pairings
            .retain(|id, p| id == client_id || p.instance.as_deref() != Some(inst.as_str()));
        let mut changed = before != self.player_pairings.len();
        if let Some(p) = self.player_pairings.get_mut(client_id) {
            if p.instance.as_deref() != Some(inst.as_str()) {
                p.instance = Some(inst);
                changed = true;
            }
        }
        changed
    }

    pub fn touch_server_pairing(&mut self, server_id: &str, now: u64) {
        if let Some(r) = self.server_pairings.iter_mut().find(|r| r.server_id == server_id) {
            r.last_used_unix = now;
        }
    }
}

fn ensure_key(slot: &mut Option<String>) -> ([u8; 32], bool) {
    if let Some(k) = slot.as_deref().and_then(|s| b64url_32(s).ok()) {
        return (k, false);
    }
    let k = generate_private_key();
    *slot = Some(b64url(&k));
    (k, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_generated_once_and_stable() {
        let mut c = SendspinConfig::default();
        let (k1, changed) = c.ensure_client_key();
        assert!(changed);
        let (k2, changed) = c.ensure_client_key();
        assert!(!changed);
        assert_eq!(k1, k2);
        let (s, _) = c.ensure_server_key();
        assert_ne!(s, k1, "client and server identities are independent");
    }

    #[test]
    fn records_replace_and_evict_lru() {
        let mut c = SendspinConfig::default();
        for i in 0..(MAX_CLIENT_RECORDS + 2) {
            c.store_server_pairing(ServerPairing {
                server_id: format!("s{}", i),
                psk: b64url(&[i as u8; 32]),
                server_name: None,
                last_used_unix: 100 + i as u64,
            });
        }
        assert_eq!(c.server_pairings.len(), MAX_CLIENT_RECORDS);
        assert!(!c.server_pairings.iter().any(|r| r.server_id == "s0"));
        // Replacing keeps one record per server.
        c.store_server_pairing(ServerPairing {
            server_id: "s5".into(),
            psk: b64url(&[9; 32]),
            server_name: Some("x".into()),
            last_used_unix: 999,
        });
        assert_eq!(c.server_pairings.iter().filter(|r| r.server_id == "s5").count(), 1);
        assert!(c.remove_server_pairing_by_psk_id(&super::super::keys::psk_id(&[9; 32])));
        assert!(!c.server_pairings.iter().any(|r| r.server_id == "s5"));
    }

    #[test]
    fn players_are_pinned_by_instance() {
        let mut c = SendspinConfig::default();
        let rec = |psk: u8| PlayerPairing {
            psk: b64url(&[psk; 32]),
            name: None,
            instance: None,
        };
        c.player_pairings.insert("old".into(), rec(1));
        assert_eq!(c.pinned_client_id("Kitchen"), None);
        assert!(!c.pin_player("missing", "Kitchen"));
        assert!(c.pin_player("old", "Kitchen"));
        assert!(!c.pin_player("old", "kitchen"), "already pinned");
        assert_eq!(c.pinned_client_id("KITCHEN").as_deref(), Some("old"));
        // Paired again with a new identity: the new one replaces the pin.
        c.player_pairings.insert("new".into(), rec(2));
        assert!(c.pin_player("new", "Kitchen"));
        assert_eq!(c.pinned_client_id("kitchen").as_deref(), Some("new"));
        assert!(!c.player_pairings.contains_key("old"));
        // Older records without an instance still load.
        let r: PlayerPairing = serde_json::from_str(r#"{"psk":"x"}"#).unwrap();
        assert_eq!(r.instance, None);
    }

    #[test]
    fn config_roundtrips_through_json() {
        let mut c = SendspinConfig::default();
        c.source_enabled = true;
        c.ensure_pairing_psk();
        let s = serde_json::to_string(&c).unwrap();
        let back: SendspinConfig = serde_json::from_str(&s).unwrap();
        assert!(back.source_enabled);
        assert_eq!(back.pairing_psk, c.pairing_psk);
        let empty: SendspinConfig = serde_json::from_str("{}").unwrap();
        assert!(!empty.source_enabled);
    }
}
