//! Our Sendspin **server** role: discover Sendspin players
//! (`_sendspin._tcp` — ESPHome speakers, reference players), connect to
//! one, and push timestamped audio to it the way Music Assistant does, so
//! a player is usable without Music Assistant.
//!
//! Bring-up (server-initiated): WebSocket connect → the player's first
//! message picks the wire (`client/init` = Noise; `client/hello` = the
//! pre-encryption wire of older firmware) → `server/hello` / `client/hello`
//! → `server/activate {activities: [playback], active_roles: [player@v1,…]}`
//! → wait for the player's first `client/state` (its clock is synced) →
//! `stream/start` → audio chunks stamped with *our* monotonic clock, which
//! the player maps to its own via `client/time` ↔ `server/time`.
//!
//! Timeline: each chunk is sent `lead` ahead of its play time, where
//! `lead` = the player's `min_buffer_ms` + `output_delay_ms` (+ the user's
//! extra latency), capped so the in-flight bytes respect the player's
//! `buffer_capacity`. Sending is deadline-driven: a stalled input is
//! filled with silence so the timeline never freezes, and drift between
//! the PC's audio clock and ours is absorbed one frame at a time.
//!
//! Players that admit unpaired access play right away (selecting the
//! speaker is the operator approval). Others need pairing once: a pairing
//! token, or a pairing code the speaker shows/speaks (dynamic) or has
//! printed (static).

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use log::{debug, info, warn};
use mdns_sd::{ServiceEvent, ServiceInfo};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::channel::{self, msg_type, payload, ChannelWriter};
use super::flac::FlacEncoder;
use super::handshake::{self, FirstFrame, ResolvedPsk};
use super::keys::{b64url, b64url_32, decode_pairing_token, PskCategory};
use super::mdns;
use super::noise::x25519_public;
use super::pairing::{self, PairOutcome, PairingCtx};
use super::proto::{self, AudioFormat, Dialect, PairMethod, PlayerHello, PlayerState};
use super::pump::{spawn_pump, Event};
use super::resample::Resampler;
use super::store::{PlayerPairing, SendspinConfig};
use super::{instant_to_us, now_us, SendspinStore};
use crate::http_server::PcmFrame;
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

pub const ID_PREFIX: &str = "sendspin:";

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// A discovered Sendspin client endpoint (usually a player).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendspinRenderer {
    /// mDNS instance name (unique on the network).
    pub instance: String,
    /// Friendly name (TXT `name`, else the instance).
    pub friendly_name: String,
    pub ip: IpAddr,
    pub port: u16,
    pub path: String,
}

impl SendspinRenderer {
    pub fn stable_id(&self) -> String {
        format!("{}{}", ID_PREFIX, self.instance.to_ascii_lowercase())
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }

    pub fn url(&self) -> String {
        format!("ws://{}{}", self.addr(), self.path)
    }
}

#[derive(Default)]
pub struct SendspinDiscoveryState {
    players: Mutex<HashMap<String, SendspinRenderer>>,
}

impl SendspinDiscoveryState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn renderers(&self) -> Vec<SendspinRenderer> {
        let mut v: Vec<_> = self.players.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
        v
    }

    pub fn find_by_id(&self, id: &str) -> Option<SendspinRenderer> {
        let key = id.strip_prefix(ID_PREFIX)?;
        self.players.lock().unwrap().get(key).cloned()
    }

    /// Add a player by hand (manual address / tests).
    pub fn insert(&self, r: SendspinRenderer) {
        self.players.lock().unwrap().insert(r.instance.to_ascii_lowercase(), r);
    }

    fn remove_fullname(&self, fullname: &str) {
        let instance = instance_from_fullname(fullname).to_ascii_lowercase();
        self.players.lock().unwrap().remove(&instance);
    }
}

fn instance_from_fullname(fullname: &str) -> &str {
    fullname
        .strip_suffix(&format!(".{}", mdns::CLIENT_SERVICE))
        .unwrap_or(fullname)
}

fn renderer_from_info(info: &ServiceInfo) -> Option<SendspinRenderer> {
    let instance = instance_from_fullname(info.get_fullname()).to_string();
    let ip = info
        .get_addresses_v4()
        .into_iter()
        .copied()
        .find(|a| !a.is_loopback() && !a.is_link_local())
        .map(IpAddr::V4)?;
    let path = info
        .get_property_val_str("path")
        .map(|p| if p.starts_with('/') { p.to_string() } else { format!("/{}", p) })
        .filter(|p| super::ws::valid_path(p))
        .unwrap_or_else(|| mdns::DEFAULT_PATH.to_string());
    let friendly_name = info
        .get_property_val_str("name")
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| instance.clone());
    Some(SendspinRenderer {
        instance,
        friendly_name,
        ip,
        port: info.get_port(),
        path,
    })
}

/// Browse `_sendspin._tcp` in the background, skipping our own source
/// advertisement.
pub fn spawn_discovery(state: Arc<SendspinDiscoveryState>) -> Result<()> {
    let daemon = mdns::daemon()?;
    let rx = daemon
        .browse(mdns::CLIENT_SERVICE)
        .map_err(|e| anyhow!("mDNS browse {}: {}", mdns::CLIENT_SERVICE, e))?;
    std::thread::Builder::new()
        .name("sendspin-discovery".into())
        .spawn(move || loop {
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if mdns::is_own_instance(info.get_fullname()) || mdns::is_source_advert(&info) {
                        continue;
                    }
                    if let Some(r) = renderer_from_info(&info) {
                        debug!("Sendspin player resolved: {} @ {}", r.friendly_name, r.url());
                        state.insert(r);
                    }
                }
                Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                    debug!("Sendspin player removed: {}", fullname);
                    state.remove_fullname(&fullname);
                }
                Ok(ServiceEvent::SearchStopped(_)) => return,
                Ok(_) => {}
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => {
                    debug!("Sendspin mDNS browse ended");
                    return;
                }
            }
        })?;
    Ok(())
}

