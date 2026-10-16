//! HTTP server.
//!
//! Responsibilities:
//!   1. `GET /stream.raw` — endless WAV stream of L16 PCM for UPnP
//!      speakers (plus the `HEAD` probe Sonos sends first).
//!   2. `NOTIFY ...` — GENA event callbacks from the speaker we hold a
//!      subscription with.
//!   3. The optional web UI and JSON API (`/r/<key>/`, `/api/*`), both
//!      needing the key in the web UI's link (see `web_access`).
//!
//! The PCM stream is fed by a single producer thread (audio path) that
//! broadcasts each packet to all current subscribers via a fan-out.
//!
//! The server is a small, deliberately strict HTTP/1.1 implementation:
//! every connection gets its own thread, request heads and bodies are
//! size-capped and read under deadlines, and the number of connections
//! (overall and per client address) is bounded. Nothing a client sends
//! can stall another client's request or the speaker's stream fetch,
//! and nothing a client sends can make the process allocate without
//! bound.

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::{debug, info, warn};
use serde::Serialize;
use std::io::{BufWriter, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::gena::{self, NotifyAdmission};
use crate::stream_gate::{canonical_ip, Refusal};
use crate::web_access::{redact_target, WebAccess, LINK_PREFIX};
use crate::{WIRE_BYTES_PER_FRAME, WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Callback invoked from the GENA NOTIFY route with the request path and
/// the raw XML body. Only called for NOTIFYs that match a live
/// subscription (SID and source address).
pub type GenaNotifyCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// A speaker the user could target. Exposed via `GET /api/speakers`.
#[derive(Clone, Debug)]
pub struct SpeakerInfo {
    /// Stable identifier (UDN if available, else IP).
    pub id: String,
    pub friendly_name: String,
    pub ip: String,
    /// `true` when this is the currently-active target.
    pub active: bool,
    /// Optional advisory shown as a row tooltip — e.g. that an AirPlay
    /// entry for a speaker that also has a non-AirPlay entry will have
    /// higher latency. `None` for most rows.
    pub note: Option<String>,
    /// `true` for a speaker the user added by address rather than one
    /// found by discovery.
    pub manual: bool,
    /// The user hid this speaker (only listed when hidden ones are asked
    /// for, or while it is the one playing).
    pub hidden: bool,
}

/// Callback that returns the current list of discovered speakers.
pub type SpeakerListCallback = Arc<dyn Fn() -> Vec<SpeakerInfo> + Send + Sync>;

/// Callback to switch the active speaker. Receives the speaker id.
/// Returns Err with a user-presentable message on failure.
pub type SpeakerSelectCallback = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Callback to force a UPnP Stop+Play on the active speaker, which
/// makes Sonos discard its current prebuffer and pick a fresh
/// (minimal) prebuffer level — useful for trimming accumulated latency.
/// Returns Err with a user-presentable message on failure.
pub type ResyncCallback = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Callback to adjust latency by N milliseconds (positive = reduce
/// latency by dropping that much audio gradually; negative = increase
/// latency by duplicating frames). Returns the new pending-frames
/// counter so the caller can see how much remains to be applied.
pub type LatencyAdjustCallback = Arc<dyn Fn(i32) -> i64 + Send + Sync>;

/// Callback deciding whether a client IP may fetch `/stream.raw` (the
/// stream gate: streaming enabled, privacy mode), and if not, why: the
/// refused device and the log are told which rule it was. Consulted per
/// request and once a second per open stream, so the user can disable
/// streaming, flip the setting or switch speakers without re-binding the
/// socket.
pub type StreamClientAllowedCallback =
    Arc<dyn Fn(std::net::IpAddr) -> Result<(), Refusal> + Send + Sync>;

/// Callback answering whether a client IP is the active speaker's (see
/// [`HttpServerConfig::speaker_peer`]).
pub type SpeakerPeerCallback = Arc<dyn Fn(std::net::IpAddr) -> bool + Send + Sync>;

/// One PCM packet to broadcast to subscribers. Pre-encoded as bytes so the
/// audio thread does the conversion once.
#[derive(Clone)]
pub struct PcmFrame(pub Arc<Vec<u8>>);

/// Hub used by the audio thread to push PCM and by HTTP workers to pull.
pub struct StreamHub {
    subscribers: Mutex<Vec<HubSubscriber>>,
    /// Frames a subscriber's full queue had no room for (dropped for that
    /// subscriber only), since launch.
    dropped: std::sync::atomic::AtomicU64,
}

struct HubSubscriber {
    tx: Sender<PcmFrame>,
    /// Peer IP for HTTP `/stream.raw` clients; `None` for internal
    /// consumers (the AirPlay senders), which privacy mode never prunes.
    peer: Option<std::net::IpAddr>,
}

impl StreamHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscribers: Mutex::new(Vec::new()),
            dropped: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Subscribe an internal (in-process) consumer.
    pub fn subscribe(&self) -> Receiver<PcmFrame> {
        self.subscribe_inner(None)
    }

    /// Subscribe an HTTP stream client, remembering its peer IP so
    /// [`StreamHub::disconnect_clients`] can cut it off later.
    pub fn subscribe_from(&self, peer: std::net::IpAddr) -> Receiver<PcmFrame> {
        self.subscribe_inner(Some(peer))
    }

    fn subscribe_inner(&self, peer: Option<std::net::IpAddr>) -> Receiver<PcmFrame> {
        // Bounded; if a subscriber stalls we drop frames rather than
        // back-pressure the audio path.
        let (tx, rx) = bounded::<PcmFrame>(128);
        self.subscribers.lock().unwrap().push(HubSubscriber { tx, peer });
        rx
    }

    /// Publish a PCM frame to all subscribers, dropping any whose channel
    /// is closed or full.
    pub fn publish(&self, frame: PcmFrame) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|sub| {
            match sub.tx.try_send(frame.clone()) {
                Ok(()) => true,
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    // Drop the frame for this slow subscriber; they'll
                    // catch up.  We keep them subscribed.
                    self.dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    true
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// Disconnect the HTTP stream clients whose peer IP matches `pred`.
    /// Dropping a subscriber's Sender makes its `StreamReader` return
    /// EOF, which closes the connection. Used when the stream gate stops
    /// admitting someone (streaming disabled, privacy turned on, speaker
    /// switched), so a listener connected *before* that doesn't keep
    /// receiving audio. Returns how many were cut.
    pub fn disconnect_clients(&self, pred: impl Fn(std::net::IpAddr) -> bool) -> usize {
        let mut subs = self.subscribers.lock().unwrap();
        let before = subs.len();
        subs.retain(|sub| !sub.peer.map(&pred).unwrap_or(false));
        before - subs.len()
    }

    /// Frames dropped for slow subscribers since launch.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current subscriber count.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}

/// Configuration for the HTTP server.
pub struct HttpServerConfig {
    pub bind: SocketAddr,
    pub hub: Arc<StreamHub>,
    pub gena_callback: Option<GenaNotifyCallback>,
    /// If set, exposes `GET /api/speakers` returning a JSON list.
    pub speaker_list: Option<SpeakerListCallback>,
    /// If set, exposes `POST /api/select` to switch the active speaker.
    pub speaker_select: Option<SpeakerSelectCallback>,
    /// If set, exposes `POST /api/resync` to force-flush the speaker's
    /// prebuffer (UPnP Stop + Play).
    pub resync: Option<ResyncCallback>,
    /// If set, exposes `POST /api/latency/adjust?ms=N` for gradual
    /// runtime latency trim / pad.
    pub latency_adjust: Option<LatencyAdjustCallback>,
    /// The web page (under `/r/<key>/`) and the JSON API (`/api/*`):
    /// whether they are on, and the key they need (see `web_access`).
    /// The `/stream.raw` route is always served regardless — the speaker
    /// can't function without it. `None` ⇒ no web page or API.
    pub web_access: Option<Arc<WebAccess>>,
    /// If set, GET/HEAD `/stream.raw` is only served when this admits the
    /// client's IP — the stream gate. `None` ⇒ the stream is served to
    /// anyone who connects.
    pub stream_client_allowed: Option<StreamClientAllowedCallback>,
    /// Host names / addresses accepted in the `Host` header on top of
    /// `localhost`, this machine's name, the bound address and every
    /// address assigned to this machine — e.g. an `--advertise-ip` that
    /// points at a NAT or proxy in front of this PC.
    pub extra_hosts: Vec<String>,
    /// True for the active speaker's addresses. Their connections are
    /// exempt from the shared connection budget, so other clients can't
    /// lock the speaker out.
    pub speaker_peer: Option<SpeakerPeerCallback>,
}

impl HttpServerConfig {
    pub fn minimal(bind: SocketAddr, hub: Arc<StreamHub>) -> Self {
        Self {
            bind,
            hub,
            gena_callback: None,
            speaker_list: None,
            speaker_select: None,
            resync: None,
            latency_adjust: None,
            web_access: None,
            stream_client_allowed: None,
            extra_hosts: Vec::new(),
            speaker_peer: None,
        }
    }
}

// -----------------------------------------------------------------------------
// Limits
// -----------------------------------------------------------------------------

/// Request line + headers, in bytes.
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
/// Whole request head of a connection's first request.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Keep-alive: how long a connection may sit idle before its next
/// request head is complete.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Whole request body.
const BODY_TIMEOUT: Duration = Duration::from_secs(10);
/// Writing an ordinary (non-stream) response.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// GENA event bodies: a full RenderingControl LastChange is a few KB.
const MAX_NOTIFY_BODY: usize = 64 * 1024;
/// `/api/select` bodies: `{"id":"..."}`.
const MAX_API_BODY: usize = 4 * 1024;
/// Connections from everyone but the active speaker (see [`ConnLimiter`]).
const MAX_CONNECTIONS: usize = 128;
/// Per client address, so one host can't use up the whole budget.
const MAX_CONNECTIONS_PER_PEER: usize = 16;
/// Per protected address (the active speaker, loopback): generous, only
/// a bound on a misbehaving client.
const MAX_PROTECTED_CONNECTIONS_PER_PEER: usize = 64;
/// A non-speaker stream consumer that accepts nothing for this long is
/// gone. The active speaker gets no such limit: it may stop reading
/// indefinitely while paused from its own app.
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// A speaker's initial event can arrive before we have read the SID
/// out of its SUBSCRIBE response; wait this long for it to register.
const NOTIFY_SUBSCRIBE_WAIT: Duration = Duration::from_secs(3);

/// Content-Security-Policy for the status page: scripts only from this
/// server, i.e. the page's `app.js` (no inline script, so markup injected
/// through a speaker name or id can't run), requests only to this origin.
const STATUS_PAGE_CSP: &str = "default-src 'none'; script-src 'self'; connect-src 'self'; \
     style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

// -----------------------------------------------------------------------------
// Server
// -----------------------------------------------------------------------------

/// Start the HTTP server on a background thread.  Returns the actual port
/// it bound to.
pub fn start_http_server(cfg: HttpServerConfig) -> Result<u16> {
    let bind = cfg.bind;
    let listener = TcpListener::bind(bind)
        .map_err(|e| anyhow::anyhow!("HTTP server bind {} failed: {}", bind, e))?;
    let actual = listener.local_addr().map(|a| a.port()).unwrap_or(bind.port());
    info!("HTTP server listening on {}", actual);

    let ctx = Arc::new(Ctx {
        hosts: HostPolicy::new(bind.ip(), &cfg.extra_hosts, is_local_ip),
        hub: cfg.hub,
        gena_callback: cfg.gena_callback,
        speaker_list: cfg.speaker_list,
        speaker_select: cfg.speaker_select,
        resync: cfg.resync,
        latency_adjust: cfg.latency_adjust,
        web_access: cfg.web_access,
        stream_client_allowed: cfg.stream_client_allowed,
        speaker_peer: cfg.speaker_peer,
        notify_lock: Mutex::new(()),
    });
    thread::Builder::new()
        .name("stream-to-speaker-http".to_string())
        .spawn(move || accept_loop(listener, ctx))
        .context("spawning HTTP server thread")?;

    Ok(actual)
}

fn is_local_ip(ip: IpAddr) -> bool {
    crate::stream_gate::is_local_address(ip)
}

struct Ctx {
    hub: Arc<StreamHub>,
    gena_callback: Option<GenaNotifyCallback>,
    speaker_list: Option<SpeakerListCallback>,
    speaker_select: Option<SpeakerSelectCallback>,
    resync: Option<ResyncCallback>,
    latency_adjust: Option<LatencyAdjustCallback>,
    web_access: Option<Arc<WebAccess>>,
    stream_client_allowed: Option<StreamClientAllowedCallback>,
    speaker_peer: Option<SpeakerPeerCallback>,
    hosts: HostPolicy,
    /// Event callbacks run one at a time, in arrival order, as they did
    /// when a single thread served every request.
    notify_lock: Mutex<()>,
}

fn accept_loop(listener: TcpListener, ctx: Arc<Ctx>) {
    let limiter = Arc::new(ConnLimiter::default());
    let mut last_error_log: Option<Instant> = None;
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                let speaker = ctx.speaker_peer.as_ref().is_some_and(|f| f(peer.ip()));
                // This PC's own clients (the local web UI, tools) are
                // protected like the speaker; other LAN hosts are not.
                let protected = speaker || canonical_ip(peer.ip()).is_loopback();
                let Some(permit) = limiter.admit(peer.ip(), protected, &stream) else {
                    debug!("http: could not track connection from {}, dropping", peer);
                    continue;
                };
                let ctx = ctx.clone();
                let spawned = thread::Builder::new()
                    .name("stream-to-speaker-http-conn".to_string())
                    .spawn(move || {
                        let _permit = permit;
                        handle_connection(stream, peer, speaker, &ctx);
                    });
                if let Err(e) = spawned {
                    warn!("http: could not spawn connection thread: {}", e);
                }
            }
            Err(e) => {
                // Transient (aborted handshake, out of sockets, ...).
                // Never fatal: the speaker's next fetch must still work.
                if last_error_log.map_or(true, |t| t.elapsed() > Duration::from_secs(10)) {
                    warn!("http: accept failed: {}", e);
                    last_error_log = Some(Instant::now());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Bounds concurrent connections without ever turning a newcomer away:
/// when a budget is full, the oldest connection charged to it is shut
/// down to make room. Protected connections — from the active speaker's
/// addresses or from this PC itself (loopback) — are never evicted on
/// behalf of other clients and don't count towards the shared budget,
/// so no other host can crowd the speaker out of `/stream.raw` or its
/// event callbacks, or lock the local web UI out.
#[derive(Default)]
struct ConnLimiter {
    inner: Mutex<LimiterInner>,
}

#[derive(Default)]
struct LimiterInner {
    next_id: u64,
    conns: Vec<Tracked>,
}

struct Tracked {
    id: u64,
    peer: IpAddr,
    priority: bool,
    /// Second handle on the socket, used to shut it down on eviction.
    stream: TcpStream,
}

struct ConnPermit {
    limiter: Arc<ConnLimiter>,
    id: u64,
}

impl LimiterInner {
    /// Shut down and forget the oldest connection matching `pred`.
    fn evict_oldest(&mut self, pred: impl Fn(&Tracked) -> bool) {
        if let Some(pos) = self.conns.iter().position(pred) {
            let t = self.conns.remove(pos);
            debug!("http: connection budget full, closing oldest from {}", t.peer);
            let _ = t.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl ConnLimiter {
    fn admit(self: &Arc<Self>, peer: IpAddr, priority: bool, stream: &TcpStream) -> Option<ConnPermit> {
        let handle = stream.try_clone().ok()?;
        let peer = canonical_ip(peer);
        let mut g = self.inner.lock().unwrap();
        let same_peer = g.conns.iter().filter(|t| t.peer == peer).count();
        let per_peer_cap =
            if priority { MAX_PROTECTED_CONNECTIONS_PER_PEER } else { MAX_CONNECTIONS_PER_PEER };
        if same_peer >= per_peer_cap {
            // A host past its own budget only displaces itself.
            g.evict_oldest(|t| t.peer == peer);
        } else if !priority && g.conns.iter().filter(|t| !t.priority).count() >= MAX_CONNECTIONS {
            g.evict_oldest(|t| !t.priority);
        }
        g.next_id += 1;
        let id = g.next_id;
        g.conns.push(Tracked { id, peer, priority, stream: handle });
        Some(ConnPermit { limiter: self.clone(), id })
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.inner.lock().unwrap().conns.len()
    }
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        let mut g = self.limiter.inner.lock().unwrap();
        g.conns.retain(|t| t.id != self.id);
    }
}

fn handle_connection(stream: TcpStream, peer: SocketAddr, speaker: bool, ctx: &Ctx) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut conn = Conn { stream, peer, speaker, buf: Vec::new() };
    let mut timeout = HEAD_TIMEOUT;
    loop {
        let head = match read_head(&mut conn, timeout) {
            Ok(h) => h,
            Err(e) => {
                let status = match e {
                    HeadErr::Closed | HeadErr::Io => None,
                    // Idle keep-alive connection: just close it.
                    HeadErr::Timeout if conn.buf.is_empty() => None,
                    HeadErr::Timeout => Some(408),
                    HeadErr::TooLarge => Some(431),
                    HeadErr::Bad => Some(400),
                    HeadErr::Version => Some(505),
                    HeadErr::NotImplemented => Some(501),
                };
                if let Some(status) = status {
                    debug!("http: bad request from {}: {}", peer, status);
                    let _ = write_reply(&mut conn.stream, 1, &Reply::text(status, ""), false, true);
                }
                return;
            }
        };
        timeout = IDLE_TIMEOUT;
        debug!("http request: {} {}", head.method, redact_target(&head.target));
        if !ctx.handle(&mut conn, &head) {
            return;
        }
    }
}

impl Ctx {
    /// Serve one request. Returns whether the connection may carry
    /// another one.
    fn handle(&self, conn: &mut Conn, head: &Head) -> bool {
        let (path, _query) = split_target(&head.target);
        let head_only = head.method == "HEAD";

        // DNS-rebinding guard: a browser tricked into treating this
        // server as some other site still sends that site's name.
        if !self.hosts.allows(head.header("host")) {
            info!(
                "http: refused {} {} from {}: unrecognised Host {:?}",
                head.method,
                redact_target(path),
                conn.peer.ip(),
                head.header("host").unwrap_or("")
            );
            let reply = Reply::text(421, "unrecognised Host header");
            let _ = write_reply(&mut conn.stream, head.minor, &reply, head_only, true);
            return false;
        }

        let mut body_read = false;
        let reply = match (head.method.as_str(), path) {
            ("GET", "/stream.raw") => {
                if let Some(refusal) = self.check_stream_client(conn.peer.ip(), &head.method) {
                    refusal
                } else {
                    self.serve_stream(conn, head);
                    return false;
                }
            }
            ("HEAD", "/stream.raw") => {
                /* Sonos and other DLNA renderers HEAD-probe the URI before
                 * committing to GET. Reply with the same headers we would
                 * on GET, but no body. Without this Sonos gives up with
                 * "Couldn't connect" before ever trying the GET. */
                if let Some(refusal) = self.check_stream_client(conn.peer.ip(), &head.method) {
                    refusal
                } else {
                    let ok = write_stream_head(&mut conn.stream, head.minor, 0).is_ok();
                    return ok && head.keep_alive() && head.body == BodyKind::None;
                }
            }
            ("GET", "/healthz") => Reply::text(200, "ok"),
            // `/`, `/app.js` and everything under the link's prefix,
            // whatever the key.
            ("GET", p) if p == "/" || p == "/app.js" || p.starts_with(LINK_PREFIX) => self.page(p),
            ("GET", "/api/speakers")
            | ("POST", "/api/select" | "/api/resync" | "/api/latency/adjust") => {
                self.api(conn, head, path, &mut body_read)
            }
            (m, _) if m.eq_ignore_ascii_case("NOTIFY") => self.notify(conn, head, path, &mut body_read),
            _ => Reply::text(404, "not found"),
        };

        // A body we didn't read leaves the connection mid-message, so it
        // can't carry another request.
        let reusable = head.keep_alive() && (head.body == BodyKind::None || body_read);
        let written = write_reply(&mut conn.stream, head.minor, &reply, head_only, !reusable);
        written.is_ok() && reusable
    }

    /// The web access state, while the web UI is on.
    fn web_on(&self) -> Option<&WebAccess> {
        self.web_access.as_deref().filter(|a| a.is_enabled())
    }

    /// The web page. It lives at `/r/<key>/`, its script at
    /// `/r/<key>/app.js`; `/`, `/app.js` and a wrong key get a page saying
    /// where the link is.
    fn page(&self, path: &str) -> Reply {
        let Some(access) = self.web_on() else {
            return Reply::html(404, WEB_UI_OFF_HTML);
        };
        let Some(rest) = path.strip_prefix(LINK_PREFIX) else {
            return Reply::link_needed();
        };
        let (key, sub) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if !access.path_key_ok(key) {
            return Reply::link_needed();
        }
        let reply = match sub {
            // The page's script is addressed relative to the page.
            "" => Reply {
                status: 308,
                content_type: None,
                headers: vec![("Location", format!("{LINK_PREFIX}{key}/"))],
                body: Vec::new(),
            },
            "/" => self.status_page(),
            "/app.js" => Reply {
                status: 200,
                content_type: Some("application/javascript; charset=utf-8"),
                headers: vec![("X-Content-Type-Options", "nosniff".into())],
                body: STATUS_PAGE_JS.as_bytes().to_vec(),
            },
            _ => Reply::text(404, "not found"),
        };
        reply.private()
    }

    /// The JSON API. Every route needs the web UI's key as a bearer token;
    /// the state-changing ones are also refused to other sites' pages.
    fn api(&self, conn: &mut Conn, head: &Head, path: &str, body_read: &mut bool) -> Reply {
        let Some(access) = self.web_on() else {
            return Reply::error(404, "web UI is disabled — enable it in the GUI");
        };
        if !access.bearer_ok(head.header("authorization")) {
            debug!("http: refused {} {} from {}: no valid API key", head.method, path, conn.peer.ip());
            // Script authors read this body raw (curl prints it as it
            // comes), so it holds nothing JSON or `html_safe` escapes: the
            // angle brackets of a `<key>` placeholder would reach them as
            // six-character escapes. ASCII only, for consoles that don't
            // show UTF-8.
            return Reply::unauthorized(
                "missing or wrong API key: send the header 'Authorization: Bearer' followed by a \
                 space and the key, the 32 hex digits after /r/ in the web UI's link. The Stream \
                 To Speaker window shows the link in its Web UI card, and stream-to-speaker.exe \
                 --web-link prints it",
            );
        }
        if head.method == "GET" {
            return self.speakers_json();
        }
        if let Some(why) = cross_site_refusal(head) {
            info!("http: refused POST {} from {}: {}", path, conn.peer.ip(), why);
            return Reply::error(403, why);
        }
        match path {
            "/api/select" => self.select(conn, head, body_read),
            "/api/resync" => self.resync(),
            _ => self.latency_adjust(&head.target),
        }
    }

    /// The stream gate for `/stream.raw`: `None` when the client may
    /// proceed, else the 403 to send, which says why, as the log does.
    fn check_stream_client(&self, peer: IpAddr, method: &str) -> Option<Reply> {
        let cb = self.stream_client_allowed.as_ref()?;
        let refusal = cb(peer).err()?;
        info!("stream: refused {} /stream.raw from {}: {}", method, peer, refusal.reason());
        Some(Reply::text(403, &format!("forbidden: {}", refusal.reason())))
    }

    /// GENA event callback. Accepted only when the SID names a
    /// subscription we hold *and* the event comes from that
    /// subscription's publisher; anything else gets 412 (the UPnP
    /// answer for an unknown SID) without its body being read.
    fn notify(&self, conn: &mut Conn, head: &Head, path: &str, body_read: &mut bool) -> Reply {
        let peer = conn.peer.ip();
        let Some(sid) = head.header("sid").map(str::trim).filter(|s| !s.is_empty()) else {
            debug!("NOTIFY without SID from {}", peer);
            return Reply::text(412, "");
        };
        let registry = gena::registry();
        let live = match registry.check(sid, peer) {
            NotifyAdmission::Live => true,
            NotifyAdmission::Pending => registry.wait_live(sid, peer, NOTIFY_SUBSCRIBE_WAIT),
            NotifyAdmission::Unknown => false,
        };
        if !live {
            debug!("NOTIFY from {} for unknown subscription {:?}; refused", peer, sid);
            return Reply::text(412, "");
        }
        let body = match read_body(conn, head, MAX_NOTIFY_BODY) {
            Ok(b) => b,
            Err(e) => return e.reply(),
        };
        *body_read = true;
        let body = String::from_utf8_lossy(&body);
        // The path is only ever logged.
        let path = redact_target(path);
        if let Some(cb) = &self.gena_callback {
            let _serial = self.notify_lock.lock().unwrap();
            cb(&path, &body);
        } else {
            debug!("NOTIFY received but no callback registered; path={}", path);
        }
        Reply::text(200, "")
    }

    fn status_page(&self) -> Reply {
        let mut rows = String::new();
        if let Some(cb) = &self.speaker_list {
            let list = cb();
            if list.is_empty() {
                rows.push_str("<tr><td colspan=\"3\"><em>No speakers discovered yet.</em></td></tr>");
            } else {
                for s in list {
                    let active_marker = if s.active { " &#9679;" } else { "" };
                    rows.push_str(&format!(
                        "<tr><td>{}{}</td><td>{}</td><td><button type=\"button\" data-select=\"{}\">select</button></td></tr>",
                        html_escape(&s.friendly_name),
                        active_marker,
                        html_escape(&s.ip),
                        html_escape(&s.id),
                    ));
                }
            }
        } else {
            rows.push_str("<tr><td colspan=\"3\"><em>Speaker selection unavailable (--no-discovery?).</em></td></tr>");
        }
        Reply {
            status: 200,
            content_type: Some("text/html; charset=utf-8"),
            headers: vec![
                ("Content-Security-Policy", STATUS_PAGE_CSP.into()),
                ("X-Content-Type-Options", "nosniff".into()),
                ("Referrer-Policy", "no-referrer".into()),
                ("Cache-Control", "no-store".into()),
            ],
            body: STATUS_PAGE_HTML.replace("{rows}", &rows).into_bytes(),
        }
    }

    fn speakers_json(&self) -> Reply {
        match &self.speaker_list {
            Some(cb) => Reply::json(200, &speaker_list(&cb())),
            None => Reply::json(
                200,
                &SpeakerList { speakers: Vec::new(), error: Some("discovery disabled") },
            ),
        }
    }

    fn select(&self, conn: &mut Conn, head: &Head, body_read: &mut bool) -> Reply {
        if !is_json_content_type(head.header("content-type")) {
            return Reply::error(415, "Content-Type must be application/json");
        }
        let body = match read_body(conn, head, MAX_API_BODY) {
            Ok(b) => b,
            Err(e) => return e.reply(),
        };
        *body_read = true;
        let body = String::from_utf8_lossy(&body);
        let Some(id) = extract_json_string(&body, "id") else {
            return Reply::error(400, "missing 'id' field");
        };
        let Some(cb) = &self.speaker_select else {
            return Reply::error(503, "speaker switching unavailable");
        };
        match cb(&id) {
            Ok(()) => Reply::json(200, &ApiOk { ok: true }),
            Err(msg) => Reply::error(500, &msg),
        }
    }

    fn resync(&self) -> Reply {
        let Some(cb) = &self.resync else {
            return Reply::error(503, "resync unavailable");
        };
        match cb() {
            Ok(()) => Reply::json(200, &ApiOk { ok: true }),
            Err(msg) => Reply::error(500, &msg),
        }
    }

    fn latency_adjust(&self, target: &str) -> Reply {
        let Some(cb) = &self.latency_adjust else {
            return Reply::error(503, "latency adjust unavailable");
        };
        let ms = extract_query_int(target, "ms").unwrap_or(0);
        if !(-5000..=5000).contains(&ms) {
            return Reply::error(400, "ms must be in [-5000, 5000]");
        }
        let pending = cb(ms as i32);
        Reply::json(200, &LatencyOk { ok: true, pending_frames: pending })
    }

    /// Serve a single `/stream.raw` connection until the stream ends or
    /// the client goes away.
    fn serve_stream(&self, conn: &mut Conn, head: &Head) {
        // Subscribe BEFORE we send headers so we don't miss the first
        // frame. Tagged with the peer IP so the stream gate can cut the
        // connection off later if this client stops being allowed.
        let peer = conn.peer.ip();
        let rx = self.hub.subscribe_from(peer);
        // The reader also re-asks the gate once a second while streaming,
        // so a listener that stops being allowed (streaming disabled,
        // speaker switched, privacy turned on) is cut within a second even
        // if no explicit prune ran.
        let recheck = self.stream_client_allowed.clone().map(|cb| (cb, peer));
        let mut reader = StreamReader::new(rx, recheck);

        // The active speaker may stop reading for as long as it is
        // paused and must find the stream still open afterwards. Any
        // other consumer that stays stalled is dropped, freeing its slot.
        let write_timeout = (!conn.speaker).then_some(STREAM_WRITE_TIMEOUT);
        let _ = conn.stream.set_write_timeout(write_timeout);
        let mut out = BufWriter::with_capacity(1024, &conn.stream);
        let result = write_stream_head(&mut out, head.minor, FAKE_CONTENT_LEN)
            .and_then(|_| std::io::copy(&mut reader, &mut out).map(|_| ()))
            .and_then(|_| out.flush());
        if let Err(e) = result {
            debug!("stream client ended: {}", e);
        }
    }
}

// -----------------------------------------------------------------------------
// Host / cross-site checks
// -----------------------------------------------------------------------------

/// Which `Host` header values the server answers to. A DNS-rebinding
/// page reaches us under the attacker's domain name and its requests
/// carry that name, so only names that can't be an attacker's are
/// accepted: `localhost`, this machine's name, IP literals that belong
/// to this machine (or the configured bind / advertise addresses). A
/// request without `Host` (HTTP/1.0 tooling) can't come from a browser
/// and is let through.
struct HostPolicy {
    bind_ip: IpAddr,
    names: Vec<String>,
    ips: Vec<IpAddr>,
    is_local: fn(IpAddr) -> bool,
}

impl HostPolicy {
    fn new(bind_ip: IpAddr, extra: &[String], is_local: fn(IpAddr) -> bool) -> Self {
        let mut names = vec!["localhost".to_string()];
        for n in machine_names() {
            let n = n.trim().trim_end_matches('.').to_ascii_lowercase();
            if n.is_empty() || names.contains(&n) {
                continue;
            }
            if !n.contains('.') {
                names.push(format!("{}.local", n));
            }
            names.push(n);
        }
        let mut ips = Vec::new();
        for e in extra {
            let e = e.trim().trim_start_matches('[').trim_end_matches(']');
            match e.parse::<IpAddr>() {
                Ok(ip) => ips.push(canonical_ip(ip)),
                Err(_) if !e.is_empty() => names.push(e.trim_end_matches('.').to_ascii_lowercase()),
                Err(_) => {}
            }
        }
        Self { bind_ip: canonical_ip(bind_ip), names, ips, is_local }
    }

    fn allows(&self, host: Option<&str>) -> bool {
        let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) else {
            return true;
        };
        let Some(name) = host_without_port(host) else {
            return false;
        };
        let mut name = name.trim_end_matches('.').to_ascii_lowercase();
        // IPv6 zone id (`fe80::1%25eth0` in a URL): not part of the address.
        if name.contains(':') {
            if let Some(pct) = name.find('%') {
                name.truncate(pct);
            }
        }
        if let Ok(ip) = name.parse::<IpAddr>() {
            let ip = canonical_ip(ip);
            if ip.is_unspecified() || ip.is_multicast() {
                return false;
            }
            return ip.is_loopback()
                || (!self.bind_ip.is_unspecified() && ip == self.bind_ip)
                || self.ips.contains(&ip)
                || (self.is_local)(ip);
        }
        self.names.iter().any(|n| *n == name)
    }
}

/// This machine's names: NetBIOS name, DNS host name and fully
/// qualified DNS name (what a router's DNS hands out for it).
#[cfg(windows)]
fn machine_names() -> Vec<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsFullyQualified, ComputerNameDnsHostname, ComputerNameNetBIOS,
        GetComputerNameExW,
    };
    let mut out: Vec<String> = std::env::var("COMPUTERNAME").into_iter().collect();
    for kind in [ComputerNameNetBIOS, ComputerNameDnsHostname, ComputerNameDnsFullyQualified] {
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        // SAFETY: `len` is the buffer's capacity in UTF-16 units; on
        // success it holds the length written, without the NUL.
        let ok = unsafe { GetComputerNameExW(kind, buf.as_mut_ptr(), &mut len) };
        if ok != 0 && (len as usize) <= buf.len() {
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
    }
    out
}

#[cfg(not(windows))]
fn machine_names() -> Vec<String> {
    let mut out: Vec<String> = std::env::var("HOSTNAME").into_iter().collect();
    if let Ok(n) = std::fs::read_to_string("/etc/hostname") {
        out.push(n);
    }
    out
}

/// `host[:port]` / `[v6][:port]` → host. None if malformed.
fn host_without_port(h: &str) -> Option<&str> {
    let valid_port = |p: &str| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = h.strip_prefix('[') {
        let end = rest.find(']')?;
        let after = &rest[end + 1..];
        if !after.is_empty() && !after.strip_prefix(':').is_some_and(valid_port) {
            return None;
        }
        return Some(&rest[..end]);
    }
    match h.split_once(':') {
        Some((name, port)) if valid_port(port) && !name.is_empty() => Some(name),
        Some(_) => None,
        None => Some(h),
    }
}

/// CSRF guard for the state-changing API: browsers label cross-site
/// requests (`Sec-Fetch-Site`, `Origin`); a page on another origin is
/// refused. Requests carrying neither header (curl, scripts) pass.
fn cross_site_refusal(head: &Head) -> Option<&'static str> {
    if let Some(site) = head.header("sec-fetch-site") {
        let site = site.trim();
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return Some("cross-site request refused");
        }
    }
    if let Some(origin) = head.header("origin") {
        let same = head
            .header("host")
            .map(|h| origin.trim().eq_ignore_ascii_case(&format!("http://{}", h.trim())))
            .unwrap_or(false);
        if !same {
            return Some("cross-origin request refused");
        }
    }
    None
}

fn is_json_content_type(ct: Option<&str>) -> bool {
    ct.and_then(|v| v.split(';').next())
        .map(|m| m.trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false)
}

// -----------------------------------------------------------------------------
// Request parsing
// -----------------------------------------------------------------------------

struct Conn {
    stream: TcpStream,
    peer: SocketAddr,
    /// From the active speaker's addresses when accepted.
    speaker: bool,
    /// Bytes received but not yet consumed.
    buf: Vec<u8>,
}

#[derive(Debug)]
enum ReadErr {
    Timeout,
    Io,
}

impl Conn {
    /// Read whatever arrives before `deadline` into `buf`. Ok(0) = EOF.
    fn fill(&mut self, deadline: Instant) -> Result<usize, ReadErr> {
        let mut tmp = [0u8; 4096];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(ReadErr::Timeout);
            }
            let wait = (deadline - now).max(Duration::from_millis(1));
            self.stream.set_read_timeout(Some(wait)).map_err(|_| ReadErr::Io)?;
            match self.stream.read(&mut tmp) {
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    return Ok(n);
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                        return Err(ReadErr::Timeout)
                    }
                    _ => return Err(ReadErr::Io),
                },
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    None,
    Length(u64),
    Chunked,
}

#[derive(Debug)]
struct Head {
    method: String,
    target: String,
    /// HTTP/1.x minor version (0 or 1).
    minor: u8,
    headers: Vec<(String, String)>,
    body: BodyKind,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn keep_alive(&self) -> bool {
        let close = self
            .header("connection")
            .map(|v| v.to_ascii_lowercase().contains("close"))
            .unwrap_or(false);
        self.minor == 1 && !close
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HeadErr {
    Closed,
    Timeout,
    Io,
    TooLarge,
    Bad,
    Version,
    NotImplemented,
}

impl From<ReadErr> for HeadErr {
    fn from(e: ReadErr) -> Self {
        match e {
            ReadErr::Timeout => HeadErr::Timeout,
            ReadErr::Io => HeadErr::Io,
        }
    }
}

fn find_crlfcrlf(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + from)
}

fn read_head(conn: &mut Conn, timeout: Duration) -> Result<Head, HeadErr> {
    let deadline = Instant::now() + timeout;
    let mut scanned = 0;
    loop {
        // Tolerate stray CRLFs between requests.
        while conn.buf.starts_with(b"\r\n") {
            conn.buf.drain(..2);
            scanned = 0;
        }
        if let Some(pos) = find_crlfcrlf(&conn.buf, scanned) {
            if pos > MAX_HEAD_BYTES {
                return Err(HeadErr::TooLarge);
            }
            let head: Vec<u8> = conn.buf.drain(..pos + 4).collect();
            return parse_head(&head[..pos]);
        }
        if conn.buf.len() > MAX_HEAD_BYTES {
            return Err(HeadErr::TooLarge);
        }
        scanned = conn.buf.len().saturating_sub(3);
        if conn.fill(deadline)? == 0 {
            return Err(if conn.buf.is_empty() { HeadErr::Closed } else { HeadErr::Bad });
        }
    }
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn parse_head(bytes: &[u8]) -> Result<Head, HeadErr> {
    let text = std::str::from_utf8(bytes).map_err(|_| HeadErr::Bad)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(HeadErr::Bad)?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HeadErr::Bad);
    };
    if !is_token(method)
        || !target.starts_with('/')
        || target.bytes().any(|b| b <= b' ' || b == 0x7f)
    {
        return Err(HeadErr::Bad);
    }
    let minor = match version {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        v if v.starts_with("HTTP/") => return Err(HeadErr::Version),
        _ => return Err(HeadErr::Bad),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(HeadErr::TooLarge);
        }
        let (name, value) = line.split_once(':').ok_or(HeadErr::Bad)?;
        if !is_token(name) {
            // Also rejects obsolete line folding (leading whitespace).
            return Err(HeadErr::Bad);
        }
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        if value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) {
            return Err(HeadErr::Bad);
        }
        headers.push((name.to_string(), value.to_string()));
    }

    let te = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"));
    let lengths: Vec<&str> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.as_str())
        .collect();
    let body = if let Some((_, te)) = te {
        // Both framings at once is a request-smuggling shape.
        if !lengths.is_empty() {
            return Err(HeadErr::Bad);
        }
        if !te.trim().eq_ignore_ascii_case("chunked") {
            return Err(HeadErr::NotImplemented);
        }
        BodyKind::Chunked
    } else if let Some(first) = lengths.first() {
        let parse = |v: &str| -> Option<u64> {
            let v = v.trim();
            if v.is_empty() || v.len() > 19 || !v.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            v.parse().ok()
        };
        let n = parse(first).ok_or(HeadErr::Bad)?;
        if lengths.iter().any(|v| parse(v) != Some(n)) {
            return Err(HeadErr::Bad);
        }
        if n == 0 { BodyKind::None } else { BodyKind::Length(n) }
    } else {
        BodyKind::None
    };

    Ok(Head { method: method.to_string(), target: target.to_string(), minor, headers, body })
}

