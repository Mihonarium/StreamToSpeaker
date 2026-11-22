//! A scripted AirPlay 1 (RAOP) receiver on loopback, for tests.
//!
//! It accepts any number of connections and serves each on its own
//! thread, one request at a time in the order they arrive, the way a
//! receiver's RTSP worker does. For every request it asks the test's
//! script what to do, and it records everything it sees: each connection
//! opened, each request with its exact bytes, and each connection the
//! client closed. Three UDP sockets stand in for the receiver's audio,
//! control and timing ports, so a session can stream into it.
//!
//! ```ignore
//! // Never answer SETUP; answer everything else as a working receiver.
//! let rx = FakeRaop::start(|req| match req.method.as_str() {
//!     "SETUP" => Answer::Ignore,
//!     _ => Answer::Normal,
//! });
//! ```

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often blocked reads and accepts look at the stop flag.
const POLL: Duration = Duration::from_millis(20);

/// One request as the receiver read it.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    /// Which connection it came on: 0 for the first one accepted.
    pub conn: usize,
    pub method: String,
    pub uri: String,
    /// Header names as sent, values trimmed, in order.
    pub headers: Vec<(String, String)>,
    /// The request line and headers exactly as received, up to and
    /// including the blank line.
    pub head: String,
    pub body: Vec<u8>,
    pub at: Instant,
}

impl Request {
    /// A header's value (the name compared without regard to case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn cseq(&self) -> Option<u32> {
        self.header("CSeq").and_then(|v| v.parse().ok())
    }

    /// `"METHOD uri"`.
    pub fn line(&self) -> String {
        format!("{} {}", self.method, self.uri)
    }
}

/// Something the receiver saw.
#[derive(Clone, Debug)]
pub(crate) enum Event {
    Connected { conn: usize, at: Instant },
    Request(Request),
    /// The client closed or reset the connection (the receiver read end
    /// of stream or an error).
    Closed { conn: usize, at: Instant },
}

/// Which `CSeq` a reply carries.
#[derive(Clone, Debug)]
pub(crate) enum CSeqField {
    /// The request's own.
    Echo,
    /// None at all.
    Omit,
    /// This one.
    Value(u32),
}

/// One RTSP reply.
#[derive(Clone, Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub cseq: CSeqField,
}

impl Reply {
    pub fn ok() -> Self {
        Self::status(200, "OK")
    }

    pub fn status(code: u16, reason: &str) -> Self {
        Self { status: code, reason: reason.to_string(), headers: Vec::new(), body: Vec::new(), cseq: CSeqField::Echo }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }

    pub fn cseq(mut self, cseq: CSeqField) -> Self {
        self.cseq = cseq;
        self
    }