/// Stop the browse started by [`spawn_discovery`]; its thread exits.
pub fn stop_discovery() {
    if let Ok(d) = mdns::daemon() {
        let _ = d.stop_browse(mdns::CLIENT_SERVICE);
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// How to pair when the player requires it.
pub enum PairingInput {
    /// A version-0 pairing token (`SP:0…`) from the player.
    Token(String),
    /// A pairing code. `dynamic`: the player derives and shows/speaks it
    /// during this attempt; `ask` blocks until the operator typed it
    /// (`None` = cancelled). Static codes may be returned immediately.
    Code {
        dynamic: bool,
        ask: Box<dyn Fn() -> Option<String> + Send>,
    },
}

/// Title / artist / album for the player's display; `None` = nothing.
pub type NowPlayingFn = Arc<dyn Fn() -> Option<(String, String, String)> + Send + Sync>;

pub struct PlayerSessionConfig {
    pub renderer: SendspinRenderer,
    pub server_name: String,
    pub store: Arc<dyn SendspinStore>,
    pub samples_rx: Receiver<PcmFrame>,
    pub extra_latency_ms: u32,
    pub pairing: Option<PairingInput>,
    /// When set and the player shows metadata, forward now-playing.
    pub now_playing: Option<NowPlayingFn>,
    pub connect_timeout: Duration,
}

#[derive(Debug)]
pub enum StartError {
    /// The player only plays for a paired server. `methods` are what it
    /// offers; `lost_credential` = we had a pairing it no longer knows;
    /// `identity_changed` = we paired with a different identity at this
    /// speaker's name (reset, replaced, or an impostor), so it must be
    /// paired again before we stream to it.
    NeedsPairing {
        methods: Vec<PairMethod>,
        lost_credential: bool,
        identity_changed: bool,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::NeedsPairing { identity_changed: true, .. } => {
                write!(f, "the speaker's identity changed since it was paired; pair it again to confirm it")
            }
            StartError::NeedsPairing { lost_credential: true, .. } => {
                write!(f, "the speaker no longer recognises this computer's pairing; pair it again")
            }
            StartError::NeedsPairing { .. } => write!(f, "the speaker needs to be paired first"),
            StartError::Other(e) => write!(f, "{:#}", e),
        }
    }
}

impl From<anyhow::Error> for StartError {
    fn from(e: anyhow::Error) -> Self {
        StartError::Other(e)
    }
}

/// State shared between the session handle and its threads.
struct Shared {
    writer: Arc<ChannelWriter>,
    dialect: Dialect,
    stop: AtomicBool,
    dead: AtomicBool,
    state: Mutex<PlayerState>,
    can_volume: AtomicBool,
    can_mute: AtomicBool,
    volume: AtomicU32,
    /// Format being streamed (for the UI / logs).
    format: Mutex<Option<AudioFormat>>,
    /// The speaker ended the session with a goodbye that rules out an
    /// automatic reconnect (another server, shutdown, …).
    ended: Mutex<Option<String>>,
    /// Current send-ahead, µs (grows when the player asks for more buffer).
    lead_us: AtomicI64,
    extra_latency_ms: u32,
    buffer_capacity: u64,
}

/// A live connection streaming to one player.
pub struct PlayerSession {
    pub renderer: SendspinRenderer,
    pub player_name: String,
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl PlayerSession {
    pub fn start(cfg: PlayerSessionConfig) -> std::result::Result<PlayerSession, StartError> {
        let PlayerSessionConfig {
            renderer,
            server_name,
            store,
            samples_rx,
            extra_latency_ms,
            pairing,
            now_playing,
            connect_timeout,
        } = cfg;
        let server_priv = {
            let mut key = [0u8; 32];
            store.update(&mut |c: &mut SendspinConfig| {
                let (k, changed) = c.ensure_server_key();
                key = k;
                changed
            });
            key
        };
        let mut up = bring_up(&renderer, &server_name, &store, &server_priv, pairing, connect_timeout)?;
        let hello = up.hello.clone();
        let shared = Arc::new(Shared {
            writer: up.writer.clone(),
            dialect: up.dialect,
            stop: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            state: Mutex::new(up.state.clone()),
            can_volume: AtomicBool::new(false),
            can_mute: AtomicBool::new(false),
            volume: AtomicU32::new(up.state.volume.unwrap_or(100)),
            format: Mutex::new(None),
            ended: Mutex::new(None),
            lead_us: AtomicI64::new(0),
            extra_latency_ms,
            buffer_capacity: hello.buffer_capacity,
        });
        update_capabilities(&shared, &hello, &up.state);

        let fmt = choose_format(&hello.formats).ok_or_else(|| {
            up.writer.close();
            StartError::Other(anyhow!(
                "{} offers no audio format we can produce (formats: {:?})",
                hello.name,
                hello.formats
            ))
        })?;
        let lead_us = compute_lead_us(&up.state, extra_latency_ms, hello.buffer_capacity, &fmt);
        info!(
            "Sendspin: streaming to {} as {} {} Hz/{}-bit/{}ch, lead {} ms ({:?} wire)",
            hello.name,
            fmt.codec,
            fmt.sample_rate,
            fmt.bit_depth,
            fmt.channels,
            lead_us / 1000,
            up.dialect
        );
        *shared.format.lock().unwrap() = Some(fmt.clone());
        shared.lead_us.store(lead_us, Ordering::SeqCst);

        let mut threads = Vec::new();
        let events = up.events.take().expect("events");
        {
            let sh = shared.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("sendspin-player-ctl".into())
                    .spawn(move || control_loop(sh, events, hello))
                    .map_err(|e| StartError::Other(e.into()))?,
            );
        }
        {
            let sh = shared.clone();
            let fmt2 = fmt.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("sendspin-player-audio".into())
                    .spawn(move || {
                        if let Err(e) = audio_loop(&sh, samples_rx, fmt2) {
                            if !sh.stop.load(Ordering::SeqCst) {
                                warn!("Sendspin audio sender stopped: {:#}", e);
                                sh.dead.store(true, Ordering::SeqCst);
                            }
                        }
                    })
                    .map_err(|e| StartError::Other(e.into()))?,
            );
        }
        if up.metadata_active {
            let sh = shared.clone();
            let np = now_playing.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("sendspin-player-meta".into())
                    .spawn(move || metadata_loop(sh, np))
                    .map_err(|e| StartError::Other(e.into()))?,
            );
        }
        Ok(PlayerSession {
            renderer,
            player_name: up.hello.name.clone(),
            shared,
            threads,
        })
    }

    /// End the stream and close the connection.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.shared.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        // Let the sender finish its chunk first: nothing may follow
        // stream/end.
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let _ = self.shared.writer.send_json(&proto::stream_end_player(now_us()));
        self.shared.writer.close();
    }

    /// The connection dropped and a reconnect may bring it back.
    pub fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst) && self.ended_by_speaker().is_none()
    }

    /// The speaker ended the session itself (it switched to another
    /// server, is shutting down, …): reason, if so. No auto-reconnect.
    pub fn ended_by_speaker(&self) -> Option<String> {
        self.shared.ended.lock().unwrap().clone()
    }

    pub fn set_volume_pct(&self, pct: u32) -> Result<()> {
        if !self.shared.can_volume.load(Ordering::SeqCst) {
            bail!("this speaker does not accept volume changes");
        }
        let pct = pct.min(100);
        self.shared.volume.store(pct, Ordering::SeqCst);
        self.shared.writer.send_json(&proto::player_command_volume(pct))
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        if !self.shared.can_mute.load(Ordering::SeqCst) {
            bail!("this speaker does not accept mute");
        }
        self.shared.writer.send_json(&proto::player_command_mute(muted))
    }

    /// Last volume the player reported (or we set).
    pub fn volume(&self) -> Option<u32> {
        self.shared
            .can_volume
            .load(Ordering::SeqCst)
            .then(|| self.shared.volume.load(Ordering::SeqCst))
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.shared.format.lock().unwrap().clone()
    }
}