/// Path and query of a request target.
fn split_target(target: &str) -> (&str, &str) {
    target.split_once('?').unwrap_or((target, ""))
}

#[derive(Debug, PartialEq, Eq)]
enum BodyErr {
    TooLarge,
    Timeout,
    Bad,
    Expectation,
    Io,
}

impl BodyErr {
    fn reply(&self) -> Reply {
        match self {
            BodyErr::TooLarge => Reply::text(413, "request body too large"),
            BodyErr::Timeout => Reply::text(408, "request body timed out"),
            BodyErr::Bad | BodyErr::Io => Reply::text(400, "malformed request body"),
            BodyErr::Expectation => Reply::text(417, ""),
        }
    }
}

impl From<ReadErr> for BodyErr {
    fn from(e: ReadErr) -> Self {
        match e {
            ReadErr::Timeout => BodyErr::Timeout,
            ReadErr::Io => BodyErr::Io,
        }
    }
}

/// Read the request body, at most `cap` bytes, within [`BODY_TIMEOUT`].
fn read_body(conn: &mut Conn, head: &Head, cap: usize) -> Result<Vec<u8>, BodyErr> {
    if let BodyKind::Length(n) = head.body {
        if n > cap as u64 {
            return Err(BodyErr::TooLarge);
        }
    }
    match head.header("expect") {
        None => {}
        Some(v) if v.trim().eq_ignore_ascii_case("100-continue") => {
            if head.minor == 1 && head.body != BodyKind::None {
                let _ = conn.stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            }
        }
        Some(_) => return Err(BodyErr::Expectation),
    }
    let deadline = Instant::now() + BODY_TIMEOUT;
    match head.body {
        BodyKind::None => Ok(Vec::new()),
        BodyKind::Length(n) => {
            let n = n as usize;
            while conn.buf.len() < n {
                if conn.fill(deadline)? == 0 {
                    return Err(BodyErr::Bad);
                }
            }
            Ok(conn.buf.drain(..n).collect())
        }
        BodyKind::Chunked => read_chunked(conn, cap, deadline),
    }
}

