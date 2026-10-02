//! Our Sendspin **source** client: Music Assistant's "Sendspin Source"
//! plugin exposes every paired client with the `source@v1` role as a Live
//! Input that can play on any Music Assistant player. We advertise
//! `_sendspin._tcp`, the server connects to us (server-initiated is the
//! recommended mode), and when it sends `server/command {source: start}`
//! we stream what Windows plays: PCM, 44.1 kHz / 16-bit / stereo — the
//! server resamples to its own 48 kHz pipeline, so we send the native rate
//! untouched.
//!
//! Music Assistant only activates `source@v1` on a **paired** connection,
//! so the first connection is unpaired (Sentinel PSK) and parks at empty
//! activities until the user pairs from Music Assistant's device settings
//! with either our pairing token or the 6-digit code we display.
//!
//! Signal presence (`line_sense`) is reported from the audio itself:
//! present while the PC plays something audible, absent after a few
//! seconds of digital silence — Music Assistant can use it to start the
//! input automatically on a chosen player.

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use log::{debug, info, warn};
use serde_json::Value;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::channel::{msg_type, payload, ChannelWriter};
use super::handshake::{self, ResolvedPsk};
use super::keys::{self, b64url, b64url_32, PskCategory};
use super::mdns;
use super::noise::{x25519_public, Suite};
use super::pairing::{self, PairOutcome, PairingCtx};
use super::proto::{self, Activation, AudioFormat, Dialect, PairMethod};
use super::pump::{spawn_pump, Event};
use super::store::{SendspinConfig, ServerPairing};
use super::time_filter::TimeFilter;
use super::{instant_to_us, now_us, unix_now, SendspinStore};
use crate::http_server::{PcmFrame, StreamHub};
use crate::{WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Our capture chunks: 20 ms (spec bounds 5–150 ms).
const CHUNK_FRAMES: usize = WIRE_SAMPLE_RATE as usize / 50;
const BYTES_PER_FRAME: usize = WIRE_CHANNELS as usize * 2;
/// Absolute sample value counted as "audible" for line sensing.
const SIGNAL_THRESHOLD: i16 = 32;
const SIGNAL_ABSENT_AFTER: Duration = Duration::from_secs(8);
/// Dynamic pairing code length we accept/offer.
const MIN_CODE_DIGITS: u32 = 6;

/// What the GUI shows for the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceState {
    /// Advertised; no server connected.
    Waiting,
    /// A server is connected but has not paired with us yet.
    NeedsPairing { server: String },
    /// Paired and connected; `streaming` while the input is playing.
    Connected { server: String, streaming: bool },
    /// Could not start (port, mDNS).
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct SourceOptions {
    pub name: String,
    pub port: u16,
    pub software_version: String,
}

struct Admitted {
    conn_id: u64,
    rank: u8,
    server_id: String,
    pairing_attempt: bool,
    writer: Arc<ChannelWriter>,
}

struct Shared {
    opts: SourceOptions,
    hub: Arc<StreamHub>,
    store: Arc<dyn SendspinStore>,
    stop: AtomicBool,
    next_conn: AtomicU64,
    state: Mutex<SourceState>,
    /// Code on display for an in-progress dynamic pairing: (code, server).
    code: Mutex<Option<(String, String)>>,
    admitted: Mutex<Option<Admitted>>,
    last_event: Mutex<Option<(String, Instant)>>,
}

/// A running source service. Dropping it stops everything.
pub struct SourceService {
    shared: Arc<Shared>,
    mdns_fullname: Option<String>,
    port: u16,
}

impl SourceService {
    /// Bind the listener, advertise, and start accepting servers.
    pub fn start(opts: SourceOptions, hub: Arc<StreamHub>, store: Arc<dyn SendspinStore>) -> Result<SourceService> {
        // Make sure our identity + pairing PSK exist before advertising.
        store.update(&mut |c: &mut SendspinConfig| {
            let (_, a) = c.ensure_client_key();
            let (_, b) = c.ensure_pairing_psk();
            a || b
        });
        let listener = TcpListener::bind(("0.0.0.0", opts.port))
            .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))
            .context("binding the Sendspin source listener")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            opts: opts.clone(),
            hub,
            store,
            stop: AtomicBool::new(false),
            next_conn: AtomicU64::new(1),
            state: Mutex::new(SourceState::Waiting),
            code: Mutex::new(None),
            admitted: Mutex::new(None),
            last_event: Mutex::new(None),
        });
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("sendspin-source-accept".into())
            .spawn(move || accept_loop(sh, listener))?;
        let mdns_fullname = match mdns::register_client(&opts.name, &opts.name, port) {
            Ok(f) => Some(f),
            Err(e) => {
                warn!("Sendspin source: mDNS advertisement failed: {:#}", e);
                *shared.state.lock().unwrap() = SourceState::Failed(format!("mDNS advertisement failed: {:#}", e));
                None
            }
        };
        info!("Sendspin source listening on port {} as {:?}", port, opts.name);
        Ok(SourceService {
            shared,
            mdns_fullname,
            port,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn state(&self) -> SourceState {
        self.shared.state.lock().unwrap().clone()
    }

    /// The 6-digit code to show while a server is pairing with us.
    pub fn pairing_code(&self) -> Option<(String, String)> {
        self.shared.code.lock().unwrap().clone()
    }

    /// A short status note (pairing results etc.), with its time.
    pub fn last_event(&self) -> Option<(String, Instant)> {
        self.shared.last_event.lock().unwrap().clone()
    }

    /// The pairing token to paste into Music Assistant.
    pub fn pairing_token(&self) -> String {
        pairing_token(&*self.shared.store)
    }

    pub fn stop(&self) {
        if self.shared.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(f) = &self.mdns_fullname {
            mdns::unregister(f);
        }
        if let Some(a) = self.shared.admitted.lock().unwrap().take() {
            let _ = a.writer.send_json(&proto::client_goodbye("shutdown"));
            a.writer.close();
        }
    }
}

impl Drop for SourceService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The pairing token for our client identity + pairing PSK.
pub fn pairing_token(store: &dyn SendspinStore) -> String {
    let mut token = String::new();
    store.update(&mut |c: &mut SendspinConfig| {
        let (k, a) = c.ensure_client_key();
        let (p, b) = c.ensure_pairing_psk();
        token = keys::encode_pairing_token(&x25519_public(&k), &p);
        a || b
    });
    token
}

fn accept_loop(sh: Arc<Shared>, listener: TcpListener) {
    while !sh.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let _ = stream.set_nonblocking(false);
                let sh2 = sh.clone();
                let _ = std::thread::Builder::new()
                    .name("sendspin-source-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle_socket(&sh2, stream, peer) {
                            debug!("Sendspin source connection from {} ended: {:#}", peer, e);
                        }
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                warn!("Sendspin source accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn handle_socket(sh: &Arc<Shared>, stream: TcpStream, peer: SocketAddr) -> Result<()> {
    let (r, w) = match super::ws::accept(stream, mdns::DEFAULT_PATH, Duration::from_secs(10))? {
        super::ws::Accepted::WebSocket(r, w) => (r, w),
        super::ws::Accepted::Rejected(what) => bail!("not a Sendspin request: {}", what),
    };
    let conn_id = sh.next_conn.fetch_add(1, Ordering::SeqCst);
    let result = run_connection(sh, conn_id, r, w, peer);
    // Release admission (if we held it) and reset the GUI state.
    {
        let mut adm = sh.admitted.lock().unwrap();
        if adm.as_ref().map(|a| a.conn_id) == Some(conn_id) {
            *adm = None;
            *sh.state.lock().unwrap() = SourceState::Waiting;
        }
    }
    if sh.code.lock().unwrap().is_some() {
        *sh.code.lock().unwrap() = None;
    }
    result
}

fn note(sh: &Shared, msg: impl Into<String>) {
    let m = msg.into();
    info!("Sendspin source: {}", m);
    *sh.last_event.lock().unwrap() = Some((m, Instant::now()));
}

/// Resolve a `psk_id` against our pairing PSK and long-term records.
fn make_resolver(store: Arc<dyn SendspinStore>) -> impl Fn(&str, Option<PskCategory>) -> Option<(ResolvedPsk, Option<String>)> + Send + Sync + Clone {
    move |psk_id: &str, cat: Option<PskCategory>| {
        let cfg = store.snapshot();
        if let Some(p) = cfg.pairing_psk.as_deref().and_then(|s| b64url_32(s).ok()) {
            let r = ResolvedPsk::new(p, PskCategory::Pairing);
            if r.psk_id == psk_id && cat.map(|c| c == PskCategory::Pairing).unwrap_or(true) {
                return Some((r, None));
            }
        }
        if cat.map(|c| c == PskCategory::LongTerm).unwrap_or(true) {
            for rec in &cfg.server_pairings {
                if let Ok(p) = b64url_32(&rec.psk) {
                    let r = ResolvedPsk::new(p, PskCategory::LongTerm);
                    if r.psk_id == psk_id {
                        return Some((r, Some(rec.server_id.clone())));
                    }
                }
            }
        }
        None
    }
}

/// Mutable per-connection state shared with helper threads.
struct Conn {
    writer: Arc<ChannelWriter>,
    filter: Mutex<TimeFilter>,
    alive: AtomicBool,
    /// Suppress time-sync sends (pairing exchange in progress).
    quiet: AtomicBool,
    dialect: Dialect,
    /// Source role active on this connection.
    source_active: AtomicBool,
    /// Initial client/state sent for the current activation.
    state_sent: AtomicBool,
    stream: Mutex<StreamCtl>,
    /// Latest signal estimate (None until the first audio frame).
    signal: Mutex<Option<bool>>,
}

#[derive(Default)]
struct StreamCtl {
    open: bool,
    /// Bumped on every (re)start so the sender re-anchors timestamps.
    generation: u64,
}

impl Conn {
    fn synced(&self) -> bool {
        self.filter.lock().unwrap().is_synchronized()
    }

    fn send_state(&self) {
        let signal = *self.signal.lock().unwrap();
        let _ = self.writer.send_json(&proto::client_state_source(true, Some(signal.unwrap_or(false))));
        self.state_sent.store(true, Ordering::SeqCst);
    }

    fn start_stream(&self) -> Result<()> {
        let mut st = self.stream.lock().unwrap();
        if st.open {
            return Ok(());
        }
        let fmt = AudioFormat {
            codec: "pcm".into(),
            sample_rate: WIRE_SAMPLE_RATE,
            channels: WIRE_CHANNELS,
            bit_depth: 16,
        };
        self.writer.send_json(&proto::client_stream_start(self.dialect, &fmt, None))?;
        st.open = true;
        st.generation += 1;
        Ok(())
    }

    fn stop_stream(&self) {
        let mut st = self.stream.lock().unwrap();
        if st.open {
            st.open = false;
            let _ = self.writer.send_json(&proto::client_stream_end(self.dialect));
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_connection(sh: &Arc<Shared>, conn_id: u64, r: super::ws::WsReader, w: Arc<super::ws::WsWriter>, peer: SocketAddr) -> Result<()> {
    let cfg = sh.store.snapshot();
    let client_priv = cfg
        .client_key
        .as_deref()
        .and_then(|k| b64url_32(k).ok())
        .ok_or_else(|| anyhow!("no client identity"))?;
    let resolver = make_resolver(sh.store.clone());
    let hs = handshake::client_handshake(r, w, &client_priv, Suite::ChaChaPoly, &resolver, Duration::from_secs(30))
        .context("handshake")?;
    let dialect = if hs.server_sends_psk_category { Dialect::Spec } else { Dialect::V9 };
    let server_id = hs.server_id.clone();
    let suite = hs.suite;
    info!(
        "Sendspin source: server {} connected from {} (key: {:?}, dialect {:?})",
        &server_id[..8.min(server_id.len())],
        peer,
        hs.psk.category,
        dialect
    );
    if hs.psk.category == PskCategory::LongTerm {
        let sid = server_id.clone();
        sh.store.update(&mut |c: &mut SendspinConfig| {
            c.touch_server_pairing(&sid, unix_now());
            true
        });
    }

    let mut category = hs.psk.category;
    let mut matched_psk_id = hs.psk.psk_id.clone();
    let h_shared = Arc::new(Mutex::new(hs.handshake_hash));
    let writer = hs.writer.clone();

    // The pump performs re-handshakes inline (key switch point).
    let hook = {
        let writer = writer.clone();
        let h_shared = h_shared.clone();
        let server_id = server_id.clone();
        let resolver = resolver.clone();
        Box::new(move |reader: &mut super::channel::ChannelReader, msg: &Value| {
            let prev = *h_shared.lock().unwrap();
            let res = handshake::client_rehandshake(reader, &writer, msg, &prev, &server_id, &client_priv, suite, &resolver);
            Some(Event::Rehandshake(match res {
                Ok((psk, h)) => {
                    *h_shared.lock().unwrap() = h;
                    Ok((psk, h))
                }
                Err(e) => Err(format!("{:#}", e)),
            }))
        })
    };
    let events = spawn_pump(hs.reader, "source", hook);

    let conn = Arc::new(Conn {
        writer: writer.clone(),
        filter: Mutex::new(TimeFilter::default()),
        alive: AtomicBool::new(true),
        quiet: AtomicBool::new(false),
        dialect,
        source_active: AtomicBool::new(false),
        state_sent: AtomicBool::new(false),
        stream: Mutex::new(StreamCtl::default()),
        signal: Mutex::new(None),
    });
    let _alive_guard = AliveGuard(conn.clone());

    // server/hello → client/hello.
    let hello = expect(&events, "server/hello", Duration::from_secs(10))?;
    let mut server_name = payload(&hello)
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Sendspin server")
        .to_string();
    send_hello(sh, &conn, category == PskCategory::LongTerm)?;

    let mut active_roles: Vec<String> = Vec::new();
    let mut admitted = false;
    let mut pairing_index: u32 = 0;
    let mut helpers_started = false;
    loop {
        if sh.stop.load(Ordering::SeqCst) {
            let _ = writer.send_json(&proto::client_goodbye("shutdown"));
            writer.close();
            return Ok(());
        }
        let ev = match events.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match ev {
            Event::Closed(err) => {
                if let Some(e) = err {
                    debug!("Sendspin source connection closed: {}", e);
                }
                return Ok(());
            }
            Event::Rehandshake(Err(e)) => bail!("re-handshake failed: {}", e),
            Event::Rehandshake(Ok((psk, _h))) => {
                category = psk.category;
                matched_psk_id = psk.psk_id.clone();
                pairing_index = 0;
                debug!("Sendspin source: re-handshake complete ({:?})", category);
            }
            Event::Binary { .. } => {}
            Event::Json { value, received_at } => match msg_type(&value) {
                "server/hello" => {
                    // aiosendspin 9.x re-exchanges hellos after a re-handshake.
                    if let Some(n) = payload(&value).get("name").and_then(Value::as_str) {
                        server_name = n.to_string();
                    }
                    send_hello(sh, &conn, category == PskCategory::LongTerm)?;
                }
                "server/activate" => {
                    let mut pending = Some(value);
                    while let Some(v) = pending.take() {
                        let act = proto::parse_activation(payload(&v));
                        if let Err(reason) = check_activation(category, &act) {
                            warn!("Sendspin source: refusing activation from {} ({})", server_name, reason);
                            let _ = writer.send_json(&proto::client_goodbye(reason));
                            writer.close();
                            return Ok(());
                        }
                        if !admitted {
                            if !admit(sh, conn_id, &act, &server_id, &writer) {
                                let _ = writer.send_json(&proto::client_goodbye("concurrent_attempt"));
                                writer.close();
                                return Ok(());
                            }
                            admitted = true;
                        } else {
                            update_rank(sh, conn_id, &act);
                        }
                        if act.playback && category == PskCategory::LongTerm {
                            let sid = server_id.clone();
                            sh.store.update(&mut |c: &mut SendspinConfig| {
                                if c.last_playback_server.as_deref() != Some(sid.as_str()) {
                                    c.last_playback_server = Some(sid.clone());
                                    true
                                } else {
                                    false
                                }
                            });
                        }
                        if let Some(roles) = &act.active_roles {
                            active_roles = roles.clone();
                        }
                        let source_now = active_roles.iter().any(|r| r.starts_with("source@"));
                        let was = conn.source_active.swap(source_now, Ordering::SeqCst);
                        if was && !source_now {
                            conn.stop_stream();
                        }
                        if !source_now {
                            conn.state_sent.store(false, Ordering::SeqCst);
                        }
                        if act.pairing {
                            pairing_index += 1;
                            set_state(sh, conn_id, SourceState::NeedsPairing { server: server_name.clone() });
                            pending = run_pairing(sh, &conn, &events, &act, category, pairing_index, &h_shared, suite, &server_id, &server_name)?;
                            continue;
                        }
                        // Steady state: helpers + availability.
                        if !helpers_started {
                            helpers_started = true;
                            spawn_time_sync(conn.clone());
                            spawn_audio(sh.clone(), conn.clone());
                        }
                        if source_now && !was && conn.synced() {
                            conn.send_state();
                        }
                        let st = if category == PskCategory::LongTerm && source_now {
                            SourceState::Connected { server: server_name.clone(), streaming: conn.stream.lock().unwrap().open }
                        } else {
                            SourceState::NeedsPairing { server: server_name.clone() }
                        };
                        set_state(sh, conn_id, st);
                    }
                }
                "server/time" => {
                    let p = payload(&value);
                    let (Some(t1), Some(t2), Some(t3)) = (
                        p.get("client_transmitted").and_then(Value::as_i64),
                        p.get("server_received").and_then(Value::as_i64),
                        p.get("server_transmitted").and_then(Value::as_i64),
                    ) else {
                        continue;
                    };
                    let t4 = instant_to_us(received_at);
                    let became_synced = {
                        let mut f = conn.filter.lock().unwrap();
                        let was = f.is_synchronized();
                        f.update_from_exchange(t1, t2, t3, t4);
                        !was && f.is_synchronized()
                    };
                    if became_synced && conn.source_active.load(Ordering::SeqCst) && !conn.state_sent.load(Ordering::SeqCst) {
                        conn.send_state();
                    }
                }
                "server/command" => {
                    if let Some(cmd) = payload(&value).get("source").and_then(|s| s.get("command")).and_then(Value::as_str) {
                        match cmd {
                            "start" => {
                                if conn.source_active.load(Ordering::SeqCst) && conn.synced() {
                                    conn.start_stream()?;
                                    note(sh, format!("{} started playing this PC's audio", server_name));
                                    set_state(sh, conn_id, SourceState::Connected { server: server_name.clone(), streaming: true });
                                } else {
                                    debug!("ignoring source start: role inactive or clock not synced");
                                }
                            }
                            "stop" => {
                                conn.stop_stream();
                                if category == PskCategory::LongTerm {
                                    set_state(sh, conn_id, SourceState::Connected { server: server_name.clone(), streaming: false });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "server/unpair" => {
                    if category == PskCategory::LongTerm {
                        let id = matched_psk_id.clone();
                        sh.store.update(&mut |c: &mut SendspinConfig| c.remove_server_pairing_by_psk_id(&id));
                        note(sh, format!("{} removed its pairing", server_name));
                        let _ = writer.send_json(&proto::client_goodbye("unpaired"));
                        writer.close();
                        return Ok(());
                    }
                }
                "pair/abort" | "server/pair-finalize" | "server/pair-init" | "server/pair-auth" | "server/pair-confirm" => {
                    // Leftovers from an attempt that already ended: discard.
                }
                other => debug!("Sendspin source: ignoring {}", other),
            },
        }
    }
}

/// Marks the connection dead when the handler returns (helpers exit).
struct AliveGuard(Arc<Conn>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
    }
}

fn send_hello(sh: &Shared, conn: &Conn, paired: bool) -> Result<()> {
    let params = proto::ClientHelloParams {
        name: &sh.opts.name,
        product_name: crate::PRODUCT_NAME,
        manufacturer: crate::PRODUCT_NAME,
        software_version: &sh.opts.software_version,
        paired,
        offer_dynamic_code: true,
        min_code_length: MIN_CODE_DIGITS,
    };
    conn.writer.send_json(&proto::client_hello_source(conn.dialect, &params))
}

fn expect(events: &Receiver<Event>, ty: &str, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(Event::Json { value, .. }) if msg_type(&value) == ty => return Ok(value),
            Ok(Event::Json { value, .. }) => bail!("expected {}, got {}", ty, msg_type(&value)),
            Ok(Event::Closed(e)) => bail!("closed while awaiting {}: {:?}", ty, e),
            Ok(_) => continue,
            Err(_) => bail!("timed out awaiting {}", ty),
        }
    }
}

/// Activation admissibility for our client (unpaired access disabled).
/// Returns the goodbye reason on rejection.
pub fn check_activation(category: PskCategory, act: &Activation) -> std::result::Result<(), &'static str> {
    if !act.other_activities.is_empty() {
        return Err("unauthorized");
    }
    if act.pairing && act.playback && category == PskCategory::LongTerm {
        return Err("unauthorized");
    }
    let has_roles = act.active_roles.as_ref().map(|r| !r.is_empty()).unwrap_or(false);
    if (act.playback || has_roles) && category != PskCategory::LongTerm {
        return Err(if category == PskCategory::Sentinel { "pairing_required" } else { "unauthorized" });
    }
    Ok(())
}

fn rank(act: &Activation) -> u8 {
    if act.playback {
        2
    } else if act.pairing {
        1
    } else {
        0
    }
}

/// Multi-server admission (server-initiated rules, simplified): an
/// incoming connection whose first activation ranks at least as high as
/// the current one displaces it; a running pairing attempt is never
/// displaced; between two idle connections the last-playback server wins.
fn admit(sh: &Shared, conn_id: u64, act: &Activation, server_id: &str, writer: &Arc<ChannelWriter>) -> bool {
    let mut adm = sh.admitted.lock().unwrap();
    let new_rank = rank(act);
    if let Some(cur) = adm.as_ref() {
        if cur.server_id == server_id || cur.conn_id == conn_id {
            // Same server reconnecting: replace silently.
        } else if cur.pairing_attempt {
            return false;
        } else if new_rank < cur.rank {
            return false;
        } else if new_rank == 0 && cur.rank == 0 {
            let last = sh.store.snapshot().last_playback_server;
            let incoming_is_last = last.as_deref() == Some(server_id);
            let current_is_last = last.as_deref() == Some(cur.server_id.as_str());
            if !incoming_is_last || current_is_last {
                return false;
            }
        }
        if cur.conn_id != conn_id {
            let _ = cur.writer.send_json(&proto::client_goodbye("another_server"));
            cur.writer.close();
        }
    }
    *adm = Some(Admitted {
        conn_id,
        rank: new_rank,
        server_id: server_id.to_string(),
        pairing_attempt: act.pairing,
        writer: writer.clone(),
    });
    true
}

fn update_rank(sh: &Shared, conn_id: u64, act: &Activation) {
    if let Some(a) = sh.admitted.lock().unwrap().as_mut() {
        if a.conn_id == conn_id {
            a.rank = rank(act);
            a.pairing_attempt = act.pairing;
        }
    }
}

fn set_state(sh: &Shared, conn_id: u64, st: SourceState) {
    let is_current = sh.admitted.lock().unwrap().as_ref().map(|a| a.conn_id) == Some(conn_id);
    if is_current {
        *sh.state.lock().unwrap() = st;
    }
}

/// Run one pairing attempt. Returns a follow-up `server/activate` to
/// process (when the server left pairing without finalizing).
#[allow(clippy::too_many_arguments)]
fn run_pairing(
    sh: &Shared,
    conn: &Conn,
    events: &Receiver<Event>,
    act: &Activation,
    category: PskCategory,
    pairing_index: u32,
    h_shared: &Mutex<[u8; 32]>,
    suite: Suite,
    server_id: &str,
    server_name: &str,
) -> Result<Option<Value>> {
    let method = act.pairing_method;
    let fits = match method {
        Some(PairMethod::PairingPsk) => category == PskCategory::Pairing,
        Some(PairMethod::DynamicCode) => category != PskCategory::Pairing,
        _ => false,
    };
    if !fits {
        let _ = conn.writer.send_json(&super::channel::envelope("pair/abort", serde_json::json!({"reason": "method_not_supported"})));
        return Ok(None);
    }
    conn.quiet.store(true, Ordering::SeqCst);
    let ctx = PairingCtx {
        dialect: conn.dialect,
        writer: &conn.writer,
        events,
        handshake_hash: *h_shared.lock().unwrap(),
        pairing_index,
        suite,
    };
    let outcome = match method {
        Some(PairMethod::PairingPsk) => pairing::client_pairing_psk(&ctx),
        _ => {
            let digits = act.pin_length.unwrap_or(MIN_CODE_DIGITS).max(MIN_CODE_DIGITS);
            let show = |code: Option<String>| {
                *sh.code.lock().unwrap() = code.map(|c| (c, server_name.to_string()));
            };
            pairing::client_dynamic_code(&ctx, digits, &show)
        }
    };
    conn.quiet.store(false, Ordering::SeqCst);
    match outcome? {
        PairOutcome::Finalized { long_term_psk } => {
            let rec = ServerPairing {
                server_id: server_id.to_string(),
                psk: b64url(&long_term_psk),
                server_name: Some(server_name.to_string()),
                last_used_unix: unix_now(),
            };
            sh.store.update(&mut |c: &mut SendspinConfig| {
                c.store_server_pairing(rec.clone());
                true
            });
            note(sh, format!("Paired with {}", server_name));
            Ok(None)
        }
        PairOutcome::Left(v) => Ok(Some(v)),
        PairOutcome::Aborted(reason) => {
            note(sh, format!("Pairing with {} ended: {}", server_name, reason.replace('_', " ")));
            Ok(None)
        }
    }
}

fn spawn_time_sync(conn: Arc<Conn>) {
    let _ = std::thread::Builder::new().name("sendspin-source-time".into()).spawn(move || {
        while conn.alive.load(Ordering::SeqCst) {
            if !conn.quiet.load(Ordering::SeqCst) && conn.writer.send_json(&proto::client_time(now_us())).is_err() {
                return;
            }
            let wait = conn.filter.lock().unwrap().next_interval_ms();
            let until = Instant::now() + Duration::from_millis(wait);
            while Instant::now() < until {
                if !conn.alive.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    });
}

/// Timestamp smoother: capture times come from frame arrival (jittery by
/// scheduling); we follow a sample-counted timeline and only slew toward
/// the measured arrival, re-anchoring on large jumps (stalls).
struct CaptureClock {
    anchor_us: Option<f64>,
    frames: u64,
}

impl CaptureClock {
    fn new() -> Self {
        Self { anchor_us: None, frames: 0 }
    }

    /// `measured_start_us`: local time the first sample of the next chunk
    /// was (approximately) captured. Returns the smoothed value.
    fn next(&mut self, measured_start_us: i64, chunk_frames: usize) -> i64 {
        let predicted = self.anchor_us.map(|a| a + self.frames as f64 * 1e6 / WIRE_SAMPLE_RATE as f64);
        let out = match predicted {
            Some(p) if (p - measured_start_us as f64).abs() < 30_000.0 => {
                // Slew 1% of the error per chunk.
                let corr = (measured_start_us as f64 - p) * 0.01;
                self.anchor_us = self.anchor_us.map(|a| a + corr);
                p + corr
            }
            _ => {
                self.anchor_us = Some(measured_start_us as f64);
                self.frames = 0;
                measured_start_us as f64
            }
        };
        self.frames += chunk_frames as u64;
        out.round() as i64
    }
}

fn spawn_audio(sh: Arc<Shared>, conn: Arc<Conn>) {
    let _ = std::thread::Builder::new().name("sendspin-source-audio".into()).spawn(move || {
        let rx: Receiver<PcmFrame> = sh.hub.subscribe();
        let mut pending: Vec<u8> = Vec::with_capacity(CHUNK_FRAMES * BYTES_PER_FRAME * 2);
        let mut clock = CaptureClock::new();
        let mut generation = 0u64;
        let mut last_loud: Option<Instant> = None;
        let mut reported_signal: Option<bool> = None;
        while conn.alive.load(Ordering::SeqCst) {
            let frame = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(f) => f,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let arrived = now_us();
            let bytes = &frame.0[..];
            // Line sensing.
            let loud = bytes
                .chunks_exact(2)
                .any(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() > SIGNAL_THRESHOLD as u16);
            if loud {
                last_loud = Some(Instant::now());
            }
            let present = last_loud.map(|t| t.elapsed() < SIGNAL_ABSENT_AFTER).unwrap_or(false);
            *conn.signal.lock().unwrap() = Some(present);
            if reported_signal != Some(present) && conn.source_active.load(Ordering::SeqCst) && conn.synced() && !conn.quiet.load(Ordering::SeqCst) {
                reported_signal = Some(present);
                conn.send_state();
            }

            // Streaming.
            let st = conn.stream.lock().unwrap();
            if !st.open {
                pending.clear();
                continue;
            }
            if st.generation != generation {
                generation = st.generation;
                pending.clear();
                clock = CaptureClock::new();
            }
            let frame_frames = bytes.len() / BYTES_PER_FRAME;
            // Local time of the first sample still pending.
            let pending_frames = pending.len() / BYTES_PER_FRAME;
            pending.extend_from_slice(&bytes[..frame_frames * BYTES_PER_FRAME]);
            let first_sample_us = arrived - ((pending_frames + frame_frames) as i64 * 1_000_000 / WIRE_SAMPLE_RATE as i64);
            // Bound backlog after a stall: keep at most ~100 ms.
            let max_bytes = (WIRE_SAMPLE_RATE as usize / 10) * BYTES_PER_FRAME;
            let mut start_us = first_sample_us;
            if pending.len() > max_bytes {
                let drop = pending.len() - max_bytes;
                let drop = drop - drop % BYTES_PER_FRAME;
                pending.drain(..drop);
                start_us += (drop / BYTES_PER_FRAME) as i64 * 1_000_000 / WIRE_SAMPLE_RATE as i64;
                clock = CaptureClock::new();
            }
            while pending.len() >= CHUNK_FRAMES * BYTES_PER_FRAME {
                let chunk: Vec<u8> = pending.drain(..CHUNK_FRAMES * BYTES_PER_FRAME).collect();
                let local_ts = clock.next(start_us, CHUNK_FRAMES);
                start_us += CHUNK_FRAMES as i64 * 1_000_000 / WIRE_SAMPLE_RATE as i64;
                let server_ts = conn.filter.lock().unwrap().compute_server_time(local_ts);
                if conn.writer.send_binary(&proto::source_chunk(server_ts, &chunk)).is_err() {
                    return;
                }
            }
            drop(st);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(playback: bool, pairing: bool, roles: Option<Vec<&str>>) -> Activation {
        Activation {
            playback,
            pairing,
            active_roles: roles.map(|r| r.into_iter().map(String::from).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn activation_rules() {
        use PskCategory::*;
        assert!(check_activation(Sentinel, &act(false, false, Some(vec![]))).is_ok());
        assert!(check_activation(Sentinel, &act(false, true, Some(vec![]))).is_ok());
        assert_eq!(check_activation(Sentinel, &act(true, false, Some(vec!["source@v1"]))), Err("pairing_required"));
        assert_eq!(check_activation(Sentinel, &act(false, false, Some(vec!["source@v1"]))), Err("pairing_required"));
        assert_eq!(check_activation(Pairing, &act(true, false, None)), Err("unauthorized"));
        assert!(check_activation(Pairing, &act(false, true, Some(vec![]))).is_ok());
        assert!(check_activation(LongTerm, &act(true, false, Some(vec!["source@v1"]))).is_ok());
        let mut mgmt = act(false, false, None);
        mgmt.other_activities.push("management".into());
        assert_eq!(check_activation(LongTerm, &mgmt), Err("unauthorized"));
    }

    #[test]
    fn capture_clock_follows_samples_and_reanchors() {
        let mut c = CaptureClock::new();
        let t0 = c.next(1_000_000, CHUNK_FRAMES);
        assert_eq!(t0, 1_000_000);
        // Jittery arrival (+3 ms) barely moves the sample-counted timeline.
        let t1 = c.next(1_020_000 + 3_000, CHUNK_FRAMES);
        assert!((t1 - 1_020_000).abs() < 100, "t1={}", t1);
        // A 200 ms stall re-anchors.
        let t2 = c.next(1_300_000, CHUNK_FRAMES);
        assert_eq!(t2, 1_300_000);
    }
}