impl Drop for PlayerSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct BringUp {
    writer: Arc<ChannelWriter>,
    events: Option<Receiver<Event>>,
    dialect: Dialect,
    hello: PlayerHello,
    state: PlayerState,
    metadata_active: bool,
}

/// Answer `client/time` inline and wait for a message matching `pred`.
fn wait_for(writer: &ChannelWriter, events: &Receiver<Event>, timeout: Duration, what: &str, pred: &dyn Fn(&Value) -> bool) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(Event::Json { value, received_at }) => {
                if msg_type(&value) == "client/time" {
                    answer_time(writer, &value, received_at);
                    continue;
                }
                if msg_type(&value) == "client/goodbye" {
                    let reason = payload(&value).get("reason").and_then(Value::as_str).unwrap_or("?").to_string();
                    bail!("the speaker closed the connection ({})", reason.replace('_', " "));
                }
                if pred(&value) {
                    return Ok(value);
                }
            }
            Ok(Event::Binary { .. }) => {}
            Ok(Event::Rehandshake(r)) => {
                r.map_err(|e| anyhow!("re-handshake failed: {}", e))?;
            }
            Ok(Event::Closed(e)) => bail!("the speaker closed the connection{}", e.map(|e| format!(": {}", e)).unwrap_or_default()),
            Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for {}", what),
            Err(RecvTimeoutError::Disconnected) => bail!("connection lost waiting for {}", what),
        }
    }
}

fn answer_time(writer: &ChannelWriter, msg: &Value, received_at: Instant) {
    if let Some(t1) = payload(msg).get("client_transmitted").and_then(Value::as_i64) {
        let t2 = instant_to_us(received_at);
        let _ = writer.send_json(&proto::server_time(t1, t2, now_us()));
    }
}

fn pairing_psk_for(store: &dyn SendspinStore, client_id: &str) -> Option<[u8; 32]> {
    store
        .snapshot()
        .player_pairings
        .get(client_id)
        .and_then(|p| b64url_32(&p.psk).ok())
}