    /// The reply's bytes, answering `req`.
    pub fn bytes_for(&self, req: &Request) -> Vec<u8> {
        let mut head = format!("RTSP/1.0 {} {}\r\n", self.status, self.reason);
        let cseq = match self.cseq {
            CSeqField::Echo => req.cseq(),
            CSeqField::Omit => None,
            CSeqField::Value(v) => Some(v),
        };
        if let Some(c) = cseq {
            head.push_str(&format!("CSeq: {}\r\n", c));
        }
        for (k, v) in &self.headers {
            head.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !self.body.is_empty() {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// What the receiver does with one request.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    /// What a working receiver answers (see [`FakeRaop::normal_reply`]).
    Normal,
    /// This reply.
    Reply(Reply),
    /// The inner answer, after a pause during which this connection reads
    /// nothing else.
    Late(Duration, Box<Answer>),
    /// The inner answer's reply, written in pieces: cut at these byte
    /// offsets, with a pause between pieces.
    InPieces { answer: Box<Answer>, cuts: Vec<usize>, gap: Duration },
    /// These bytes, whatever they are.
    Raw(Vec<u8>),
    /// No answer; the connection goes on serving the next request.
    Ignore,
    /// Close the connection without answering.
    Close,
    /// Reset the connection without answering.
    Reset,
}

impl Answer {
    pub fn late(delay: Duration, answer: Answer) -> Self {
        Answer::Late(delay, Box::new(answer))
    }

    pub fn in_pieces(answer: Answer, cuts: &[usize], gap: Duration) -> Self {
        Answer::InPieces { answer: Box::new(answer), cuts: cuts.to_vec(), gap }
    }
}

type Script = Box<dyn FnMut(&Request) -> Answer + Send>;

struct Shared {
    events: Mutex<Vec<Event>>,
    changed: Condvar,
    script: Mutex<Script>,
    stop: AtomicBool,
    ports: UdpPorts,
}

impl Shared {
    fn push(&self, e: Event) {
        self.events.lock().unwrap().push(e);
        self.changed.notify_all();
    }

    /// Sleep `d`, or less if the receiver is being dropped.
    fn pause(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= end {
                return;
            }
            std::thread::sleep((end - now).min(POLL));
        }
    }
}

/// The receiver's UDP ports, as SETUP's normal answer gives them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UdpPorts {
    pub audio: u16,
    pub control: u16,
    pub timing: u16,
}

/// A scripted receiver listening on `127.0.0.1`. Dropping it stops every
/// thread it started.
pub(crate) struct FakeRaop {
    addr: SocketAddr,
    shared: Arc<Shared>,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    accept: Option<JoinHandle<()>>,
    udp: Vec<JoinHandle<()>>,
    /// Datagrams received on the audio, control and timing ports.
    datagrams: Arc<[AtomicU64; 3]>,
}

impl FakeRaop {
    /// Start a receiver that answers each request as `script` says.
    pub fn start(script: impl FnMut(&Request) -> Answer + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake receiver");
        listener.set_nonblocking(true).expect("nonblocking listener");
        let addr = listener.local_addr().expect("listener address");

        let sockets: Vec<UdpSocket> = (0..3).map(|_| UdpSocket::bind("127.0.0.1:0").expect("bind UDP")).collect();
        let port_of = |s: &UdpSocket| s.local_addr().expect("UDP address").port();
        let ports = UdpPorts { audio: port_of(&sockets[0]), control: port_of(&sockets[1]), timing: port_of(&sockets[2]) };

        let shared = Arc::new(Shared {
            events: Mutex::new(Vec::new()),
            changed: Condvar::new(),
            script: Mutex::new(Box::new(script)),
            stop: AtomicBool::new(false),
            ports,
        });
        let datagrams: Arc<[AtomicU64; 3]> = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
        // One thread drains all three UDP ports.
        let udp = {
            let shared = shared.clone();
            let datagrams = datagrams.clone();
            vec![std::thread::spawn(move || {
                for sock in &sockets {
                    sock.set_nonblocking(true).ok();
                }
                let mut buf = [0u8; 2048];
                while !shared.stop.load(Ordering::Acquire) {
                    let mut idle = true;
                    for (i, sock) in sockets.iter().enumerate() {
                        while sock.recv_from(&mut buf).is_ok() {
                            datagrams[i].fetch_add(1, Ordering::Relaxed);
                            idle = false;
                        }
                    }
                    if idle {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })]
        };

        let threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let shared = shared.clone();
            let threads = threads.clone();
            std::thread::spawn(move || {
                let mut next = 0usize;
                while !shared.stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((sock, _)) => {
                            let conn = next;
                            next += 1;
                            shared.push(Event::Connected { conn, at: Instant::now() });
                            let worker = shared.clone();
                            // Out of threads (the machine's limit): the
                            // connection is dropped, as a receiver that can't
                            // serve it would, and the test's output says so.
                            match std::thread::Builder::new().spawn(move || serve(conn, sock, &worker)) {
                                Ok(h) => threads.lock().unwrap_or_else(|e| e.into_inner()).push(h),
                                Err(e) => eprintln!("fake receiver: no thread to serve connection {} ({}); dropped it", conn, e),
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL),
                        Err(_) => std::thread::sleep(POLL),
                    }
                }
            })
        };
        Self { addr, shared, threads, accept: Some(accept), udp, datagrams }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn udp_ports(&self) -> UdpPorts {
        self.shared.ports
    }

    /// Datagrams received so far on the audio, control and timing ports.
    pub fn datagrams(&self) -> [u64; 3] {
        [0, 1, 2].map(|i| self.datagrams[i].load(Ordering::Relaxed))
    }

    pub fn events(&self) -> Vec<Event> {
        self.shared.events.lock().unwrap().clone()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Request(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// Every request as `"<conn> METHOD uri"`, in the order received.
    pub fn lines(&self) -> Vec<String> {
        self.requests().iter().map(|r| format!("{} {}", r.conn, r.line())).collect()
    }

    /// When connection `conn` was accepted, if it has been.
    pub fn connected_at(&self, conn: usize) -> Option<Instant> {
        self.events().into_iter().find_map(|e| match e {
            Event::Connected { conn: c, at } if c == conn => Some(at),
            _ => None,
        })
    }

    /// When the client closed connection `conn`, if it has.
    pub fn closed_at(&self, conn: usize) -> Option<Instant> {
        self.events().into_iter().find_map(|e| match e {
            Event::Closed { conn: c, at } if c == conn => Some(at),
            _ => None,
        })
    }

    /// Wait until `done` holds for the events so far, at most `timeout`.
    /// True if it does.
    pub fn wait_until(&self, timeout: Duration, done: impl Fn(&[Event]) -> bool) -> bool {
        let end = Instant::now() + timeout;
        let mut events = self.shared.events.lock().unwrap();
        loop {
            if done(&events) {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            events = self.shared.changed.wait_timeout(events, end - now).unwrap().0;
        }
    }

    /// Wait until `n` requests have arrived, at most `timeout`.
    pub fn wait_for_requests(&self, n: usize, timeout: Duration) -> bool {
        self.wait_until(timeout, |ev| ev.iter().filter(|e| matches!(e, Event::Request(_))).count() >= n)
    }

    /// What a working receiver answers to `req`, given its UDP `ports`.
    pub fn normal_reply(req: &Request, ports: UdpPorts) -> Reply {
        match (req.method.as_str(), req.uri.as_str()) {
            ("OPTIONS", _) => Reply::ok().header(
                "Public",
                "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER, POST",
            ),
            ("POST", "/auth-setup") => {
                // The receiver's X25519 key, then an (empty) certificate
                // and signature.
                let mut body = vec![9u8; 32];
                body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
                Reply::ok().header("Content-Type", "application/octet-stream").body(&body)
            }
            ("SETUP", _) => Reply::ok()
                .header(
                    "Transport",
                    &format!(
                        "RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}",
                        ports.audio, ports.control, ports.timing
                    ),
                )
                .header("Session", "1")
                .header("Audio-Jack-Status", "connected; type=analog"),
            ("ANNOUNCE" | "RECORD" | "SET_PARAMETER" | "GET_PARAMETER" | "FLUSH" | "TEARDOWN", _) => Reply::ok(),
            _ => Reply::status(501, "Not Implemented"),
        }
    }
}

impl Drop for FakeRaop {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(a) = self.accept.take() {
            let _ = a.join();
        }
        let threads: Vec<_> = self.threads.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        for t in self.udp.drain(..) {
            let _ = t.join();
        }
    }
}

/// Serve one connection until the client closes it, the script ends it,
/// or the receiver is dropped.
fn serve(conn: usize, mut sock: TcpStream, shared: &Shared) {
    sock.set_read_timeout(Some(POLL)).ok();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let req = match next_request(conn, &mut sock, &mut buf, shared) {
            Some(r) => r,
            None => {
                shared.push(Event::Closed { conn, at: Instant::now() });
                return;
            }
        };
        shared.push(Event::Request(req.clone()));
        let answer = {
            let mut script = shared.script.lock().unwrap();
            (*script)(&req)
        };
        if !act(&answer, &req, &mut sock, shared) {
            return;
        }
    }
}

/// Carry out `answer`. False once the connection is gone.
fn act(answer: &Answer, req: &Request, sock: &mut TcpStream, shared: &Shared) -> bool {
    match answer {
        Answer::Normal => write(sock, &FakeRaop::normal_reply(req, shared.ports).bytes_for(req)),
        Answer::Reply(r) => write(sock, &r.bytes_for(req)),
        Answer::Late(d, inner) => {
            shared.pause(*d);
            !shared.stop.load(Ordering::Acquire) && act(inner, req, sock, shared)
        }
        Answer::InPieces { answer, cuts, gap } => {
            let bytes = match answer.as_ref() {
                Answer::Normal => FakeRaop::normal_reply(req, shared.ports).bytes_for(req),
                Answer::Reply(r) => r.bytes_for(req),
                Answer::Raw(b) => b.clone(),
                other => panic!("InPieces needs a reply, not {:?}", other),
            };
            let mut from = 0;
            for &cut in cuts.iter().chain(std::iter::once(&bytes.len())) {
                let cut = cut.clamp(from, bytes.len());
                if from > 0 {
                    shared.pause(*gap);
                }
                if !write(sock, &bytes[from..cut]) {
                    return false;
                }
                from = cut;
            }
            true
        }
        Answer::Raw(b) => write(sock, b),
        Answer::Ignore => true,
        Answer::Close => {
            let _ = sock.shutdown(Shutdown::Both);
            false
        }
        Answer::Reset => {
            // A zero linger time makes the close a reset.
            let _ = socket2::SockRef::from(&*sock).set_linger(Some(Duration::ZERO));
            false
        }
    }
}

fn write(sock: &mut TcpStream, bytes: &[u8]) -> bool {
    sock.write_all(bytes).and_then(|_| sock.flush()).is_ok()
}

/// Read the next request on the connection. None once the client has
/// closed it (or the receiver is being dropped).
fn next_request(conn: usize, sock: &mut TcpStream, buf: &mut Vec<u8>, shared: &Shared) -> Option<Request> {
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..head_end + 4]).into_owned();
            let mut lines = head.split("\r\n");
            let first = lines.next().unwrap_or_default();
            let mut parts = first.split(' ');
            let method = parts.next().unwrap_or_default().to_string();
            let uri = parts.next().unwrap_or_default().to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.to_string(), v.trim().to_string()))
                .collect();
            let len = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + len {
                let body = buf[head_end + 4..head_end + 4 + len].to_vec();
                buf.drain(..head_end + 4 + len);
                return Some(Request { conn, method, uri, headers, head, body, at: Instant::now() });
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            return None;
        }
        match sock.read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test client: the connection, and what it has read past the last
    /// reply (two replies can arrive in one read).
    struct Client {
        s: TcpStream,
        pending: Vec<u8>,
    }

    impl std::ops::Deref for Client {
        type Target = TcpStream;
        fn deref(&self) -> &TcpStream {
            &self.s
        }
    }

    impl std::ops::DerefMut for Client {
        fn deref_mut(&mut self) -> &mut TcpStream {
            &mut self.s
        }
    }

    fn connect(rx: &FakeRaop) -> Client {
        let s = TcpStream::connect(("127.0.0.1", rx.port())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Client { s, pending: Vec::new() }
    }

    fn send(s: &mut TcpStream, method: &str, uri: &str, cseq: u32, body: &[u8]) {
        let mut head = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\n", method, uri, cseq);
        if !body.is_empty() {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
    }

    /// Read until a whole reply (head and body) has arrived, keeping any
    /// bytes after it for the next call; also return how many reads it
    /// took.
    fn read_reply(c: &mut Client) -> (String, usize) {
        let mut tmp = [0u8; 4096];
        let mut reads = 0;
        loop {
            let buf = &c.pending;
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .map(|v| v.parse::<usize>().unwrap())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    let reply = String::from_utf8_lossy(&buf[..end + 4 + len]).to_string();
                    c.pending.drain(..end + 4 + len);
                    return (reply, reads);
                }
            }
            let n = c.s.read(&mut tmp).unwrap_or_else(|e| panic!("reading the reply: {e:?}"));
            assert!(n > 0, "connection closed mid-reply");
            reads += 1;
            c.pending.extend_from_slice(&tmp[..n]);
        }
    }

    #[test]
    fn records_every_request_on_every_connection_in_order() {
        let rx = FakeRaop::start(|_| Answer::Normal);
        let mut a = connect(&rx);
        send(&mut a, "OPTIONS", "*", 1, b"");
        read_reply(&mut a);
        let mut b = connect(&rx);
        send(&mut b, "TEARDOWN", "rtsp://x/1", 9, b"");
        read_reply(&mut b);
        send(&mut a, "ANNOUNCE", "rtsp://x/1", 2, b"v=0\r\n");
        read_reply(&mut a);
        assert!(rx.wait_for_requests(3, Duration::from_secs(10)));
        assert_eq!(rx.lines(), ["0 OPTIONS *", "1 TEARDOWN rtsp://x/1", "0 ANNOUNCE rtsp://x/1"]);
        let announce = &rx.requests()[2];
        assert_eq!(announce.body, b"v=0\r\n");
        assert_eq!(announce.cseq(), Some(2));
        assert_eq!(announce.head, "ANNOUNCE rtsp://x/1 RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\n");
        drop(a);
        assert!(rx.wait_until(Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Event::Closed { conn: 0, .. }))));
        assert!(rx.closed_at(1).is_none());
        assert!(rx.connected_at(1).is_some() && rx.connected_at(2).is_none());
    }

    #[test]
    fn answers_with_the_scripted_status_and_echoes_cseq() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "ANNOUNCE" => Answer::Reply(Reply::status(453, "Not Enough Bandwidth")),
            _ => Answer::Normal,
        });
        let mut s = connect(&rx);
        send(&mut s, "ANNOUNCE", "rtsp://x/1", 7, b"");
        let (reply, _) = read_reply(&mut s);
        assert_eq!(reply, "RTSP/1.0 453 Not Enough Bandwidth\r\nCSeq: 7\r\n\r\n");
        send(&mut s, "SETUP", "rtsp://x/1", 8, b"");
        let (reply, _) = read_reply(&mut s);
        let p = rx.udp_ports();
        assert!(reply.contains(&format!("server_port={};control_port={};timing_port={}", p.audio, p.control, p.timing)));
        assert!(reply.contains("CSeq: 8\r\n"));
    }

    #[test]
    fn an_ignored_request_gets_no_answer_and_the_next_one_is_still_served() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::Ignore,
            _ => Answer::Normal,
        });
        let mut s = connect(&rx);
        send(&mut s, "SETUP", "rtsp://x/1", 1, b"");
        send(&mut s, "TEARDOWN", "rtsp://x/1", 2, b"");
        let (reply, _) = read_reply(&mut s);
        assert!(reply.starts_with("RTSP/1.0 200 OK\r\nCSeq: 2\r\n"), "{reply}");
        assert_eq!(rx.lines(), ["0 SETUP rtsp://x/1", "0 TEARDOWN rtsp://x/1"]);
    }

    #[test]
    fn a_late_answer_waits_and_holds_up_the_connection() {
        let delay = Duration::from_millis(300);
        let rx = FakeRaop::start(move |req| match req.method.as_str() {
            "SETUP" => Answer::late(delay, Answer::Normal),
            _ => Answer::Normal,
        });
        let mut s = connect(&rx);
        let sent = Instant::now();
        send(&mut s, "SETUP", "rtsp://x/1", 1, b"");
        send(&mut s, "RECORD", "rtsp://x/1", 2, b"");
        let (first, _) = read_reply(&mut s);
        assert!(sent.elapsed() >= delay);
        assert!(first.contains("CSeq: 1\r\n"));
        let (second, _) = read_reply(&mut s);
        assert!(second.contains("CSeq: 2\r\n"));
        let reqs = rx.requests();
        // RECORD was read only once SETUP had been answered.
        assert!(reqs[1].at.duration_since(reqs[0].at) >= delay);
    }

    #[test]
    fn a_reply_in_pieces_arrives_whole_over_several_reads() {
        let rx = FakeRaop::start(|_| {
            Answer::in_pieces(Answer::Reply(Reply::ok().body(b"0123456789")), &[5, 20, 30], Duration::from_millis(50))
        });
        let mut s = connect(&rx);
        send(&mut s, "GET_PARAMETER", "rtsp://x/1", 3, b"");
        let (reply, reads) = read_reply(&mut s);
        assert_eq!(reply, "RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 10\r\n\r\n0123456789");
        assert!(reads >= 2, "{reads}");
    }

    #[test]
    fn a_reply_without_cseq_or_with_another_one() {
        let rx = FakeRaop::start(|req| match req.cseq() {
            Some(1) => Answer::Reply(Reply::ok().cseq(CSeqField::Omit)),
            _ => Answer::Reply(Reply::ok().cseq(CSeqField::Value(77))),
        });
        let mut s = connect(&rx);
        send(&mut s, "OPTIONS", "*", 1, b"");
        assert_eq!(read_reply(&mut s).0, "RTSP/1.0 200 OK\r\n\r\n");
        send(&mut s, "OPTIONS", "*", 2, b"");
        assert_eq!(read_reply(&mut s).0, "RTSP/1.0 200 OK\r\nCSeq: 77\r\n\r\n");
    }

    #[test]
    fn close_ends_the_connection_and_reset_resets_it() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::Close,
            _ => Answer::Reset,
        });
        let mut s = connect(&rx);
        send(&mut s, "SETUP", "rtsp://x/1", 1, b"");
        let mut tmp = [0u8; 16];
        assert_eq!(s.read(&mut tmp).unwrap(), 0, "a closed connection reads end of stream");

        let mut s = connect(&rx);
        send(&mut s, "RECORD", "rtsp://x/1", 1, b"");
        let err = s.read(&mut tmp).expect_err("a reset connection reads an error");
        assert_eq!(err.kind(), ErrorKind::ConnectionReset);
    }

    #[test]
    fn raw_bytes_go_out_as_given() {
        let rx = FakeRaop::start(|_| Answer::Raw(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 9\r\n\r\nabc".to_vec()));
        let mut s = connect(&rx);
        send(&mut s, "OPTIONS", "*", 1, b"");
        let mut buf = vec![0u8; 256];
        let mut got = Vec::new();
        while !got.ends_with(b"abc") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 9\r\n\r\nabc");
    }

    #[test]
    fn counts_datagrams_on_its_udp_ports() {
        let rx = FakeRaop::start(|_| Answer::Normal);
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let p = rx.udp_ports();
        s.send_to(b"a", ("127.0.0.1", p.audio)).unwrap();
        s.send_to(b"t", ("127.0.0.1", p.timing)).unwrap();
        s.send_to(b"t", ("127.0.0.1", p.timing)).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while rx.datagrams() != [1, 0, 2] && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rx.datagrams(), [1, 0, 2]);
    }
}
