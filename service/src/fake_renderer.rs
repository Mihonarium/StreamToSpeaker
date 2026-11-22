//! A scripted stand-in for a UPnP renderer, for tests: an HTTP peer on
//! loopback that answers each request with whatever the test's script
//! returns and records every request it was sent.
//!
//! ```ignore
//! let speaker = FakeRenderer::start(|req| match req.soap_action() {
//!     Some("SetAVTransportURI") => Reply::soap_fault(714, "Illegal MIME-type"),
//!     _ => Reply::soap_ok(AV_TRANSPORT, req.soap_action().unwrap_or(""), &[]),
//! });
//! let control_url = speaker.url("/AVTransport/Control");
//! // ... exercise the code under test against `control_url` ...
//! assert_eq!(speaker.soap_actions(), ["Stop", "SetAVTransportURI"]);
//! ```
//!
//! Each connection carries one request (the app's clients all send
//! `Connection: close`). The script runs on the connection's own thread,
//! so it may block, or make requests of its own (a renderer probing the
//! stream URL it was given). A reply can be delayed, sent in parts, never
//! sent, or followed by a connection that stays open: the shapes of slow
//! and misbehaving devices. Everything the fake holds open is released
//! when it is dropped.

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Service types for [`Reply::soap_ok`].
pub(crate) const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub(crate) const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Largest request the fake reads; the app's are a few KB.
const MAX_REQUEST: usize = 1 << 20;

/// One request as the fake received it.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    pub target: String,
    /// In the order received, names as sent.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Everything that arrived, head and body, as it came.
    pub raw: Vec<u8>,
}

impl Request {
    /// The first header called `name`, compared without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The action a SOAP request invokes: `Play` for
    /// `SOAPAction: "urn:schemas-upnp-org:service:AVTransport:1#Play"`.
    pub fn soap_action(&self) -> Option<&str> {
        let v = self.header("soapaction")?.trim().trim_matches('"');
        v.rsplit_once('#').map(|(_, action)| action)
    }

    /// The text of a SOAP request's argument `name` (the first element of
    /// that local name in the body), as an XML parser reads it: a
    /// `CurrentURIMetaData` argument comes back as the DIDL document.
    pub fn soap_arg(&self, name: &str) -> Option<String> {
        let text = self.body_text();
        let doc = roxmltree::Document::parse(&text).ok()?;
        let node = doc.descendants().find(|n| n.tag_name().name() == name)?;
        Some(node.text().unwrap_or("").to_string())
    }
}

/// What the fake does with a request: bytes to send, each part after a
/// pause, and whether the connection is then closed or held open.
#[derive(Clone, Debug)]
pub(crate) struct Reply {
    parts: Vec<(Duration, Vec<u8>)>,
    hold_open: bool,
}

impl Reply {
    /// Exactly these bytes, then close.
    pub fn raw(bytes: impl Into<Vec<u8>>) -> Self {
        Self { parts: vec![(Duration::ZERO, bytes.into())], hold_open: false }
    }

    /// Nothing at all: the request is read and never answered. The
    /// connection stays open until the fake is dropped.
    pub fn silent() -> Self {
        Self { parts: Vec::new(), hold_open: true }
    }