#[allow(clippy::too_many_lines)]
fn bring_up(
    renderer: &SendspinRenderer,
    server_name: &str,
    store: &Arc<dyn SendspinStore>,
    server_priv: &[u8; 32],
    mut pairing: Option<PairingInput>,
    timeout: Duration,
) -> std::result::Result<BringUp, StartError> {
    let host = format!("{}:{}", renderer.ip, renderer.port);
    let (mut ws_r, ws_w) = super::ws::connect(renderer.addr(), &host, &renderer.path, timeout)
        .with_context(|| format!("connecting to {}", renderer.url()))?;
    let first = handshake::read_first_frame(&mut ws_r, &ws_w, timeout)?;
    let server_id = b64url(&x25519_public(server_priv));
    // The identity we paired with at this speaker's name, if any.
    let pinned = store.snapshot().pinned_client_id(&renderer.instance);
    match first {
        FirstFrame::Refused(why) => Err(anyhow!("the speaker's protocol version is not supported ({})", why).into()),
        FirstFrame::LegacyHello { .. } if pinned.is_some() => {
            ws_w.close();
            Err(anyhow!(
                "{} was paired with this computer but now answers without encryption, so this computer won't stream to it \
                 (its firmware may have been downgraded, or another device is using its name)",
                renderer.friendly_name
            )
            .into())
        }
        FirstFrame::LegacyHello { value } => {
            // Pre-encryption wire: plaintext, the hello carries everything.
            let hello = proto::parse_player_hello(payload(&value));
            if !hello.roles.iter().any(|r| r == "player@v1") {
                return Err(anyhow!("{} is not a Sendspin speaker (roles: {:?})", hello.name, hello.roles).into());
            }
            let roles = roles_to_activate(&hello);
            let (reader, writer) = channel::channel(ws_r, ws_w, None);
            writer.send_json(&proto::legacy_server_hello(&server_id, server_name, &roles))?;
            let events = spawn_pump(reader, "player", Box::new(|_, _| None));
            let guard = CloseGuard(Some(writer.clone()));
            let state_msg = wait_for(&writer, &events, Duration::from_secs(10), "the speaker's state", &|v| msg_type(v) == "client/state")?;
            let state = proto::parse_player_state(payload(&state_msg));
            let _ = writer.send_json(&proto::group_update(true, &group_id(), server_name));
            guard.disarm();
            Ok(BringUp {
                writer,
                events: Some(events),
                dialect: Dialect::V9,
                metadata_active: roles.iter().any(|r| r == "metadata@v1"),
                hello,
                state,
            })
        }
        FirstFrame::Noise { raw, client_id, suite } => {
            // Which PSK do we reference?
            let token = match &pairing {
                Some(PairingInput::Token(t)) => Some(decode_pairing_token(t).context("pairing token")?),
                _ => None,
            };
            if let Some((key, _)) = &token {
                if b64url(key) != client_id {
                    return Err(anyhow!("that pairing token belongs to a different speaker").into());
                }
            }
            let lt = pairing_psk_for(&**store, &client_id);
            let psk = match (&token, lt) {
                (Some((_, p)), _) => ResolvedPsk::new(*p, PskCategory::Pairing),
                (None, Some(p)) if pairing.is_none() => ResolvedPsk::new(p, PskCategory::LongTerm),
                _ => ResolvedPsk::sentinel(),
            };
            let referenced_lt = psk.category == PskCategory::LongTerm;
            let hs = handshake::server_handshake(ws_r, ws_w, &raw, &client_id, suite, server_priv, psk, timeout)?;
            let writer = hs.writer.clone();
            let suite = hs.suite;
            let mut handshake_hash = hs.handshake_hash;
            let mut category = hs.psk.category;
            // The pump completes server-initiated re-handshakes: we stage
            // the initiator state, it reads message 2 and switches keys.
            let staged: Arc<Mutex<Option<super::noise::Handshake>>> = Arc::new(Mutex::new(None));
            let hook = {
                let staged = staged.clone();
                let writer = writer.clone();
                Box::new(move |reader: &mut channel::ChannelReader, msg: &Value| {
                    let Some(mut hs) = staged.lock().unwrap().take() else {
                        return None;
                    };
                    let res = (|| -> Result<[u8; 32]> {
                        let data = payload(msg).get("data").and_then(Value::as_str).ok_or_else(|| anyhow!("no data"))?;
                        let m2 = super::keys::b64url_decode(data)?;
                        hs.read_message_2(&m2)?;
                        let h = hs.handshake_hash();
                        let (send, recv) = hs.into_transport()?.split();
                        writer.swap_cipher(send);
                        reader.swap_cipher(recv);
                        Ok(h)
                    })();
                    Some(Event::Rehandshake(res.map(|h| (ResolvedPsk::sentinel(), h)).map_err(|e| format!("{:#}", e))))
                })
            };
            let events = spawn_pump(hs.reader, "player", hook);
            let guard = CloseGuard(Some(writer.clone()));

            writer.send_json(&proto::server_hello(server_name))?;
            let hello_msg = wait_for(&writer, &events, Duration::from_secs(10), "client/hello", &|v| msg_type(v) == "client/hello")?;
            let mut hello = proto::parse_player_hello(payload(&hello_msg));
            let dialect = hello.dialect.unwrap_or(Dialect::V9);
            if !hello.roles.iter().any(|r| r == "player@v1") {
                return Err(anyhow!("{} is not a Sendspin speaker (roles: {:?})", hello.name, hello.roles).into());
            }

            if pinned.as_deref().is_some_and(|p| p != client_id) && pairing.is_none() {
                warn!("Sendspin: {} answers with a different identity than the one paired at that name", hello.name);
                return Err(StartError::NeedsPairing {
                    methods: hello.pair_methods.clone(),
                    lost_credential: false,
                    identity_changed: true,
                });
            }
            if token.is_some() && category != PskCategory::Pairing {
                // Message 2 verified only under the Sentinel: the speaker
                // does not hold that token's PSK, so nothing may be paired
                // over this connection.
                return Err(anyhow!("{} did not accept that pairing token (it may be out of date); get a current one from the speaker", hello.name).into());
            }
            if hs.credential_mismatch && pairing.is_none() {
                return Err(StartError::NeedsPairing {
                    methods: hello.pair_methods.clone(),
                    lost_credential: true,
                    identity_changed: false,
                });
            }
            let mut pairing_index = 0u32;
            if category != PskCategory::LongTerm {
                let may_play_unpaired = category == PskCategory::Sentinel && hello.unpaired_access && pairing.is_none();
                if !may_play_unpaired {
                    let Some(input) = pairing.take() else {
                        return Err(StartError::NeedsPairing {
                            methods: hello.pair_methods.clone(),
                            lost_credential: referenced_lt,
                            identity_changed: false,
                        });
                    };
                    let (method, ask) = match input {
                        PairingInput::Token(_) => (PairMethod::PairingPsk, None),
                        PairingInput::Code { dynamic, ask } => {
                            let m = if dynamic { PairMethod::DynamicCode } else { PairMethod::StaticCode };
                            (m, Some(ask))
                        }
                    };
                    if !hello.pair_methods.contains(&method) {
                        return Err(anyhow!("{} does not offer that pairing method", hello.name).into());
                    }
                    pairing_index += 1;
                    writer.send_json(&proto::server_activate(dialect, false, Some((method, Some(6))), Some(&[])))?;
                    let ctx = PairingCtx {
                        dialect,
                        writer: &writer,
                        events: &events,
                        handshake_hash,
                        pairing_index,
                        suite,
                    };
                    let outcome = match (&method, ask) {
                        (PairMethod::PairingPsk, _) => pairing::server_pairing_psk(&ctx)?,
                        (m, Some(ask)) => pairing::server_code(&ctx, *m == PairMethod::DynamicCode, &*ask)?,
                        _ => unreachable!(),
                    };
                    let lt_psk = match outcome {
                        PairOutcome::Finalized { long_term_psk } => long_term_psk,
                        PairOutcome::Aborted(reason) => {
                            return Err(anyhow!("pairing was not completed ({})", reason.replace('_', " ")).into())
                        }
                        PairOutcome::CodeMismatch => return Err(anyhow!("pairing was not completed (the code did not match)").into()),
                        PairOutcome::Left(_) => return Err(anyhow!("pairing ended unexpectedly").into()),
                    };
                    let name = hello.name.clone();
                    let cid = client_id.clone();
                    store.update(&mut |c: &mut SendspinConfig| {
                        c.player_pairings.insert(
                            cid.clone(),
                            PlayerPairing {
                                psk: b64url(&lt_psk),
                                name: Some(name.clone()),
                                instance: None,
                            },
                        );
                        c.pin_player(&cid, &renderer.instance);
                        true
                    });
                    pairing::server_send_finalize(&writer)?;
                    info!("Sendspin: paired with {}", hello.name);
                    // Re-handshake onto the new long-term PSK.
                    let lt = ResolvedPsk::new(lt_psk, PskCategory::LongTerm);
                    let client_pub = super::keys::peer_id_to_key(&client_id)?;
                    let mut ini = super::noise::Handshake::initiator(suite, server_priv, &client_pub, &handshake_hash, &lt.psk);
                    let p1 = serde_json::json!({ "psk_id": lt.psk_id, "psk_category": "lt" }).to_string();
                    let m1 = ini.write_message_1(p1.as_bytes())?;
                    *staged.lock().unwrap() = Some(ini);
                    writer.send_json(&channel::envelope("noise/handshake", serde_json::json!({ "data": b64url(&m1) })))?;
                    // Wait for the pump to switch keys.
                    let deadline = Instant::now() + Duration::from_secs(15);
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        match events.recv_timeout(left) {
                            Ok(Event::Rehandshake(Ok((_, h)))) => {
                                handshake_hash = h;
                                break;
                            }
                            Ok(Event::Rehandshake(Err(e))) => return Err(anyhow!("re-handshake failed: {}", e).into()),
                            Ok(Event::Closed(e)) => return Err(anyhow!("connection closed after pairing: {:?}", e).into()),
                            Ok(_) => continue,
                            Err(_) => return Err(anyhow!("timed out re-keying after pairing").into()),
                        }
                    }
                    let _ = handshake_hash;
                    category = PskCategory::LongTerm;
                    if dialect == Dialect::V9 {
                        // aiosendspin 9.x re-exchanges hellos after the
                        // re-handshake.
                        writer.send_json(&proto::server_hello(server_name))?;
                        let h2 = wait_for(&writer, &events, Duration::from_secs(10), "client/hello", &|v| msg_type(v) == "client/hello")?;
                        hello = proto::parse_player_hello(payload(&h2));
                    }
                }
            }
            if category == PskCategory::LongTerm {
                // Records made before pinning existed get pinned on first use.
                let cid = client_id.clone();
                store.update(&mut |c: &mut SendspinConfig| c.pin_player(&cid, &renderer.instance));
            }
            debug!("Sendspin: {} playing on a {:?} connection", hello.name, category);
            let roles = roles_to_activate(&hello);
            writer.send_json(&proto::server_activate(dialect, true, None, Some(&roles)))?;
            let _ = writer.send_json(&proto::group_update(false, &group_id(), server_name));
            let state_msg = wait_for(&writer, &events, Duration::from_secs(15), "the speaker's state", &|v| {
                msg_type(v) == "client/state"
            })?;
            let mut state = proto::parse_player_state(payload(&state_msg));
            if state.available == Some(false) {
                // Current players report unavailable until their clock has
                // converged; give the time sync a moment before deciding
                // the speaker is busy with something else.
                let deadline = Instant::now() + Duration::from_secs(8);
                while state.available == Some(false) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(anyhow!("{} is busy with another source right now", hello.name).into());
                    }
                    let next = wait_for(&writer, &events, left, "the speaker to become available", &|v| msg_type(v) == "client/state")
                        .map_err(|_| anyhow!("{} is busy with another source right now", hello.name))?;
                    let upd = proto::parse_player_state(payload(&next));
                    merge_state(&mut state, &upd);
                }
            }
            guard.disarm();
            Ok(BringUp {
                writer,
                events: Some(events),
                dialect,
                metadata_active: roles.iter().any(|r| r == "metadata@v1"),
                hello,
                state,
            })
        }
    }
}

