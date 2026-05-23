//! HTTP server.
//!
//! Two responsibilities:
//!   1. `GET /stream.raw` — endless chunked stream of L16 PCM.
//!   2. `NOTIFY ...` (also `POST` / `M-POST` if Sonos uses them) — GENA
//!      event callbacks from the speaker.
//!
//! The PCM stream is fed by a single producer thread (audio path) that
//! broadcasts each packet to all current subscribers via a fan-out.

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::{debug, info, warn};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::{WIRE_BYTES_PER_FRAME, WIRE_CHANNELS, WIRE_SAMPLE_RATE};

/// Callback invoked from the GENA NOTIFY route with the raw XML body.
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

/// One PCM packet to broadcast to subscribers. Pre-encoded as bytes so the
/// audio thread does the conversion once.
#[derive(Clone)]
pub struct PcmFrame(pub Arc<Vec<u8>>);

/// Hub used by the audio thread to push PCM and by HTTP workers to pull.
pub struct StreamHub {
    subscribers: Mutex<Vec<Sender<PcmFrame>>>,
}

impl StreamHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscribers: Mutex::new(Vec::new()),
        })
    }

    pub fn subscribe(&self) -> Receiver<PcmFrame> {
        // Bounded; if a subscriber stalls we drop frames rather than
        // back-pressure the audio path.
        let (tx, rx) = bounded::<PcmFrame>(128);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Publish a PCM frame to all subscribers, dropping any whose channel
    /// is closed or full.
    pub fn publish(&self, frame: PcmFrame) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| {
            match tx.try_send(frame.clone()) {
                Ok(()) => true,
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    // Drop the frame for this slow subscriber; they'll
                    // catch up.  We keep them subscribed.
                    true
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
            }
        });
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
        }
    }
}

/// Start the HTTP server on a background thread.  Returns the actual port
/// it bound to.
pub fn start_http_server(cfg: HttpServerConfig) -> Result<u16> {
    let bind = cfg.bind;
    let server = Server::http(bind)
        .map_err(|e| anyhow::anyhow!("tiny_http bind {} failed: {}", bind, e))?;
    let actual = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(bind.port());
    info!("HTTP server listening on {}", actual);

    let hub = cfg.hub.clone();
    let gena_cb = cfg.gena_callback.clone();
    let list_cb = cfg.speaker_list.clone();
    let select_cb = cfg.speaker_select.clone();
    let resync_cb = cfg.resync.clone();

    thread::Builder::new()
        .name("stream-to-speaker-http".to_string())
        .spawn(move || run_server(server, hub, gena_cb, list_cb, select_cb, resync_cb))
        .context("spawning HTTP server thread")?;

    Ok(actual)
}

fn run_server(
    server: Server,
    hub: Arc<StreamHub>,
    gena_callback: Option<GenaNotifyCallback>,
    speaker_list: Option<SpeakerListCallback>,
    speaker_select: Option<SpeakerSelectCallback>,
    resync: Option<ResyncCallback>,
) {
    for req in server.incoming_requests() {
        let url = req.url().to_string();
        let method = req.method().clone();
        debug!("http request: {} {}", method, url);

        match (method.clone(), url.as_str()) {
            (Method::Get, "/stream.raw") => {
                let hub = hub.clone();
                thread::Builder::new()
                    .name("stream-to-speaker-http-stream".to_string())
                    .spawn(move || {
                        if let Err(e) = serve_stream(req, hub) {
                            debug!("stream client ended: {}", e);
                        }
                    })
                    .ok();
            }
            (Method::Head, "/stream.raw") => {
                /* Sonos and other DLNA renderers HEAD-probe the URI before
                 * committing to GET. Reply with the same DLNA headers we
                 * would on GET, but no body. Without this Sonos gives up
                 * with "Couldn't connect" before ever trying the GET. */
                serve_stream_head(req);
            }
            (Method::Get, "/healthz") => {
                let _ = req.respond(
                    Response::from_string("ok").with_status_code(StatusCode(200)),
                );
            }
            (Method::Get, "/") => {
                serve_status_page(req, &speaker_list);
            }
            (Method::Get, "/api/speakers") => {
                serve_speakers_json(req, &speaker_list);
            }
            (Method::Post, "/api/select") => {
                serve_select(req, &speaker_select);
            }
            (Method::Post, "/api/resync") => {
                serve_resync(req, &resync);
            }
            (m, path) => {
                // Could be a GENA NOTIFY. tiny_http parses standard
                // methods; NOTIFY shows up as Method::NonStandard.
                let m_str = format!("{}", m);
                if m_str.eq_ignore_ascii_case("NOTIFY") {
                    handle_notify(req, path, &gena_callback);
                } else {
                    let _ = req.respond(
                        Response::from_string("not found")
                            .with_status_code(StatusCode(404)),
                    );
                }
            }
        }
    }
    warn!("HTTP server loop exited");
}