fn read_line(conn: &mut Conn, deadline: Instant, max: usize) -> Result<String, BodyErr> {
    let mut scanned = 0;
    loop {
        if let Some(p) = conn.buf[scanned..].windows(2).position(|w| w == b"\r\n") {
            let p = scanned + p;
            let line: Vec<u8> = conn.buf.drain(..p + 2).take(p).collect();
            return String::from_utf8(line).map_err(|_| BodyErr::Bad);
        }
        if conn.buf.len() > max {
            return Err(BodyErr::Bad);
        }
        scanned = conn.buf.len().saturating_sub(1);
        if conn.fill(deadline)? == 0 {
            return Err(BodyErr::Bad);
        }
    }
}

fn read_chunked(conn: &mut Conn, cap: usize, deadline: Instant) -> Result<Vec<u8>, BodyErr> {
    let mut out = Vec::new();
    loop {
        let line = read_line(conn, deadline, 1024)?;
        let size = line.split(';').next().unwrap_or("").trim();
        if size.is_empty() || size.len() > 8 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BodyErr::Bad);
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| BodyErr::Bad)?;
        if size == 0 {
            // Trailer section, up to the empty line.
            for _ in 0..MAX_HEADERS {
                if read_line(conn, deadline, 1024)?.is_empty() {
                    return Ok(out);
                }
            }
            return Err(BodyErr::Bad);
        }
        if out.len().saturating_add(size) > cap {
            return Err(BodyErr::TooLarge);
        }
        while conn.buf.len() < size + 2 {
            if conn.fill(deadline)? == 0 {
                return Err(BodyErr::Bad);
            }
        }
        if &conn.buf[size..size + 2] != b"\r\n" {
            return Err(BodyErr::Bad);
        }
        out.extend(conn.buf.drain(..size));
        conn.buf.drain(..2);
    }
}