/// Closes a half-built connection on error paths (the reader thread
/// would otherwise keep the socket open).
struct CloseGuard(Option<Arc<ChannelWriter>>);

impl CloseGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        if let Some(w) = self.0.take() {
            w.close();
        }
    }
}

/// We stream to one player at a time: one fixed solo group.
fn group_id() -> String {
    "stream-to-speaker".to_string()
}

/// Roles we activate: the player, plus metadata when offered.
fn roles_to_activate(hello: &PlayerHello) -> Vec<String> {
    let mut roles = vec!["player@v1".to_string()];
    if hello.roles.iter().any(|r| r == "metadata@v1") {
        roles.push("metadata@v1".to_string());
    }
    roles
}

fn update_capabilities(sh: &Shared, hello: &PlayerHello, state: &PlayerState) {
    let mut cmds: Vec<String> = hello.hello_commands.clone();
    if let Some(c) = &state.state_commands {
        cmds.extend(c.iter().cloned());
    }
    sh.can_volume.store(cmds.iter().any(|c| c == "volume"), Ordering::SeqCst);
    sh.can_mute.store(cmds.iter().any(|c| c == "mute"), Ordering::SeqCst);
    if let Some(v) = state.volume {
        sh.volume.store(v.min(100), Ordering::SeqCst);
    }
}