// -----------------------------------------------------------------------------
// Speaker management API
// -----------------------------------------------------------------------------

fn serve_status_page(req: tiny_http::Request, speakers: &Option<SpeakerListCallback>) {
    let mut rows = String::new();
    if let Some(cb) = speakers {
        let list = cb();
        if list.is_empty() {
            rows.push_str("<tr><td colspan=\"3\"><em>No speakers discovered yet.</em></td></tr>");
        } else {
            for s in list {
                let active_marker = if s.active { " &#9679;" } else { "" };
                rows.push_str(&format!(
                    "<tr><td>{}{}</td><td>{}</td><td><button onclick=\"select('{}')\">select</button></td></tr>",
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

    let html = format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<title>Stream To Speaker</title>
<style>
  body {{ font-family: -apple-system, system-ui, Segoe UI, sans-serif; max-width: 640px; margin: 2em auto; padding: 0 1em; }}
  h1 {{ margin-bottom: 0.2em; }}
  small {{ color: #666; }}
  table {{ border-collapse: collapse; width: 100%; margin-top: 1em; }}
  td, th {{ padding: 0.4em 0.6em; border-bottom: 1px solid #ddd; text-align: left; }}
  button {{ padding: 0.3em 0.8em; }}
  .actions {{ margin-top: 1.5em; }}
  .actions button {{ margin-right: 0.5em; }}
</style>
</head><body>
<h1>Stream To Speaker</h1>
<small>Active marker &#9679; shows the currently-targeted device.</small>
<table>
  <thead><tr><th>Speaker</th><th>IP</th><th></th></tr></thead>
  <tbody>{rows}</tbody>
</table>
<div class="actions">
  <button onclick="resync()" title="Force Sonos to flush its prebuffer (UPnP Stop + Play). Brief audio glitch, but trims accumulated latency.">resync (trim latency)</button>
</div>
<script>
function select(id) {{
  fetch('/api/select', {{
    method: 'POST',
    headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{id: id}})
  }}).then(r => location.reload());
}}
function resync() {{
  fetch('/api/resync', {{method: 'POST'}}).then(r => r.json()).then(j => {{
    if (j.error) {{ alert('resync failed: ' + j.error); }}
  }});
}}
setTimeout(() => location.reload(), 5000);
</script>
</body></html>"#,
        rows = rows
    );

    let mut resp = Response::from_string(html).with_status_code(StatusCode(200));
    resp.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
    );
    let _ = req.respond(resp);
}

fn serve_speakers_json(req: tiny_http::Request, speakers: &Option<SpeakerListCallback>) {
    let json = match speakers {
        Some(cb) => speakers_to_json(&cb()),
        None => "{\"speakers\": [], \"error\": \"discovery disabled\"}".to_string(),
    };
    let mut resp = Response::from_string(json).with_status_code(StatusCode(200));
    resp.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap(),
    );
    let _ = req.respond(resp);
}

fn serve_select(
    mut req: tiny_http::Request,
    select_cb: &Option<SpeakerSelectCallback>,
) {
    let mut body = String::new();
    if let Err(e) = req.as_reader().read_to_string(&mut body) {
        respond_json_owned(req, 400, &format!(r#"{{"error":"reading body: {}"}}"#, e));
        return;
    }
    let id = match extract_json_string(&body, "id") {
        Some(s) => s,
        None => {
            respond_json_owned(req, 400, r#"{"error":"missing 'id' field"}"#);
            return;
        }
    };
    let Some(cb) = select_cb else {
        respond_json_owned(req, 503, r#"{"error":"speaker switching unavailable"}"#);
        return;
    };
    match cb(&id) {
        Ok(()) => {
            respond_json_owned(req, 200, r#"{"ok":true}"#);
        }
        Err(msg) => {
            respond_json_owned(
                req,
                500,
                &format!(r#"{{"error":{}}}"#, json_string(&msg)),
            );
        }
    }
}

fn serve_resync(req: tiny_http::Request, resync_cb: &Option<ResyncCallback>) {
    let Some(cb) = resync_cb else {
        respond_json_owned(req, 503, r#"{"error":"resync unavailable"}"#);
        return;
    };
    match cb() {
        Ok(()) => respond_json_owned(req, 200, r#"{"ok":true}"#),
        Err(msg) => respond_json_owned(
            req,
            500,
            &format!(r#"{{"error":{}}}"#, json_string(&msg)),
        ),
    }
}

fn respond_json_owned(req: tiny_http::Request, status: u16, body: &str) {
    let mut resp = Response::from_string(body.to_string()).with_status_code(StatusCode(status));
    resp.add_header(
        Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap(),
    );
    let _ = req.respond(resp);
}

/// Minimal JSON serialiser for the speaker list — avoids pulling in serde
/// for ~30 lines of output.
fn speakers_to_json(list: &[SpeakerInfo]) -> String {
    let mut s = String::from(r#"{"speakers":["#);
    for (i, sp) in list.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"id":{},"name":{},"ip":{},"active":{}}}"#,
            json_string(&sp.id),
            json_string(&sp.friendly_name),
            json_string(&sp.ip),
            sp.active
        ));
    }
    s.push_str("]}");
    s
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
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
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
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

fn handle_notify(
    mut req: tiny_http::Request,
    path: &str,
    cb: &Option<GenaNotifyCallback>,
) {
    let mut body = String::new();
    if let Err(e) = req.as_reader().read_to_string(&mut body) {
        debug!("NOTIFY body read failed: {}", e);
    }
    if let Some(cb) = cb {
        cb(path, &body);
    } else {
        debug!("NOTIFY received but no callback registered; path={}", path);
    }
    let _ = req.respond(Response::from_string("").with_status_code(StatusCode(200)));
}

/// Serve a single `/stream.raw` connection.  We write chunked HTTP/1.1 by
/// hand so we have precise control over flush timing.
fn serve_stream(req: tiny_http::Request, hub: Arc<StreamHub>) -> Result<()> {
    // Subscribe BEFORE we send headers so we don't miss the first frame.
    let rx = hub.subscribe();

    // We need the raw TCP stream for hand-rolled chunked transfer.
    // tiny_http exposes the request via `respond` but using a Response
    // with chunked writer also works.  We use the Response::empty path
    // with a custom reader to keep things simple.
    //
    // Strategy: build a Response that holds a "reader" backed by our
    // crossbeam channel.  tiny_http will Transfer-Encoding: chunked it
    // for us.

    let reader = StreamReader::new(rx);

    // swyh-rs's StreamSize::U32maxNotChunked pattern — emit a fixed
    // (fake) Content-Length of u32::MAX-1 and set chunked_threshold to
    // u32::MAX so tiny_http NEVER triggers chunked transfer. Sonos
    // rejects WAV streams without Content-Length / with Transfer-Encoding,
    // even though both are technically legal HTTP.
    const FAKE_CONTENT_LEN: usize = (u32::MAX - 1) as usize;
    let mut response = Response::empty(StatusCode(200))
        .with_data(reader, Some(FAKE_CONTENT_LEN))
        .with_chunked_threshold(u32::MAX as usize);
    for h in dlna_stream_headers() {
        response.add_header(h);
    }

    req.respond(response).map_err(|e| anyhow::anyhow!("stream respond: {}", e))?;
    Ok(())
}

/// Reply to a HEAD on /stream.raw with the headers we'd send on GET but
/// no body. Sonos uses this as a probe.
fn serve_stream_head(req: tiny_http::Request) {
    let mut response = Response::empty(StatusCode(200)).with_data(std::io::empty(), Some(0));
    for h in dlna_stream_headers() {
        response.add_header(h);
    }
    let _ = req.respond(response);
}

/// Headers shared between GET and HEAD on /stream.raw. The
/// `TransferMode.dlna.org: Streaming` header is what swyh-rs always emits
/// on stream responses; many DLNA renderers (including Sonos) treat it as
/// a contract that the body is a live stream and won't seek. `Accept-Ranges:
/// none` discourages Range requests we don't currently honour.
///
/// Each `Header::from_bytes` returns Result; we log+skip on failure rather
/// than .unwrap() to avoid panicking the HTTP thread on the rare edge case
/// where tiny_http rejects a token character.
fn dlna_stream_headers() -> Vec<Header> {
    let mut out = Vec::with_capacity(4);
    let add = |out: &mut Vec<Header>, name: &[u8], val: &[u8]| {
        match Header::from_bytes(name, val) {
            Ok(h) => out.push(h),
            Err(_) => warn!(
                "dropping header {:?}={:?} (tiny_http rejected)",
                String::from_utf8_lossy(name),
                String::from_utf8_lossy(val)
            ),
        }
    };
    // Match swyh-rs's get_dlna_headers/get_std_headers output exactly.
    // Sonos's DLNA stack is pedantic. Do NOT add Accept-Ranges or
    // contentFeatures.dlna.org on the 200 response — swyh-rs omits both.
    add(&mut out, b"Server",                b"stream-to-speaker tiny-http");
    add(&mut out, b"Connection",            b"close");
    add(&mut out, b"Content-Type",          b"audio/vnd.wave;codec=1");
    add(&mut out, b"TransferMode.dlna.org", b"Streaming");
    out
}

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
}

impl StreamReader {
    fn new(rx: Receiver<PcmFrame>) -> Self {
        Self {
            rx,
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