// -----------------------------------------------------------------------------
// Responses
// -----------------------------------------------------------------------------

struct Reply {
    status: u16,
    content_type: Option<&'static str>,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: (!body.is_empty()).then_some("text/plain; charset=utf-8"),
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    /// A JSON body, serialised by [`to_json`].
    fn json<T: Serialize + ?Sized>(status: u16, value: &T) -> Self {
        Self {
            status,
            content_type: Some("application/json; charset=utf-8"),
            headers: vec![
                ("X-Content-Type-Options", "nosniff".into()),
                ("Cache-Control", "no-store".into()),
            ],
            body: to_json(value).into_bytes(),
        }
    }

    /// `{"error": msg}`.
    fn error(status: u16, msg: &str) -> Self {
        Self::json(status, &ApiError { error: msg })
    }

    /// A 401 for an API request without the key.
    fn unauthorized(msg: &str) -> Self {
        let mut reply = Self::error(401, msg);
        reply.headers.push(("WWW-Authenticate", WWW_AUTHENTICATE.into()));
        reply
    }

    /// A short page under the status page's policy (no script at all).
    fn html(status: u16, body: &'static str) -> Self {
        Self {
            status,
            content_type: Some("text/html; charset=utf-8"),
            headers: vec![
                ("Content-Security-Policy", STATUS_PAGE_CSP.into()),
                ("X-Content-Type-Options", "nosniff".into()),
            ],
            body: body.as_bytes().to_vec(),
        }
        .private()
    }

    /// The page for `/`, `/app.js` and a wrong key: where to find the
    /// link.
    fn link_needed() -> Self {
        let mut reply = Self::html(401, LINK_NEEDED_HTML);
        reply.headers.push(("WWW-Authenticate", WWW_AUTHENTICATE.into()));
        reply
    }

    /// For a reply to an address that may hold the key: no `Referer`
    /// carrying it anywhere, and nothing kept in caches.
    fn private(mut self) -> Self {
        for (name, value) in [("Referrer-Policy", "no-referrer"), ("Cache-Control", "no-store")] {
            if !self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
                self.headers.push((name, value.into()));
            }
        }
        self
    }
}

/// The challenge a 401 names (RFC 7235 requires one); browsers show the
/// page's text for it rather than a password prompt.
const WWW_AUTHENTICATE: &str = "Bearer realm=\"Stream To Speaker\"";

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

fn write_reply(
    w: &mut impl Write,
    minor: u8,
    reply: &Reply,
    head_only: bool,
    close: bool,
) -> std::io::Result<()> {
    let mut out = format!(
        "HTTP/1.{} {} {}\r\nServer: stream-to-speaker\r\nDate: {}\r\n",
        minor,
        reply.status,
        reason_phrase(reply.status),
        http_date(SystemTime::now())
    );
    if let Some(ct) = reply.content_type {
        out.push_str(&format!("Content-Type: {}\r\n", ct));
    }
    for (k, v) in &reply.headers {
        out.push_str(&format!("{}: {}\r\n", k, v));
    }
    out.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
    if close {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    if !head_only {
        bytes.extend_from_slice(&reply.body);
    }
    w.write_all(&bytes)?;
    w.flush()
}

/// swyh-rs's StreamSize::U32maxNotChunked pattern — a fixed (fake)
/// Content-Length of u32::MAX-1 and never chunked transfer. Sonos
/// rejects WAV streams without Content-Length / with Transfer-Encoding,
/// even though both are technically legal HTTP.
const FAKE_CONTENT_LEN: u64 = (u32::MAX - 1) as u64;

/// Status line + headers for GET/HEAD `/stream.raw`. These are the exact
/// bytes (name, order, values) speakers have been receiving, matching
/// swyh-rs's stream responses: Sonos's DLNA stack is pedantic. The
/// `TransferMode.dlna.org: Streaming` header tells renderers the body is
/// a live stream they mustn't seek. Do NOT add Accept-Ranges or
/// contentFeatures.dlna.org — swyh-rs omits both.
fn write_stream_head(w: &mut impl Write, minor: u8, content_length: u64) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.{} 200 OK\r\n\
         Date: {}\r\n\
         Server: stream-to-speaker tiny-http\r\n\
         Content-Type: audio/vnd.wave;codec=1\r\n\
         TransferMode.dlna.org: Streaming\r\n\
         Content-Length: {}\r\n\
         \r\n",
        minor,
        http_date(SystemTime::now()),
        content_length
    );
    w.write_all(head.as_bytes())?;
    w.flush()
}

/// RFC 7231 IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    // Civil-from-days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        weekday,
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// -----------------------------------------------------------------------------
// Web UI
// -----------------------------------------------------------------------------

const STATUS_PAGE_HTML: &str = r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<title>Stream To Speaker</title>
<style>
  body { font-family: -apple-system, system-ui, Segoe UI, sans-serif; max-width: 640px; margin: 2em auto; padding: 0 1em; }
  h1 { margin-bottom: 0.2em; }
  small { color: #666; }
  table { border-collapse: collapse; width: 100%; margin-top: 1em; }
  td, th { padding: 0.4em 0.6em; border-bottom: 1px solid #ddd; text-align: left; }
  button { padding: 0.3em 0.8em; }
  .actions { margin-top: 1.5em; }
  .actions button { margin-right: 0.5em; }
  #pending { margin-left: 0.6em; color: #666; font-size: 0.9em; }
</style>
<script src="app.js" defer></script>
</head><body>
<h1>Stream To Speaker</h1>
<small>Active marker &#9679; shows the currently-targeted device.</small>
<table>
  <thead><tr><th>Speaker</th><th>IP</th><th></th></tr></thead>
  <tbody>{rows}</tbody>
</table>
<div class="actions">
  <button type="button" data-latency="-100" title="Pad 100 ms — sends a sample-doubled gap into the stream so Sonos buffers more, in case a drain went too far and Sonos is glitching">+100 ms latency</button>
  <button type="button" data-latency="-25" title="Pad 25 ms">+25 ms</button>
  <button type="button" data-latency="25" title="Drain 25 ms from Sonos's prebuffer by dropping that much audio, spread over ~0.5 s">&minus;25 ms</button>
  <button type="button" data-latency="100" title="Drain 100 ms — same as above, larger step">&minus;100 ms latency</button>
  &nbsp;
  <button type="button" data-action="resync" title="Hard reset: UPnP Stop + Play, Sonos picks a fresh prebuffer. Brief audio glitch.">resync</button>
  <span id="pending"></span>
</div>
</body></html>"#;

/// Served as `/r/<key>/app.js`. Speaker ids reach it only through `data-*`
/// attributes (read as plain strings), never through generated code. The
/// API's key is the one in the page's own address.
const STATUS_PAGE_JS: &str = r#"'use strict';
const key = location.pathname.split('/')[2] || '';
function post(url, body) {
  const opts = {method: 'POST', headers: {'Authorization': 'Bearer ' + key}};
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  return fetch(url, opts);
}
function selectSpeaker(id) {
  post('/api/select', {id: id}).then(() => location.reload());
}
function resync() {
  post('/api/resync').then(r => r.json()).then(j => {
    if (j.error) { alert('resync failed: ' + j.error); }
  });
}
function adjustLatency(ms) {
  post('/api/latency/adjust?ms=' + encodeURIComponent(ms))
    .then(r => r.json()).then(j => {
      if (j.error) { alert('latency adjust failed: ' + j.error); return; }
      const f = j.pending_frames || 0;
      const remaining_ms = (f / 44.1).toFixed(0);
      document.getElementById('pending').textContent =
        f === 0 ? '' : 'pending: ' + remaining_ms + ' ms';
    });
}
document.addEventListener('click', ev => {
  const b = ev.target.closest('button');
  if (!b) { return; }
  if (b.dataset.select !== undefined) {
    selectSpeaker(b.dataset.select);
  } else if (b.dataset.latency !== undefined) {
    adjustLatency(parseInt(b.dataset.latency, 10));
  } else if (b.dataset.action === 'resync') {
    resync();
  }
});
setTimeout(() => location.reload(), 5000);
"#;

/// The answer for `/`, `/app.js` and a link whose key is wrong or was
/// replaced.
const LINK_NEEDED_HTML: &str = r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<title>Stream To Speaker</title>
<style>body { font-family: -apple-system, system-ui, Segoe UI, sans-serif; max-width: 640px; margin: 2em auto; padding: 0 1em; }</style>
</head><body>
<h1>Stream To Speaker</h1>
<p>The web UI opens only from its link, which holds a secret key. On the PC,
open Stream To Speaker and use <b>Copy link</b> in its Web UI card. If it runs
without a window (<code>--headless</code>, or as a service), run
<code>stream-to-speaker.exe --web-link</code> as the Windows account it runs
under: that prints the link.</p>
<p>A link stops working when <b>Make a new link</b> is used.</p>
</body></html>"#;

const WEB_UI_OFF_HTML: &str = r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<title>Stream To Speaker</title>
<style>body { font-family: -apple-system, system-ui, Segoe UI, sans-serif; max-width: 640px; margin: 2em auto; padding: 0 1em; }</style>
</head><body>
<h1>Stream To Speaker</h1>
<p>The web UI is off. Turn it on in the Stream To Speaker window (Web UI card),
or start the app with <code>--web</code>, then open its link: the window shows
it, and <code>stream-to-speaker.exe --web-link</code> prints it.</p>
</body></html>"#;

/// Pull an integer-valued query parameter from a request URL.
fn extract_query_int(url: &str, key: &str) -> Option<i64> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return v.parse::<i64>().ok();
            }
        }
    }
    None
}