/// Sample rates we resample to (an arbitrary rate could need a huge
/// filter).
const STANDARD_RATES: [u32; 11] = [8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Pick the stream format: the player's highest-priority PCM/FLAC entry,
/// but prefer one at our native 44.1 kHz (no resampling) when offered.
pub fn choose_format(formats: &[AudioFormat]) -> Option<AudioFormat> {
    let producible = |f: &&AudioFormat| {
        matches!(f.codec.as_str(), "pcm" | "flac")
            && (1..=2).contains(&f.channels)
            && matches!(f.bit_depth, 16 | 24 | 32)
            && STANDARD_RATES.contains(&f.sample_rate)
            && !(f.codec == "flac" && f.bit_depth == 32)
    };
    formats
        .iter()
        .filter(producible)
        .find(|f| f.sample_rate == WIRE_SAMPLE_RATE)
        .or_else(|| formats.iter().find(producible))
        .cloned()
}

/// How far ahead of play time we send each chunk, µs.
pub fn compute_lead_us(state: &PlayerState, extra_ms: u32, buffer_capacity: u64, fmt: &AudioFormat) -> i64 {
    let min_buffer = state.min_buffer_ms.unwrap_or(500) as i64;
    let delay = state.output_delay_ms.unwrap_or(0) as i64;
    let mut lead_ms = (min_buffer + delay + extra_ms as i64).clamp(150, 12_000);
    if buffer_capacity > 0 {
        // Chunks in flight ≈ lead worth of audio (PCM bound; FLAC is smaller).
        let bytes_per_ms = (fmt.sample_rate as u64 * fmt.channels as u64 * (fmt.bit_depth as u64 / 8)) as f64 / 1000.0;
        let per_chunk_overhead = 13.0 / 20.0;
        let max_ms = (buffer_capacity as f64 * 0.9 / (bytes_per_ms + per_chunk_overhead)) as i64 - 20;
        if max_ms > 50 && lead_ms > max_ms {
            lead_ms = max_ms;
        }
    }
    lead_ms * 1000
}

fn control_loop(sh: Arc<Shared>, events: Receiver<Event>, hello: PlayerHello) {
    loop {
        if sh.stop.load(Ordering::SeqCst) {
            return;
        }
        let ev = match events.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                sh.dead.store(true, Ordering::SeqCst);
                return;
            }
        };
        match ev {
            Event::Json { value, received_at } => match msg_type(&value) {
                "client/time" => answer_time(&sh.writer, &value, received_at),
                "client/state" => {
                    let st = proto::parse_player_state(payload(&value));
                    {
                        let mut cur = sh.state.lock().unwrap();
                        merge_state(&mut cur, &st);
                        if let Some(fmt) = sh.format.lock().unwrap().clone() {
                            let lead = compute_lead_us(&cur, sh.extra_latency_ms, sh.buffer_capacity, &fmt);
                            sh.lead_us.store(lead, Ordering::SeqCst);
                        }
                    }
                    update_capabilities(&sh, &hello, &st);
                    if st.available == Some(false) {
                        info!("Sendspin: {} reports it is busy with another source", hello.name);
                    }
                }
                "client/goodbye" => {
                    let reason = payload(&value).get("reason").and_then(Value::as_str).unwrap_or("?");
                    info!("Sendspin: {} said goodbye ({})", hello.name, reason);
                    if reason != "restart" {
                        // Spec: no automatic reconnect for these reasons.
                        *sh.ended.lock().unwrap() = Some(reason.to_string());
                    }
                    sh.dead.store(true, Ordering::SeqCst);
                    sh.writer.close();
                    return;
                }
                other => debug!("Sendspin player: ignoring {}", other),
            },
            Event::Binary { .. } => {}
            Event::Rehandshake(_) => {}
            Event::Closed(e) => {
                if !sh.stop.load(Ordering::SeqCst) {
                    warn!("Sendspin: connection to {} lost{}", hello.name, e.map(|e| format!(": {}", e)).unwrap_or_default());
                    sh.dead.store(true, Ordering::SeqCst);
                }
                return;
            }
        }
    }
}