    /// An HTTP/1.1 response with these headers, a `Content-Length` for
    /// `body`, and `body`.
    pub fn status(code: u16, reason: &str, headers: &[(&str, &str)], body: &str) -> Self {
        let mut head = format!("HTTP/1.1 {code} {reason}\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        Self::raw(format!("{head}{body}"))
    }

    /// `200 OK` with an XML body, as a device answers a description fetch
    /// or a SOAP action.
    pub fn ok(body: &str) -> Self {
        Self::status(200, "OK", &[("Content-Type", "text/xml; charset=\"utf-8\"")], body)
    }

    /// A successful SOAP answer to `action` of `service`, carrying the
    /// output arguments given (values are escaped here).
    pub fn soap_ok(service: &str, action: &str, out: &[(&str, &str)]) -> Self {
        let args: String = out.iter().map(|(k, v)| format!("<{k}>{}</{k}>", escape(v))).collect();
        Self::ok(&envelope(&format!("<u:{action}Response xmlns:u=\"{service}\">{args}</u:{action}Response>")))
    }

    /// The UPnP error answer for a refused action: `500` with a SOAP
    /// fault carrying `code` and `description` (escaped here).
    pub fn soap_fault(code: u32, description: &str) -> Self {
        let fault = format!(
            "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
             <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
             <errorDescription>{}</errorDescription></UPnPError></detail></s:Fault>",
            escape(description)
        );
        Self::status(500, "Internal Server Error", &[("Content-Type", "text/xml; charset=\"utf-8\"")], &envelope(&fault))
    }

    /// Wait `wait` before sending anything.
    pub fn after(mut self, wait: Duration) -> Self {
        match self.parts.first_mut() {
            Some(first) => first.0 += wait,
            None => self.parts.push((wait, Vec::new())),
        }
        self
    }

    /// Then, after `wait`, send `bytes` too.
    pub fn then(mut self, wait: Duration, bytes: impl Into<Vec<u8>>) -> Self {
        self.parts.push((wait, bytes.into()));
        self
    }

    /// Keep the connection open after the last byte, as a device that
    /// ignores `Connection: close` does, until the fake is dropped.
    pub fn then_hold_open(mut self) -> Self {
        self.hold_open = true;
        self
    }
}

/// A MediaRenderer device description named `name`, its services on the
/// describing host at `/AVTransport/...`, `/RenderingControl/...` and,
/// with `connection_manager`, `/ConnectionManager/...`.
pub(crate) fn renderer_description(name: &str, connection_manager: bool) -> String {
    let service = |kind: &str| {
        format!(
            "<service><serviceType>urn:schemas-upnp-org:service:{kind}:1</serviceType>\
             <serviceId>urn:upnp-org:serviceId:{kind}</serviceId><SCPDURL>/{kind}/scpd.xml</SCPDURL>\
             <controlURL>/{kind}/Control</controlURL><eventSubURL>/{kind}/Event</eventSubURL></service>"
        )
    };
    let cm = if connection_manager { service("ConnectionManager") } else { String::new() };
    format!(
        "<?xml version=\"1.0\"?>\n<root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major>\
         <minor>0</minor></specVersion><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>\
         <friendlyName>{}</friendlyName><modelName>Fake</modelName><UDN>uuid:fake-{}</UDN><serviceList>{}{}{}\
         </serviceList></device></root>",
        escape(name),
        escape(&name.to_ascii_lowercase().replace(' ', "-")),
        service("AVTransport"),
        service("RenderingControl"),
        cm,
    )
}

fn envelope(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>{body}</s:Body></s:Envelope>"
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

type Script = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// The fake renderer: listening from [`FakeRenderer::start`] until dropped.
pub(crate) struct FakeRenderer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Request>>>,
    arrived: Receiver<Request>,
    stopped: Arc<AtomicBool>,
    /// Never sent on; dropping it wakes every connection that waits or
    /// holds on, so they close.
    _release: Sender<()>,
}

impl FakeRenderer {
    /// Listen on loopback and answer every request with `script`.
    pub fn start(script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fake renderer");
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (arrived_tx, arrived) = crossbeam_channel::unbounded();
        let (release, release_rx) = crossbeam_channel::bounded::<()>(0);
        let stopped = Arc::new(AtomicBool::new(false));
        let script: Script = Arc::new(script);
        {
            let seen = seen.clone();
            let stopped = stopped.clone();
            thread::spawn(move || {
                for conn in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok(conn) = conn else { continue };
                    let (seen, arrived_tx, release_rx, script) =
                        (seen.clone(), arrived_tx.clone(), release_rx.clone(), script.clone());
                    thread::spawn(move || serve(conn, &seen, &arrived_tx, &release_rx, &script));
                }
            });
        }
        Self { addr, seen, arrived, stopped, _release: release }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// The next request in arrival order not yet taken here, waiting up
    /// to `timeout` for one.
    pub fn next_request(&self, timeout: Duration) -> Option<Request> {
        self.arrived.recv_timeout(timeout).ok()
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// The SOAP actions received so far, in arrival order.
    pub fn soap_actions(&self) -> Vec<String> {
        self.requests().iter().filter_map(|r| r.soap_action().map(str::to_string)).collect()
    }
}

impl Drop for FakeRenderer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(
    mut conn: TcpStream,
    seen: &Mutex<Vec<Request>>,
    arrived: &Sender<Request>,
    release: &Receiver<()>,
    script: &Script,
) {
    let Some(req) = read_request(&mut conn) else { return };
    seen.lock().unwrap().push(req.clone());
    let _ = arrived.send(req.clone());
    let reply = script(&req);
    for (wait, bytes) in &reply.parts {
        // A pause ends early, and the connection closes, when the fake
        // is dropped.
        if !wait.is_zero() && release.recv_timeout(*wait) != Err(RecvTimeoutError::Timeout) {
            return;
        }
        if conn.write_all(bytes).and_then(|_| conn.flush()).is_err() {
            return;
        }
    }
    if reply.hold_open {
        let _ = release.recv();
    }
}

/// One request: the head up to the blank line, then `Content-Length`
/// bytes of body. None if the client went away first.
fn read_request(conn: &mut TcpStream) -> Option<Request> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if raw.len() > MAX_REQUEST {
            return None;
        }
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while raw.len() < body_start + len.min(MAX_REQUEST) {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    let body = raw[body_start..].to_vec();
    Some(Request { method, target, headers, body, raw })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(5);

    fn send(url_addr: SocketAddr, request: &str) -> TcpStream {
        let mut s = TcpStream::connect(url_addr).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        s
    }

    /// Read until the peer closes, or until `n` bytes have come.
    fn read_some(s: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while out.len() < n {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(k) => out.extend_from_slice(&buf[..k]),
            }
        }
        out
    }