// -----------------------------------------------------------------------------
// JSON replies
// -----------------------------------------------------------------------------

/// One row of `GET /api/speakers`. The field names and their order are the
/// API's wire format; `SpeakerInfo`'s other fields stay out of it.
#[derive(Serialize)]
struct ApiSpeaker<'a> {
    id: &'a str,
    name: &'a str,
    ip: &'a str,
    active: bool,
    manual: bool,
}

/// `GET /api/speakers`. `error` is present only when discovery is off.
#[derive(Serialize)]
struct SpeakerList<'a> {
    speakers: Vec<ApiSpeaker<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[derive(Serialize)]
struct ApiError<'a> {
    error: &'a str,
}

#[derive(Serialize)]
struct ApiOk {
    ok: bool,
}

#[derive(Serialize)]
struct LatencyOk {
    ok: bool,
    pending_frames: i64,
}

fn speaker_list(list: &[SpeakerInfo]) -> SpeakerList<'_> {
    SpeakerList {
        speakers: list
            .iter()
            .map(|sp| ApiSpeaker {
                id: &sp.id,
                name: &sp.friendly_name,
                ip: &sp.ip,
                active: sp.active,
                manual: sp.manual,
            })
            .collect(),
        error: None,
    }
}

/// What [`html_safe`] writes as `\u` escapes: `<` (it starts `</script`
/// and `<!--`), `>`, `&`, and U+2028 / U+2029 (line terminators inside a
/// JavaScript string in older engines). The set Go's encoding/json escapes
/// for the same reason.
const HTML_UNSAFE: [char; 5] = ['<', '>', '&', '\u{2028}', '\u{2029}'];

/// An API body as JSON text that is also safe inside an HTML `<script>`
/// element. Speaker names and ids come from devices on the network, so
/// they may hold any text; serde_json keeps them inside their strings, and
/// [`html_safe`] keeps them from ending a script element if this JSON is
/// ever embedded in a page.
fn to_json<T: Serialize + ?Sized>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(s) => html_safe(s),
        // Our bodies are structs of strings, numbers and booleans, which
        // can't fail to serialise; this only keeps a panic out of a
        // `panic = "abort"` service.
        Err(_) => r#"{"error":"internal error"}"#.to_string(),
    }
}