fn merge_state(cur: &mut PlayerState, upd: &PlayerState) {
    macro_rules! take {
        ($f:ident) => {
            if upd.$f.is_some() {
                cur.$f = upd.$f.clone();
            }
        };
    }
    take!(available);
    take!(volume);
    take!(muted);
    take!(output_delay_ms);
    take!(required_lead_time_ms);
    take!(min_buffer_ms);
    take!(state_commands);
}

fn metadata_loop(sh: Arc<Shared>, np: Option<NowPlayingFn>) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut last: Option<Option<(String, String, String)>> = None;
    while !sh.stop.load(Ordering::SeqCst) {
        let cur = np.as_ref().and_then(|f| f());
        if last.as_ref() != Some(&cur) {
            let (t, a, al) = match &cur {
                Some((t, a, al)) => (Some(t.as_str()), Some(a.as_str()), Some(al.as_str())),
                None => (None, None, None),
            };
            if sh
                .writer
                .send_json(&proto::server_state_metadata(sh.dialect, now_us(), t, a, al))
                .is_err()
            {
                return;
            }
            last = Some(cur);
        }
        for _ in 0..20 {
            if sh.stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Converts our 44.1 kHz s16 stereo into the negotiated format.
struct Converter {
    resampler: Resampler,
    out_channels: usize,
}

impl Converter {
    fn new(fmt: &AudioFormat) -> Self {
        Self {
            resampler: Resampler::new(WIRE_SAMPLE_RATE, fmt.sample_rate, WIRE_CHANNELS as usize),
            out_channels: fmt.channels as usize,
        }
    }

    /// Input: LE s16 stereo bytes. Output appended as interleaved i16 at
    /// the target rate and channel count.
    fn push(&mut self, bytes: &[u8], ring: &mut VecDeque<i16>) {
        let input: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut out = Vec::with_capacity(input.len() * 12 / 10 + 8);
        self.resampler.process(&input, &mut out);
        if self.out_channels == 1 {
            ring.extend(out.chunks_exact(2).map(|f| ((f[0] as i32 + f[1] as i32) / 2) as i16));
        } else {
            ring.extend(out);
        }
    }
}

fn encode_pcm(samples: &[i16], bit_depth: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * (bit_depth as usize / 8));
    for &s in samples {
        match bit_depth {
            16 => out.extend_from_slice(&s.to_le_bytes()),
            24 => {
                let v = (s as i32) << 8;
                out.extend_from_slice(&v.to_le_bytes()[..3]);
            }
            _ => out.extend_from_slice(&((s as i32) << 16).to_le_bytes()),
        }
    }
    out
}

fn audio_loop(sh: &Shared, rx: Receiver<PcmFrame>, fmt: AudioFormat) -> Result<()> {
    let mut lead_us = sh.lead_us.load(Ordering::SeqCst);
    let rate = fmt.sample_rate as i64;
    let ch = fmt.channels as usize;
    let chunk_frames = (fmt.sample_rate / 50) as usize; // 20 ms
    let mut conv = Converter::new(&fmt);
    let mut flac = (fmt.codec == "flac").then(|| FlacEncoder::new(fmt.sample_rate, fmt.channels, fmt.bit_depth, chunk_frames as u16));
    let header = flac
        .as_ref()
        .map(|e| base64::engine::general_purpose::STANDARD.encode(e.codec_header()));
    let mut ring: VecDeque<i16> = VecDeque::with_capacity(chunk_frames * ch * 8);
    // Drop whatever queued up during bring-up: start from live audio.
    while rx.try_recv().is_ok() {}

    let mut anchor_us = now_us() + lead_us;
    let mut frames_sent: i64 = 0;
    sh.writer.send_json(&proto::stream_start_player(&fmt, header.as_deref(), now_us()))?;
    let _ = sh.writer.send_json(&proto::group_update(true, &group_id(), "Stream To Speaker"));
    let mut last_input = Instant::now();
    // Input cushion kept in the ring to ride out 10 ms input granularity
    // and scheduling jitter; drift is corrected around it with a deadband.
    let cushion = chunk_frames * 2; // ~40 ms
    let deadband = chunk_frames; // ±20 ms
    let mut filling = true;
    loop {
        if sh.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        // The player asked for more buffer (min_buffer / output delay):
        // push the timeline later by the difference. A smaller lead is kept
        // as is — moving earlier would overlap audio already sent.
        let wanted = sh.lead_us.load(Ordering::SeqCst);
        if wanted > lead_us + 20_000 {
            anchor_us += wanted - lead_us;
            lead_us = wanted;
        }
        let ts = anchor_us + frames_sent * 1_000_000 / rate;
        let send_at = ts - lead_us;
        // Pull input until it is time to send this chunk.
        loop {
            let now = now_us();
            if now >= send_at {
                break;
            }
            let wait = Duration::from_micros(((send_at - now) as u64).min(5_000));
            match rx.recv_timeout(wait) {
                Ok(frame) => {
                    conv.push(&frame.0, &mut ring);
                    last_input = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if sh.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        while let Ok(frame) = rx.try_recv() {
            conv.push(&frame.0, &mut ring);
            last_input = Instant::now();
        }
        let now = now_us();
        if now - send_at > 500_000 {
            // We stalled (suspend, debugger): re-glue the timeline instead
            // of bursting stale audio.
            debug!("Sendspin sender stalled {} ms; re-anchoring", (now - send_at) / 1000);
            ring.clear();
            anchor_us = now + lead_us;
            frames_sent = 0;
            filling = true;
            continue;
        }
        let backlog = ring.len() / ch;
        let input_live = last_input.elapsed() < Duration::from_millis(100);
        if filling && backlog >= chunk_frames + cushion {
            filling = false;
        } else if !filling && backlog < chunk_frames {
            // Underrun: play silence until the cushion is rebuilt, rather
            // than chopping a partial chunk with zeros every time.
            filling = true;
        }
        let mut chunk: Vec<i16> = Vec::with_capacity(chunk_frames * ch);
        if filling {
            chunk.resize(chunk_frames * ch, 0);
            if !input_live {
                ring.clear(); // idle input: nothing worth keeping
            }
        } else {
            let mut take = chunk_frames;
            let target = chunk_frames + cushion;
            if backlog > target + deadband {
                // Input clock faster than ours: skip one frame.
                for _ in 0..ch {
                    ring.pop_front();
                }
            } else if backlog + deadband < target && input_live {
                // Slower: repeat one frame.
                take = chunk_frames - 1;
            }
            for _ in 0..take * ch {
                chunk.push(ring.pop_front().unwrap_or(0));
            }
            if take < chunk_frames {
                let last: Vec<i16> = chunk[chunk.len().saturating_sub(ch)..].to_vec();
                chunk.extend(last);
            }
            chunk.resize(chunk_frames * ch, 0);
        }
        // Bound the backlog even under a runaway input.
        let cap = (chunk_frames * 10) * ch;
        while ring.len() > cap {
            ring.pop_front();
        }
        let payload = match flac.as_mut() {
            Some(enc) => {
                let shift = fmt.bit_depth as i32 - 16;
                let wide: Vec<i32> = chunk.iter().map(|&s| (s as i32) << shift).collect();
                enc.encode(&wide)
            }
            None => encode_pcm(&chunk, fmt.bit_depth),
        };
        sh.writer
            .send_binary(&proto::audio_chunk(sh.dialect, ts, now_us(), &payload))
            .context("sending audio")?;
        frames_sent += chunk_frames as i64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(codec: &str, rate: u32, bits: u16) -> AudioFormat {
        AudioFormat {
            codec: codec.into(),
            sample_rate: rate,
            channels: 2,
            bit_depth: bits,
        }
    }

    #[test]
    fn format_choice_prefers_native_rate_then_priority() {
        let fmts = vec![f("opus", 48_000, 16), f("flac", 48_000, 24), f("pcm", 44_100, 16)];
        assert_eq!(choose_format(&fmts).unwrap(), f("pcm", 44_100, 16));
        let fmts = vec![f("opus", 48_000, 16), f("flac", 48_000, 24), f("pcm", 48_000, 16)];
        assert_eq!(choose_format(&fmts).unwrap(), f("flac", 48_000, 24));
        assert!(choose_format(&[f("opus", 48_000, 16)]).is_none());
        assert!(choose_format(&[f("pcm", 44_101, 16)]).is_none());
        assert_eq!(choose_format(&[f("pcm", 47_999, 16), f("pcm", 96_000, 24)]).unwrap(), f("pcm", 96_000, 24));
    }

    #[test]
    fn lead_respects_buffer_capacity() {
        let st = PlayerState {
            min_buffer_ms: Some(1000),
            output_delay_ms: Some(50),
            ..Default::default()
        };
        let fmt = f("pcm", 48_000, 16);
        assert_eq!(compute_lead_us(&st, 0, 0, &fmt), 1_050_000);
        // 100 KB of buffer at 192 KB/s caps the lead well below 1 s.
        let capped = compute_lead_us(&st, 0, 100_000, &fmt);
        assert!(capped < 500_000 && capped > 300_000, "{}", capped);
        // Unknown min buffer falls back to 500 ms; floor of 150 ms.
        let st0 = PlayerState { min_buffer_ms: Some(0), ..Default::default() };
        assert_eq!(compute_lead_us(&st0, 0, 0, &fmt), 150_000);
    }

    #[test]
    fn pcm_encoding_widths() {
        assert_eq!(encode_pcm(&[0x1234], 16), vec![0x34, 0x12]);
        assert_eq!(encode_pcm(&[0x1234], 24), vec![0x00, 0x34, 0x12]);
        assert_eq!(encode_pcm(&[-1], 32), vec![0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn ids_roundtrip_through_discovery_state() {
        let st = SendspinDiscoveryState::new();
        let r = SendspinRenderer {
            instance: "Kitchen Speaker".into(),
            friendly_name: "Kitchen".into(),
            ip: "192.168.1.5".parse().unwrap(),
            port: 8928,
            path: "/sendspin".into(),
        };
        st.insert(r.clone());
        assert_eq!(r.stable_id(), "sendspin:kitchen speaker");
        assert_eq!(st.find_by_id(&r.stable_id()), Some(r.clone()));
        assert_eq!(r.url(), "ws://192.168.1.5:8928/sendspin");
        st.remove_fullname("Kitchen Speaker._sendspin._tcp.local.");
        assert!(st.renderers().is_empty());
    }
}