    /// Whether the peer has closed: a read returns EOF within `within`.
    fn closes_within(s: &mut TcpStream, within: Duration) -> bool {
        s.set_read_timeout(Some(within)).unwrap();
        let mut b = [0u8; 1];
        matches!(s.read(&mut b), Ok(0))
    }

    const PLAY: &str = "POST /AVTransport/Control HTTP/1.1\r\nHost: x\r\n\
        SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#Play\"\r\nContent-Length: 125\r\n\r\n\
        <s:Envelope xmlns:s=\"s\"><s:Body><u:Play xmlns:u=\"u\"><InstanceID>0</InstanceID>\
        <Speed>1</Speed></u:Play></s:Body></s:Envelope>";

    #[test]
    fn records_what_it_was_sent_and_answers_with_the_script() {
        assert_eq!(PLAY.split_once("\r\n\r\n").unwrap().1.len(), 125);
        let fake = FakeRenderer::start(|req| {
            Reply::raw(format!("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Saw: {}\r\n\r\n", req.target))
        });
        assert_eq!(fake.url("/AVTransport/Control"), format!("http://{}/AVTransport/Control", fake.addr()));
        let mut s = send(fake.addr(), PLAY);
        let reply = String::from_utf8(read_some(&mut s, usize::MAX)).unwrap();
        assert_eq!(reply, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Saw: /AVTransport/Control\r\n\r\n");

        let req = fake.next_request(WAIT).expect("a request");
        assert_eq!((req.method.as_str(), req.target.as_str()), ("POST", "/AVTransport/Control"));
        assert_eq!(req.header("soapaction"), Some("\"urn:schemas-upnp-org:service:AVTransport:1#Play\""));
        assert_eq!(req.soap_action(), Some("Play"));
        assert_eq!(req.soap_arg("Speed").as_deref(), Some("1"));
        assert_eq!(req.soap_arg("Missing"), None);
        assert_eq!(req.raw, PLAY.as_bytes());
        assert_eq!(fake.soap_actions(), ["Play"]);
        // Taken once: nothing more is waiting.
        assert!(fake.next_request(Duration::from_millis(50)).is_none());
        assert_eq!(fake.requests().len(), 1);
    }

    #[test]
    fn soap_replies_carry_their_arguments_and_fault_codes() {
        let ok = Reply::soap_ok(CONNECTION_MANAGER, "GetProtocolInfo", &[("Sink", "http-get:*:audio/wav:*"), ("Source", "")]);
        let ok = String::from_utf8(ok.parts[0].1.clone()).unwrap();
        let (head, body) = ok.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.ends_with(&format!("Content-Length: {}", body.len())), "{head}");
        let doc = roxmltree::Document::parse(body).unwrap();
        let text = |name: &str| doc.descendants().find(|n| n.tag_name().name() == name).and_then(|n| n.text());
        assert_eq!(text("Sink"), Some("http-get:*:audio/wav:*"));

        let play = Reply::soap_ok(AV_TRANSPORT, "Play", &[]);
        let play = String::from_utf8(play.parts[0].1.clone()).unwrap();
        assert!(play.contains("<u:PlayResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"></u:PlayResponse>"), "{play}");

        let fault = Reply::soap_fault(714, "Illegal <MIME>-type");
        let fault = String::from_utf8(fault.parts[0].1.clone()).unwrap();
        let (head, body) = fault.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 500 Internal Server Error\r\n"), "{head}");
        let doc = roxmltree::Document::parse(body).unwrap();
        let text = |name: &str| doc.descendants().find(|n| n.tag_name().name() == name).and_then(|n| n.text());
        assert_eq!(text("errorCode"), Some("714"));
        assert_eq!(text("errorDescription"), Some("Illegal <MIME>-type"));
    }

    #[test]
    fn a_reply_can_come_late_and_in_parts() {
        let fake = FakeRenderer::start(|_| {
            Reply::raw("HTTP/1.1 200 OK\r\n")
                .after(Duration::from_millis(300))
                .then(Duration::from_millis(300), "Content-Length: 0\r\n\r\n")
        });
        let start = Instant::now();
        let mut s = send(fake.addr(), PLAY);
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"HTTP/1.1 200 OK\r\n");
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert_eq!(read_some(&mut s, usize::MAX), b"Content-Length: 0\r\n\r\n");
        assert!(start.elapsed() >= Duration::from_millis(600));
    }

    #[test]
    fn a_held_connection_stays_open_until_the_fake_is_dropped() {
        let fake = FakeRenderer::start(|_| Reply::status(200, "OK", &[], "<done/>").then_hold_open());
        let mut s = send(fake.addr(), PLAY);
        let want = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n<done/>";
        assert_eq!(read_some(&mut s, want.len()), want);
        assert!(!closes_within(&mut s, Duration::from_millis(200)), "closed after answering");
        drop(fake);
        assert!(closes_within(&mut s, WAIT));
    }

    #[test]
    fn a_silent_fake_reads_the_request_and_never_answers() {
        let fake = FakeRenderer::start(|_| Reply::silent());
        let mut s = send(fake.addr(), PLAY);
        assert!(fake.next_request(WAIT).is_some(), "the request was read");
        assert!(!closes_within(&mut s, Duration::from_millis(200)));
        drop(fake);
        assert!(closes_within(&mut s, WAIT));
        assert_eq!(read_some(&mut s, 1), b"");
    }

    #[test]
    fn a_pending_delay_ends_when_the_fake_is_dropped() {
        let fake = FakeRenderer::start(|_| Reply::ok("<late/>").after(Duration::from_secs(600)));
        let mut s = send(fake.addr(), PLAY);
        assert!(fake.next_request(WAIT).is_some());
        drop(fake);
        assert!(closes_within(&mut s, WAIT), "the delayed reply kept the connection");
    }
}