/// Rewrites every [`HTML_UNSAFE`] character in JSON text as its
/// six-character `\uXXXX` escape. They can only occur inside string
/// literals in JSON, where the escape stands for the same character, so
/// every JSON parser reads the same value.
fn html_safe(json: String) -> String {
    if !json.contains(HTML_UNSAFE) {
        return json;
    }
    let mut out = String::with_capacity(json.len() + 16);
    for c in json.chars() {
        if HTML_UNSAFE.contains(&c) {
            // Built with format! from the code point: a hand-typed escape
            // for `<` is easy to end up as the character itself.
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn extract_json_string(body: &str, key: &str) -> Option<String> {
    // Forgiving extractor: looks for "key" : "value" pattern.
    let needle = format!("\"{}\"", key);
    let idx = body.find(&needle)?;
    let after_key = &body[idx + needle.len()..];
    let colon = after_key.find(':')?;
    let after_colon = &after_key[colon + 1..];
    let start = after_colon.find('"')? + 1;
    let rest = &after_colon[start..];
    let end = find_unescaped_quote(rest)?;
    Some(unescape_json_string(&rest[..end]))
}

fn find_unescaped_quote(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(ch) if hex.len() == 4 => out.push(ch),
                        _ => {
                            out.push_str("\\u");
                            out.push_str(&hex);
                        }
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// -----------------------------------------------------------------------------
// PCM stream body
// -----------------------------------------------------------------------------

/// How often an open stream re-asks the stream gate about its peer, so
/// one the gate stops admitting (streaming disabled, privacy mode turned
/// on, speaker switched) is cut within this long.
const GATE_RECHECK: Duration = Duration::from_secs(1);

/// `Read` adapter that pulls PCM frames from a crossbeam Receiver. Blocks
/// up to a few seconds for a frame; if nothing arrives it returns EOF
/// (closing the stream), which is the cleanest way to signal the client we
/// have nothing to send.
struct StreamReader {
    rx: Receiver<PcmFrame>,
    /// Bytes not yet returned from the last received frame.
    leftover: Vec<u8>,
    /// Offset into `leftover` that has been served.
    leftover_pos: usize,
    /// The stream gate + this client's address; re-checked every
    /// [`GATE_RECHECK`]. `None` when the server has no gate (it then
    /// serves anyone).
    gate: Option<(StreamClientAllowedCallback, std::net::IpAddr)>,
    last_gate_check: std::time::Instant,
}

impl StreamReader {
    fn new(
        rx: Receiver<PcmFrame>,
        gate: Option<(StreamClientAllowedCallback, std::net::IpAddr)>,
    ) -> Self {
        Self {
            rx,
            gate,
            last_gate_check: std::time::Instant::now(),
            // Pre-load a 44-byte RIFF/WAVE header so the consumer (Sonos)
            // sees a valid streaming WAV from byte 0. swyh-rs does the
            // same thing in audio/rwstream.rs.
            leftover: wav_header_streaming().to_vec(),
            leftover_pos: 0,
        }
    }
}

/// Returns a 44-byte WAV/RIFF header configured for streaming "forever".
/// Field values match swyh-rs's create_wav_hdr (rwstream.rs) exactly:
///   - RIFF chunk size = u32::MAX = 0xFFFFFFFF
///   - data chunk size = u32::MAX - 36 = 0xFFFFFFDB  (36 = WAVE + fmt
///     header (8) + fmt payload (16) + data header (8))
/// Sonos parses these as "very large" without computing a finite length.
fn wav_header_streaming() -> [u8; 44] {
    let sample_rate: u32 = WIRE_SAMPLE_RATE;
    let channels:    u16 = WIRE_CHANNELS;
    let bits:        u16 = 16;
    let block_align: u16 = channels * (bits / 8);
    let byte_rate:   u32 = sample_rate * (block_align as u32);
    let riff_size:   u32 = u32::MAX;
    let data_size:   u32 = u32::MAX - 36;

    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff_size.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());          // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes());           // format = PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_size.to_le_bytes());
    h
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.leftover_pos >= self.leftover.len() {
            // Still allowed? EOF closes the connection if not.
            if let Some((cb, peer)) = &self.gate {
                if self.last_gate_check.elapsed() >= GATE_RECHECK {
                    self.last_gate_check = std::time::Instant::now();
                    if let Err(refusal) = cb(*peer) {
                        info!("stream: stopped serving {}: {}", peer, refusal.reason());
                        return Ok(0);
                    }
                }
            }
            // Need a new frame.  Wait up to 30 s; if nothing, signal EOF.
            match self.rx.recv_timeout(Duration::from_secs(30)) {
                Ok(frame) => {
                    self.leftover = (*frame.0).clone();
                    self.leftover_pos = 0;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Ok(0),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }

        let available = self.leftover.len() - self.leftover_pos;
        let n = buf.len().min(available);
        buf[..n].copy_from_slice(&self.leftover[self.leftover_pos..self.leftover_pos + n]);
        self.leftover_pos += n;
        Ok(n)
    }
}

/// Convert a Vec<i16> sample buffer to PCM bytes.
///
/// We stream WAV (RIFF/WAVE) which uses **little-endian** samples — the
/// same byte order as Windows i16 internally, so this is effectively a
/// memcpy reinterpret. We keep the name "_l16_be_bytes" for ABI stability
/// with main.rs but the impl is little-endian. Renderers receiving the
/// WAV header (audio/vnd.wave;codec=1, fmt=PCM) decode accordingly.
///
/// If we ever support raw L16 over HTTP again (RFC 3551 big-endian, no
/// WAV header), add a separate helper.
pub fn samples_to_l16_be_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let b = s.to_le_bytes();
        out.push(b[0]);
        out.push(b[1]);
    }
    out
}

/// Suggested bytes-per-second on the wire, for buffer-sizing math.
pub fn wire_bytes_per_second() -> usize {
    (WIRE_SAMPLE_RATE as usize) * WIRE_BYTES_PER_FRAME
}

/// Just a const stash so users can confirm channel count without importing
/// from lib.rs directly.
pub const WIRE_CHANNELS_PUBLIC: u16 = WIRE_CHANNELS;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    /// What this module writes to the log while the tests run, for tests
    /// of what a request leaves there. The tests share one process and one
    /// logger, so each picks out the lines its own requests caused.
    mod log_capture {
        use std::sync::{Mutex, Once};

        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

        struct Capture;

        impl log::Log for Capture {
            fn enabled(&self, metadata: &log::Metadata) -> bool {
                metadata.target().starts_with("stream_to_speaker::http_server")
            }
            fn log(&self, record: &log::Record) {
                if self.enabled(record.metadata()) {
                    LINES.lock().unwrap().push(record.args().to_string());
                }
            }
            fn flush(&self) {}
        }

        static CAPTURE: Capture = Capture;

        /// Capture from now on, at every level. A logger installed before
        /// this one would leave the tests looking at nothing, so that fails.
        pub(super) fn start() {
            static ONCE: Once = Once::new();
            ONCE.call_once(|| {
                log::set_logger(&CAPTURE).expect("no other logger in the tests");
                log::set_max_level(log::LevelFilter::Trace);
            });
        }

        pub(super) fn lines() -> Vec<String> {
            LINES.lock().unwrap().clone()
        }
    }

    #[test]
    fn disconnect_clients_cuts_matching_http_peers_only() {
        let hub = StreamHub::new();
        let internal = hub.subscribe(); // e.g. an AirPlay sender
        let stranger = hub.subscribe_from("192.168.1.99".parse::<IpAddr>().unwrap());
        let speaker = hub.subscribe_from("192.168.1.50".parse::<IpAddr>().unwrap());
        assert_eq!(hub.subscriber_count(), 3);

        let cut = hub.disconnect_clients(|ip| ip != "192.168.1.50".parse::<IpAddr>().unwrap());
        assert_eq!(cut, 1);
        assert_eq!(hub.subscriber_count(), 2);

        hub.publish(PcmFrame(Arc::new(vec![0u8; 4])));
        // The stranger's channel is dead — its reader sees EOF.
        assert!(matches!(
            stranger.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
        // The speaker and the internal consumer still get audio.
        assert!(speaker.try_recv().is_ok());
        assert!(internal.try_recv().is_ok());
    }

    // ---- pure helpers -------------------------------------------------

    fn policy() -> HostPolicy {
        fn local(ip: IpAddr) -> bool {
            ip == "192.168.1.10".parse::<IpAddr>().unwrap()
        }
        HostPolicy::new("0.0.0.0".parse().unwrap(), &["10.9.9.9".to_string()], local)
    }

    #[test]
    fn host_policy_accepts_own_addresses_and_rejects_foreign_names() {
        let p = policy();
        for ok in [
            "localhost", "LOCALHOST:5901", "127.0.0.1:5901", "[::1]:5901", "192.168.1.10:5901",
            "192.168.1.10", "10.9.9.9:5901", "[::ffff:192.168.1.10]:5901",
            "[::1%25eth0]:5901", "[::1%eth0]",
        ] {
            assert!(p.allows(Some(ok)), "{ok}");
        }
        assert!(p.allows(None));
        for bad in [
            "evil.example:5901", "evil.example", "192.168.1.99:5901", "0.0.0.0:5901",
            "localhost.evil.example", "127.0.0.1:abc", "[::1", "a:b:c",
        ] {
            assert!(!p.allows(Some(bad)), "{bad}");
        }
    }

    fn head(raw: &str) -> Result<Head, HeadErr> {
        let raw = raw.replace('\n', "\r\n");
        parse_head(raw.trim_end().as_bytes())
    }

    #[test]
    fn host_policy_accepts_this_machines_names() {
        let p = policy();
        for n in machine_names() {
            let n = n.trim().trim_end_matches('.');
            if !n.is_empty() {
                assert!(p.allows(Some(&format!("{}:5901", n.to_ascii_uppercase()))), "{n}");
                assert!(!p.allows(Some(&format!("{}.evil.example", n))), "{n}");
            }
        }
    }

    #[test]
    fn parse_head_framing() {
        let h = head("POST /api/select HTTP/1.1\nHost: x\nContent-Length: 12\n").unwrap();
        assert_eq!(h.body, BodyKind::Length(12));
        assert!(h.keep_alive());
        let h = head("NOTIFY /gena HTTP/1.1\nTransfer-Encoding: chunked\n").unwrap();
        assert_eq!(h.body, BodyKind::Chunked);
        let h = head("GET / HTTP/1.0\n").unwrap();
        assert!(!h.keep_alive());
        assert_eq!(head("GET / HTTP/1.1\nContent-Length: 1\nContent-Length: 2\n").unwrap_err(), HeadErr::Bad);
        assert_eq!(head("GET / HTTP/1.1\nContent-Length: 1\nTransfer-Encoding: chunked\n").unwrap_err(), HeadErr::Bad);
        assert_eq!(head("GET / HTTP/1.1\nContent-Length: -1\n").unwrap_err(), HeadErr::Bad);
        assert_eq!(head("GET / HTTP/1.1\nContent-Length: 99999999999999999999999\n").unwrap_err(), HeadErr::Bad);
        assert_eq!(head("GET / HTTP/1.1\nTransfer-Encoding: gzip\n").unwrap_err(), HeadErr::NotImplemented);
        assert_eq!(head("GET / HTTP/2.0\n").unwrap_err(), HeadErr::Version);
        assert_eq!(head("GET / HTTP/1.1\nX: a\n folded\n").unwrap_err(), HeadErr::Bad);
        assert_eq!(head("GET  / HTTP/1.1\n").unwrap_err(), HeadErr::Bad);
    }

    #[test]
    fn cross_site_checks() {
        let same = head("POST /api/resync HTTP/1.1\nHost: 192.168.1.10:5901\nOrigin: http://192.168.1.10:5901\nSec-Fetch-Site: same-origin\n").unwrap();
        assert_eq!(cross_site_refusal(&same), None);
        let tool = head("POST /api/resync HTTP/1.1\nHost: localhost:5901\n").unwrap();
        assert_eq!(cross_site_refusal(&tool), None);
        let evil = head("POST /api/resync HTTP/1.1\nHost: 192.168.1.10:5901\nOrigin: http://evil.example\n").unwrap();
        assert!(cross_site_refusal(&evil).is_some());
        let null = head("POST /api/resync HTTP/1.1\nHost: 192.168.1.10:5901\nOrigin: null\n").unwrap();
        assert!(cross_site_refusal(&null).is_some());
        let site = head("POST /api/resync HTTP/1.1\nHost: 192.168.1.10:5901\nSec-Fetch-Site: cross-site\n").unwrap();
        assert!(cross_site_refusal(&site).is_some());
        assert!(is_json_content_type(Some("application/json; charset=utf-8")));
        assert!(!is_json_content_type(Some("text/plain")));
        assert!(!is_json_content_type(None));
    }

    #[test]
    fn http_date_formats() {
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            http_date(UNIX_EPOCH + Duration::from_secs(1_000_000_000)),
            "Sun, 09 Sep 2001 01:46:40 GMT"
        );
        assert_eq!(
            http_date(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "Tue, 29 Feb 2000 00:00:00 GMT"
        );
    }

    /// A connected socket pair over loopback.
    fn socket_pair(listener: &TcpListener) -> (TcpStream, TcpStream) {
        let c = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (s, _) = listener.accept().unwrap();
        (c, s)
    }

    /// Has the far end been shut down? (A live, quiet peer times out.)
    fn is_shut(c: &mut TcpStream) -> bool {
        c.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut b = [0u8; 1];
        match c.read(&mut b) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
        }
    }

    #[test]
    fn conn_limiter_evicts_within_budget_and_spares_the_speaker() {
        let l = Arc::new(ConnLimiter::default());
        let lst = TcpListener::bind("127.0.0.1:0").unwrap();
        let a: IpAddr = "192.168.1.99".parse().unwrap();
        let speaker: IpAddr = "192.168.1.50".parse().unwrap();

        let (mut sc, ss) = socket_pair(&lst);
        let _sp = l.admit(speaker, true, &ss).unwrap();
        // One host past its own budget displaces only its own oldest.
        let mut first = None;
        let mut permits = Vec::new();
        for i in 0..=MAX_CONNECTIONS_PER_PEER {
            let (c, s) = socket_pair(&lst);
            permits.push(l.admit(a, false, &s).unwrap());
            if i == 0 {
                first = Some(c);
            }
        }
        assert!(is_shut(first.as_mut().unwrap()));
        assert_eq!(l.count(), 1 + MAX_CONNECTIONS_PER_PEER);
        // Many hosts filling the shared budget never touch the speaker.
        for i in 0..MAX_CONNECTIONS + 8 {
            let (_c, s) = socket_pair(&lst);
            let ip = IpAddr::from([10, 0, (i / 200) as u8, (i % 200) as u8 + 1]);
            permits.push(l.admit(ip, false, &s).unwrap());
        }
        assert_eq!(l.count(), 1 + MAX_CONNECTIONS);
        assert!(!is_shut(&mut sc));
    }

    // ---- live server --------------------------------------------------

    fn start(cfg: HttpServerConfig) -> u16 {
        start_http_server(cfg).unwrap()
    }

    fn minimal() -> (u16, Arc<StreamHub>) {
        let hub = StreamHub::new();
        let port = start(HttpServerConfig::minimal("127.0.0.1:0".parse().unwrap(), hub.clone()));
        (port, hub)
    }

    fn connect(port: u16) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    /// Send `req`, read until close or `n` bytes.
    fn exchange(port: u16, req: &[u8], n: usize) -> String {
        String::from_utf8_lossy(&exchange_raw(port, req, n)).to_string()
    }

    /// [`exchange`], returning the bytes as they arrived.
    fn exchange_raw(port: u16, req: &[u8], n: usize) -> Vec<u8> {
        let mut s = connect(port);
        s.write_all(req).unwrap();
        let mut buf = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match s.read(&mut buf[got..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => got += k,
            }
        }
        buf.truncate(got);
        buf
    }

    fn healthz(port: u16) -> String {
        exchange(port, b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", 4096)
    }

    /// Connect to the server from a chosen loopback source address.
    fn connect_from(src: [u8; 4], port: u16) -> TcpStream {
        use socket2::{Domain, Socket, Type};
        let sock = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        sock.bind(&SocketAddr::from((src, 0)).into()).unwrap();
        sock.connect(&SocketAddr::from(([127, 0, 0, 1], port)).into()).unwrap();
        let s: TcpStream = sock.into();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    #[test]
    fn stalled_stream_listeners_cannot_lock_out_the_speaker() {
        let hub = StreamHub::new();
        let mut cfg = HttpServerConfig::minimal("127.0.0.1:0".parse().unwrap(), hub.clone());
        let speaker: IpAddr = "127.0.0.1".parse().unwrap();
        cfg.speaker_peer = Some(Arc::new(move |ip| ip == speaker));
        let port = start(cfg);
        let h = hub.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                h.publish(PcmFrame(Arc::new(vec![1u8; 4096])));
                thread::sleep(Duration::from_millis(2));
            }
        });
        // Many hosts open streams and never read them. (Loopback sources
        // are protected themselves; eviction of LAN hosts is covered by
        // the limiter test above.)
        let mut hogs = Vec::new();
        for host in 2..=12u8 {
            for _ in 0..MAX_CONNECTIONS_PER_PEER {
                let mut s = connect_from([127, 0, 0, host], port);
                s.write_all(b"GET /stream.raw HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
                hogs.push(s);
            }
        }
        thread::sleep(Duration::from_millis(300));
        let probe = |src: [u8; 4]| {
            let mut s = connect_from(src, port);
            s.write_all(b"HEAD /stream.raw HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut buf = [0u8; 512];
            let n = s.read(&mut buf).unwrap_or(0);
            String::from_utf8_lossy(&buf[..n]).to_string()
        };
        // The speaker gets through, and so does a newcomer.
        assert!(probe([127, 0, 0, 1]).starts_with("HTTP/1.1 200"));
        assert!(probe([127, 0, 0, 13]).starts_with("HTTP/1.1 200"));
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn huge_content_length_is_refused_without_reading() {
        let (port, _hub) = minimal();
        let r = exchange(
            port,
            b"NOTIFY /gena HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 99999999999999\r\n\r\n",
            4096,
        );
        assert!(r.starts_with("HTTP/1.1 412"), "{r}");
        let r = exchange(
            port,
            b"POST /nope HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 99999999999999\r\n\r\n",
            4096,
        );
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        assert!(r.contains("Connection: close"), "{r}");
        assert!(healthz(port).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn stalled_clients_do_not_block_others() {
        let (port, hub) = minimal();
        // Half a request head, and a body that never arrives.
        let mut a = connect(port);
        a.write_all(b"GET /stream.raw HTTP/1.1\r\nHo").unwrap();
        let mut b = connect(port);
        b.write_all(b"POST /api/select HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2000\r\n\r\n{").unwrap();
        let t = Instant::now();
        assert!(healthz(port).starts_with("HTTP/1.1 200"));
        assert!(t.elapsed() < Duration::from_secs(2));

        // The stream still starts promptly.
        let h = hub.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                h.publish(PcmFrame(Arc::new(vec![1u8; 64])));
                thread::sleep(Duration::from_millis(5));
            }
        });
        let r = exchange(port, b"GET /stream.raw HTTP/1.1\r\nHost: 127.0.0.1:5901\r\n\r\n", 220);
        stop.store(true, Ordering::Relaxed);
        assert!(r.starts_with("HTTP/1.1 200 OK\r\nDate: "), "{r}");
        assert!(r.contains(
            "\r\nServer: stream-to-speaker tiny-http\r\nContent-Type: audio/vnd.wave;codec=1\r\nTransferMode.dlna.org: Streaming\r\nContent-Length: 4294967294\r\n\r\nRIFF"
        ), "{r}");
        drop((a, b));
    }

    #[test]
    fn head_probe_then_get_on_one_connection() {
        let (port, hub) = minimal();
        let mut s = connect(port);
        s.write_all(b"HEAD /stream.raw HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut buf = [0u8; 512];
        let n = s.read(&mut buf).unwrap();
        let r = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n") && r.ends_with("Content-Length: 0\r\n\r\n"), "{r}");
        assert!(!r.contains("Connection"), "{r}");
        hub.publish(PcmFrame(Arc::new(vec![0u8; 4])));
        s.write_all(b"GET /stream.raw HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let n = s.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200 OK"));
    }

    #[test]
    fn an_open_stream_ends_when_the_gate_closes() {
        // The gate closing for a listener — streaming disabled, say — ends
        // a stream already being served within about a second, and later
        // requests get a 403. Both, and the log, say which rule refused.
        log_capture::start();
        let hub = StreamHub::new();
        // 0: admitted; 1: streaming disabled; 2: privacy mode refuses.
        let state = Arc::new(AtomicU8::new(0));
        let gate = state.clone();
        let mut cfg = HttpServerConfig::minimal("127.0.0.1:0".parse().unwrap(), hub.clone());
        cfg.stream_client_allowed = Some(Arc::new(move |_| match gate.load(Ordering::Acquire) {
            0 => Ok(()),
            1 => Err(Refusal::StreamingOff),
            _ => Err(Refusal::NotGranted),
        }));
        let port = start(cfg);
        let h = hub.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                h.publish(PcmFrame(Arc::new(vec![1u8; 256])));
                thread::sleep(Duration::from_millis(5));
            }
        });
        let mut s = connect(port);
        s.write_all(b"GET /stream.raw HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut buf = vec![0u8; 8192];
        let n = s.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200 OK"));
        state.store(1, Ordering::Release);
        let closed_at = Instant::now();
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => assert!(closed_at.elapsed() < Duration::from_secs(4), "still streaming"),
                Err(e) => panic!("stream didn't end: {e}"),
            }
        }
        assert!(closed_at.elapsed() < Duration::from_millis(2500), "{:?}", closed_at.elapsed());
        let refused = |method: &str| {
            let r = exchange(
                port,
                format!("{method} /stream.raw HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
                4096,
            );
            assert!(r.starts_with("HTTP/1.1 403"), "{method}: {r}");
            r.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default()
        };
        let streaming_off = "streaming is disabled on this PC";
        let privacy = "privacy mode is on: only the speaker being played on (and its group) may listen";
        assert_eq!(refused("HEAD"), "");
        assert_eq!(refused("GET"), format!("forbidden: {streaming_off}"));
        state.store(2, Ordering::Release);
        assert_eq!(refused("HEAD"), "");
        assert_eq!(refused("GET"), format!("forbidden: {privacy}"));
        stop.store(true, Ordering::Relaxed);
        let log = log_capture::lines();
        for line in [
            format!("stream: stopped serving 127.0.0.1: {streaming_off}"),
            format!("stream: refused HEAD /stream.raw from 127.0.0.1: {streaming_off}"),
            format!("stream: refused GET /stream.raw from 127.0.0.1: {streaming_off}"),
            format!("stream: refused GET /stream.raw from 127.0.0.1: {privacy}"),
        ] {
            assert!(log.contains(&line), "no {line:?} in {log:#?}");
        }
    }

    #[test]
    fn rebinding_host_is_refused() {
        let (port, _hub) = minimal();
        let r = exchange(port, b"GET /stream.raw HTTP/1.1\r\nHost: evil.example:5901\r\n\r\n", 4096);
        assert!(r.starts_with("HTTP/1.1 421"), "{r}");
    }

    #[test]
    fn notify_requires_live_subscription_from_its_publisher() {
        let hub = StreamHub::new();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let mut cfg = HttpServerConfig::minimal("127.0.0.1:0".parse().unwrap(), hub);
        cfg.gena_callback = Some(Arc::new(move |_p: &str, body: &str| {
            seen2.lock().unwrap().push(body.to_string());
        }));
        let port = start(cfg);
        let reg = gena::registry();
        let token = reg.begin_subscribe(vec!["127.0.0.1".parse().unwrap()]);
        reg.finish_subscribe(token, Some("uuid:test-http-notify"));
        let token = reg.begin_subscribe(vec!["192.0.2.7".parse().unwrap()]);
        reg.finish_subscribe(token, Some("uuid:test-http-elsewhere"));

        let send = |sid: &str, body: &str| {
            exchange(
                port,
                format!(
                    "NOTIFY /gena HTTP/1.1\r\nHost: 127.0.0.1\r\nSID: {sid}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
                4096,
            )
        };
        assert!(send("uuid:test-http-notify", "<a/>").starts_with("HTTP/1.1 200"));
        assert!(send("uuid:forged", "<b/>").starts_with("HTTP/1.1 412"));
        // Right SID, wrong source address.
        assert!(send("uuid:test-http-elsewhere", "<c/>").starts_with("HTTP/1.1 412"));
        // Chunked body from the right publisher.
        let r = exchange(
            port,
            b"NOTIFY /gena HTTP/1.1\r\nHost: 127.0.0.1\r\nSID: uuid:test-http-notify\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n<d/\r\n1\r\n>\r\n0\r\n\r\n",
            4096,
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert_eq!(*seen.lock().unwrap(), vec!["<a/>".to_string(), "<d/>".to_string()]);
        reg.remove("uuid:test-http-notify");
        assert!(send("uuid:test-http-notify", "<e/>").starts_with("HTTP/1.1 412"));
    }

    /// The web UI's key in the API tests.
    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn auth() -> String {
        format!("Authorization: Bearer {KEY}\r\n")
    }

    struct ApiServer {
        port: u16,
        picked: Arc<Mutex<Vec<String>>>,
        resyncs: Arc<Mutex<u32>>,
        access: Arc<WebAccess>,
    }

    fn api_server() -> ApiServer {
        let hub = StreamHub::new();
        let picked = Arc::new(Mutex::new(Vec::new()));
        let picked2 = picked.clone();
        let resyncs = Arc::new(Mutex::new(0));
        let resyncs2 = resyncs.clone();
        let access = WebAccess::with_secret(crate::web_access::WebSecret::parse(KEY).unwrap());
        let mut cfg = HttpServerConfig::minimal("127.0.0.1:0".parse().unwrap(), hub);
        cfg.speaker_list = Some(Arc::new(|| {
            vec![SpeakerInfo {
                id: "uuid:x');alert(1);//\"><script>alert(2)</script>".into(),
                friendly_name: "<img src=x onerror=alert(3)>".into(),
                ip: "192.168.1.50".into(),
                active: true,
                note: None,
                manual: false,
                hidden: false,
            }]
        }));
        cfg.speaker_select = Some(Arc::new(move |id: &str| {
            picked2.lock().unwrap().push(id.to_string());
            Ok(())
        }));
        cfg.resync = Some(Arc::new(move || {
            *resyncs2.lock().unwrap() += 1;
            Ok(())
        }));
        cfg.latency_adjust = Some(Arc::new(|ms| ms as i64 * 10));
        cfg.web_access = Some(access.clone());
        ApiServer { port: start(cfg), picked, resyncs, access }
    }

    fn get(port: u16, path: &str, extra: &str) -> String {
        exchange(
            port,
            format!("GET {path} HTTP/1.1\r\nHost: localhost:5901\r\nConnection: close\r\n{extra}\r\n").as_bytes(),
            65536,
        )
    }

    fn header<'a>(reply: &'a str, name: &str) -> Option<&'a str> {
        let head = reply.split("\r\n\r\n").next()?;
        head.split("\r\n").find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    #[test]
    fn status_page_has_csp_and_no_inline_code() {
        let srv = api_server();
        let r = get(srv.port, &format!("/r/{KEY}/"), "");
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("Content-Security-Policy: default-src 'none'; script-src 'self'"), "{r}");
        assert!(!r.contains("onclick"));
        assert!(!r.contains("<script>"));
        assert!(!r.contains("<img"));
        assert!(r.contains("data-select=\"uuid:x&#39;);alert(1);//&quot;&gt;&lt;script&gt;"), "{r}");
        // Its script is addressed relative to the page, under the link.
        assert!(r.contains("<script src=\"app.js\" defer>"), "{r}");
        let js = get(srv.port, &format!("/r/{KEY}/app.js"), "");
        assert!(js.contains("application/javascript") && js.contains("dataset.select"));
        assert!(js.contains("'Authorization': 'Bearer ' + key"), "{js}");
    }

    #[test]
    fn the_page_opens_only_from_its_link() {
        let srv = api_server();
        let page = format!("/r/{KEY}/");
        for r in [get(srv.port, &page, ""), get(srv.port, &format!("{page}app.js"), "")] {
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
            assert_eq!(header(&r, "Referrer-Policy"), Some("no-referrer"), "{r}");
            assert_eq!(header(&r, "Cache-Control"), Some("no-store"), "{r}");
        }
        // `/`, `/app.js`, a wrong key, a key of the wrong length.
        for path in [
            "/".to_string(),
            "/app.js".to_string(),
            "/r/00000000000000000000000000000000/".to_string(),
            format!("/r/{}/", &KEY[..31]),
            format!("/r/{}/app.js", KEY.replace('0', "1")),
            "/r//".to_string(),
        ] {
            let r = get(srv.port, &path, "");
            assert!(r.starts_with("HTTP/1.1 401"), "{path}: {r}");
            assert!(header(&r, "WWW-Authenticate").is_some(), "{r}");
            // Where the link is: the window, or --web-link without one.
            assert!(r.contains("Copy link") && r.contains("--web-link"), "{r}");
            assert!(!r.contains("data-select") && !r.contains("192.168.1.50"), "{path}: {r}");
            assert_eq!(header(&r, "Referrer-Policy"), Some("no-referrer"), "{r}");
        }
        // Without the trailing slash: sent to the page.
        let r = get(srv.port, &format!("/r/{KEY}"), "");
        assert!(r.starts_with("HTTP/1.1 308"), "{r}");
        assert_eq!(header(&r, "Location"), Some(page.as_str()), "{r}");
        assert!(get(srv.port, &format!("{page}nothing-here"), "").starts_with("HTTP/1.1 404"));
        // The key in the address is not a key for the API...
        assert!(get(srv.port, &format!("{page}api/speakers"), "").starts_with("HTTP/1.1 404"));
        // ...and a bearer token doesn't open `/`.
        assert!(get(srv.port, "/", &auth()).starts_with("HTTP/1.1 401"));
    }

    #[test]
    fn every_api_route_needs_the_key() {
        let srv = api_server();
        let request = |line: &str, extra: &str| {
            exchange(
                srv.port,
                format!(
                    "{line} HTTP/1.1\r\nHost: 127.0.0.1:5901\r\nConnection: close\r\n\
                     Content-Type: application/json\r\nContent-Length: 17\r\n{extra}\r\n{{\"id\":\"uuid:abc\"}}"
                )
                .as_bytes(),
                65536,
            )
        };
        let routes = ["GET /api/speakers", "POST /api/select", "POST /api/resync", "POST /api/latency/adjust?ms=25"];
        let wrong = [
            String::new(),
            "Authorization: Bearer 00000000000000000000000000000000\r\n".to_string(),
            format!("Authorization: Basic {KEY}\r\n"),
            format!("Authorization: {KEY}\r\n"),
            format!("Authorization: Bearer {KEY}0\r\n"),
            format!("X-Api-Key: {KEY}\r\n"),
        ];
        for line in routes {
            for extra in &wrong {
                let r = request(line, extra);
                assert!(r.starts_with("HTTP/1.1 401"), "{line} {extra:?}: {r}");
                assert!(header(&r, "Content-Type").is_some_and(|c| c.starts_with("application/json")), "{r}");
                assert!(header(&r, "WWW-Authenticate").is_some(), "{r}");
                let body = r.split("\r\n\r\n").nth(1).unwrap();
                let doc: serde_json::Value = serde_json::from_str(body).unwrap();
                assert!(doc["error"].as_str().unwrap().contains("Authorization: Bearer"), "{body}");
                assert!(!r.contains(KEY), "a reply names the key: {r}");
            }
        }
        // Nothing reached the app.
        assert!(srv.picked.lock().unwrap().is_empty());
        assert_eq!(*srv.resyncs.lock().unwrap(), 0);
        // With it, every route works.
        for line in routes {
            assert!(request(line, &auth()).starts_with("HTTP/1.1 200"), "{line}");
        }
        assert!(request("POST /api/resync", &format!("authorization: bearer {KEY}\r\n")).starts_with("HTTP/1.1 200"));
        assert_eq!(*srv.picked.lock().unwrap(), vec!["uuid:abc".to_string()]);
        assert_eq!(*srv.resyncs.lock().unwrap(), 2);
    }

    #[test]
    fn a_script_without_the_key_is_told_what_to_send_in_plain_text() {
        // Script authors meet this body raw (curl prints it as it comes),
        // so it must hold nothing JSON or `html_safe` would escape: the
        // angle brackets of a `<key>` placeholder arrive as six-character
        // escapes, quotes with a backslash.
        let expected = concat!(
            r#"{"error":"missing or wrong API key: send the header 'Authorization: Bearer' "#,
            r#"followed by a space and the key, the 32 hex digits after /r/ in the web UI's "#,
            r#"link. The Stream To Speaker window shows the link in its Web UI card, and "#,
            r#"stream-to-speaker.exe --web-link prints it"}"#,
        );
        let srv = api_server();
        for req in [
            "GET /api/speakers HTTP/1.1\r\nHost: localhost:5901\r\nConnection: close\r\n\r\n".to_string(),
            "POST /api/resync HTTP/1.1\r\nHost: localhost:5901\r\nConnection: close\r\n\
             Authorization: Bearer 00000000000000000000000000000000\r\nContent-Length: 0\r\n\r\n"
                .to_string(),
        ] {
            let raw = exchange_raw(srv.port, req.as_bytes(), 65536);
            let text = String::from_utf8_lossy(&raw);
            assert!(raw.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"), "{text}");
            let at = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a complete reply");
            let body = &raw[at + 4..];
            assert!(body.is_ascii() && !body.contains(&b'\\'), "{text}");
            assert_eq!(body, expected.as_bytes(), "{text}");
        }
    }

    #[test]
    fn a_new_secret_retires_the_old_link_and_key_at_once() {
        let srv = api_server();
        let new = crate::web_access::WebSecret::generate().unwrap();
        let new_key = new.to_hex();
        srv.access.set_secret(new);
        assert!(get(srv.port, &format!("/r/{KEY}/"), "").starts_with("HTTP/1.1 401"));
        assert!(get(srv.port, "/api/speakers", &auth()).starts_with("HTTP/1.1 401"));
        assert!(get(srv.port, &format!("/r/{new_key}/"), "").starts_with("HTTP/1.1 200"));
        let bearer = format!("Authorization: Bearer {new_key}\r\n");
        assert!(get(srv.port, "/api/speakers", &bearer).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn link_keys_never_reach_the_log() {
        // Every line that logs a request path replaces the key after /r/,
        // the right one or not: the line each request gets, the one for a
        // request refused for its Host, and an event callback's path.
        log_capture::start();
        let srv = api_server();
        let marker = format!("log-probe-{}", srv.port);
        let wrong = "fedcba9876543210fedcba9876543210";
        let request = |method: &str, key: &str, extra: &str| {
            exchange(
                srv.port,
                format!(
                    "{method} /r/{key}/{marker}?q=1 HTTP/1.1\r\nConnection: close\r\n{}{extra}\r\n",
                    auth()
                )
                .as_bytes(),
                65536,
            )
        };
        assert!(request("GET", KEY, "Host: localhost:5901\r\n").starts_with("HTTP/1.1 404"));
        assert!(request("GET", wrong, "Host: localhost:5901\r\n").starts_with("HTTP/1.1 401"));
        assert!(request("GET", KEY, "Host: evil.example\r\n").starts_with("HTTP/1.1 421"));
        let sid = format!("uuid:{marker}");
        let reg = gena::registry();
        let token = reg.begin_subscribe(vec!["127.0.0.1".parse().unwrap()]);
        reg.finish_subscribe(token, Some(&sid));
        let notify = request("NOTIFY", KEY, &format!("Host: 127.0.0.1\r\nSID: {sid}\r\nContent-Length: 0\r\n"));
        reg.remove(&sid);
        assert!(notify.starts_with("HTTP/1.1 200"), "{notify}");

        let log = log_capture::lines();
        let ours: Vec<&String> = log.iter().filter(|l| l.contains(&marker)).collect();
        let redacted = format!("/r/<key>/{marker}");
        for expected in [
            format!("http request: GET {redacted}?q=1"),
            format!("http request: NOTIFY {redacted}?q=1"),
            format!("http: refused GET {redacted} from 127.0.0.1: unrecognised Host \"evil.example\""),
            format!("NOTIFY received but no callback registered; path={redacted}"),
        ] {
            assert!(ours.iter().any(|l| **l == expected), "no {expected:?} in {ours:#?}");
        }
        assert_eq!(ours.iter().filter(|l| l.starts_with("http request: GET")).count(), 3, "{ours:#?}");
        // Nor anywhere else, from any test's requests.
        for line in &log {
            assert!(!line.contains(KEY) && !line.contains(wrong), "{line}");
        }
    }

    #[test]
    fn with_the_web_ui_off_nothing_answers_even_with_the_key() {
        let srv = api_server();
        srv.access.set_enabled(false);
        let page = get(srv.port, &format!("/r/{KEY}/"), "");
        assert!(page.starts_with("HTTP/1.1 404") && page.contains("The web UI is off"), "{page}");
        assert!(page.contains("--web-link"), "{page}");
        assert!(!page.contains("data-select"));
        let api = get(srv.port, "/api/speakers", &auth());
        assert!(api.starts_with("HTTP/1.1 404") && api.contains("web UI is disabled"), "{api}");
        // No web access configured at all: the same.
        let (port, _hub) = minimal();
        assert!(get(port, &format!("/r/{KEY}/"), "").starts_with("HTTP/1.1 404"));
        assert!(get(port, "/api/speakers", &auth()).starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn select_api_checks_origin_and_content_type() {
        let srv = api_server();
        let post = |extra: &str, body: &str| {
            exchange(
                srv.port,
                format!(
                    "POST /api/select HTTP/1.1\r\nHost: 127.0.0.1:5901\r\nConnection: close\r\n{}{extra}Content-Length: {}\r\n\r\n{body}",
                    auth(),
                    body.len()
                )
                .as_bytes(),
                4096,
            )
        };
        let body = r#"{"id":"uuid:abc"}"#;
        assert!(post("Content-Type: text/plain\r\n", body).starts_with("HTTP/1.1 415"));
        // The key doesn't make another site's page welcome.
        assert!(post("Content-Type: application/json\r\nOrigin: http://evil.example\r\n", body)
            .starts_with("HTTP/1.1 403"));
        assert!(post("Content-Type: application/json\r\nSec-Fetch-Site: cross-site\r\n", body)
            .starts_with("HTTP/1.1 403"));
        assert!(post(
            "Content-Type: application/json\r\nOrigin: http://127.0.0.1:5901\r\nSec-Fetch-Site: same-origin\r\n",
            body
        )
        .starts_with("HTTP/1.1 200"));
        assert!(post("Content-Type: application/json\r\n", body).starts_with("HTTP/1.1 200"));
        assert_eq!(*srv.picked.lock().unwrap(), vec!["uuid:abc".to_string(), "uuid:abc".to_string()]);
    }

    #[test]
    fn latency_route_matches_with_query() {
        let srv = api_server();
        let r = exchange(
            srv.port,
            format!(
                "POST /api/latency/adjust?ms=25 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{}\r\n",
                auth()
            )
            .as_bytes(),
            4096,
        );
        assert!(r.starts_with("HTTP/1.1 200") && r.contains(r#""pending_frames":250"#), "{r}");
    }

    // ---- JSON replies -------------------------------------------------

    /// Names a LAN device can announce (an SSDP friendlyName, an mDNS
    /// instance label): markup, JSON-breaking quotes, control characters,
    /// JavaScript line terminators, non-ASCII.
    const HOSTILE_NAMES: &[&str] = &[
        "</script><script>alert(1)</script>",
        "</SCRIPT ><img src=x onerror=alert(1)>",
        "<!--<script>",
        "\"},{\"id\":\"evil\",\"name\":\"x",
        "back\\slash\\\"quote\\",
        "nul\u{0}bell\u{7}bs\u{8}ff\u{c}esc\u{1b}us\u{1f}del\u{7f}",
        "line\u{2028}para\u{2029}end",
        "Living & Dining <Kitchen>",
        "Küche \u{1F50A} Гостиная",
        "",
    ];

    /// `\u` and four hex digits, built at run time so no escape literal in
    /// this file can turn into the character it stands for.
    fn esc(c: char) -> String {
        format!("\\u{:04x}", c as u32)
    }

    fn speaker(id: &str, name: &str) -> SpeakerInfo {
        SpeakerInfo {
            id: id.into(),
            friendly_name: name.into(),
            ip: "192.168.1.50".into(),
            active: true,
            note: Some("not in the API".into()),
            manual: false,
            hidden: false,
        }
    }

    #[test]
    fn html_safe_writes_six_character_escapes() {
        let e = esc('<');
        assert_eq!(e.as_bytes(), [b'\\', b'u', b'0', b'0', b'3', b'c']);
        assert_eq!(html_safe("\"<\"".to_string()).as_bytes(), b"\"\\u003c\"");
        assert_eq!(
            html_safe("\"a<b>c&d\u{2028}e\u{2029}\"".to_string()),
            format!("\"a{}b{}c{}d{}e{}\"", esc('<'), esc('>'), esc('&'), esc('\u{2028}'), esc('\u{2029}'))
        );
        // Valid on any JSON text, keys included.
        let v = serde_json::json!({ "</script>": ["<!--", { "a&b": "x\u{2028}y>z" }], "n": 1.5 });
        let text = html_safe(serde_json::to_string(&v).unwrap());
        assert!(!text.contains(HTML_UNSAFE), "{text}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), v);
    }

    #[test]
    fn speaker_json_is_html_safe_and_carries_the_names_exactly() {
        for name in HOSTILE_NAMES {
            let list = [speaker(name, name), speaker("uuid:2", "Kitchen")];
            let body = to_json(&speaker_list(&list));
            assert!(!body.contains(HTML_UNSAFE), "{body}");
            assert!(!body.to_ascii_lowercase().contains("</script"), "{body}");
            let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(doc.as_object().unwrap().len(), 1, "{body}");
            let rows = doc["speakers"].as_array().unwrap();
            assert_eq!(rows.len(), 2, "{body}");
            assert_eq!(rows[0]["id"], *name);
            assert_eq!(rows[0]["name"], *name);
            assert_eq!(rows[1]["name"], "Kitchen");
            assert!(rows[0].get("note").is_none() && rows[0].get("hidden").is_none());
        }
    }

    #[test]
    fn json_bodies_keep_their_field_names_and_order() {
        let list = [
            speaker("uuid:RINCON_000E58A0123401400", "Living Room + Kitchen"),
            SpeakerInfo { manual: true, active: false, ..speaker("manual:192.168.1.77:7000", "Den \"quoted\" back\\slash\ttab") },
        ];
        assert_eq!(
            to_json(&speaker_list(&list)),
            concat!(
                r#"{"speakers":[{"id":"uuid:RINCON_000E58A0123401400","name":"Living Room + Kitchen","#,
                r#""ip":"192.168.1.50","active":true,"manual":false},"#,
                r#"{"id":"manual:192.168.1.77:7000","name":"Den \"quoted\" back\\slash\ttab","#,
                r#""ip":"192.168.1.50","active":false,"manual":true}]}"#,
            )
        );
        assert_eq!(to_json(&speaker_list(&[])), r#"{"speakers":[]}"#);
        assert_eq!(
            to_json(&SpeakerList { speakers: Vec::new(), error: Some("discovery disabled") }),
            r#"{"speakers":[],"error":"discovery disabled"}"#
        );
        assert_eq!(to_json(&ApiOk { ok: true }), r#"{"ok":true}"#);
        assert_eq!(to_json(&LatencyOk { ok: true, pending_frames: -441 }), r#"{"ok":true,"pending_frames":-441}"#);
        assert_eq!(
            to_json(&ApiError { error: "Still connecting to <b>Den</b> — try again" }),
            format!("{{\"error\":\"Still connecting to {0}b{1}Den{0}/b{1} — try again\"}}", esc('<'), esc('>'))
        );
    }

    /// Random strings over an alphabet of the characters that matter: every
    /// error body parses back to exactly the text it carries.
    #[test]
    fn json_error_bodies_round_trip_any_text() {
        const ALPHABET: &[char] = &[
            '<', '>', '&', '"', '\\', '/', '\'', '!', '-', 's', 'c', 'r', 'i', 'p', 't', 'S',
            ' ', '\n', '\r', '\t', '\u{0}', '\u{8}', '\u{c}', '\u{1f}', '\u{7f}', '\u{e9}',
            '\u{2028}', '\u{2029}', '\u{feff}', '\u{1F50A}', 'u', '0',
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 24) as usize;
            let s: String =
                (0..len).map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize]).collect();
            let body = to_json(&ApiError { error: &s });
            assert!(!body.contains(HTML_UNSAFE), "{body}");
            let back: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(back["error"], s);
        }
    }

    #[test]
    fn speakers_api_escapes_names_from_the_network() {
        let srv = api_server();
        let r = get(srv.port, "/api/speakers", &auth());
        let (head, body) = r.split_once("\r\n\r\n").expect("a complete reply");
        assert!(head.starts_with("HTTP/1.1 200"), "{r}");
        assert!(head.contains("Content-Type: application/json; charset=utf-8"), "{head}");
        assert!(head.contains("X-Content-Type-Options: nosniff"), "{head}");
        assert!(!body.contains(HTML_UNSAFE), "{body}");
        let doc: serde_json::Value = serde_json::from_str(body).unwrap();
        let row = &doc["speakers"][0];
        assert_eq!(row["id"], "uuid:x');alert(1);//\"><script>alert(2)</script>");
        assert_eq!(row["name"], "<img src=x onerror=alert(3)>");
        assert_eq!(row["active"], true);
    }
}
