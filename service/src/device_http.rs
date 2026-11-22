//! The small HTTP client the app uses with devices on the LAN: UPnP
//! control (SOAP), event subscriptions (GENA) and device descriptions.
//!
//! An answer is complete when its own framing says so: at its
//! `Content-Length`, at the last chunk when `Transfer-Encoding` says
//! chunked, otherwise when the device closes the connection. So a device
//! that keeps the connection open after answering, in spite of the
//! `Connection: close` every request here carries, is not taken for one
//! that doesn't answer. Each exchange runs under one deadline, connecting
//! included, and the bytes taken from the device under one size bound.

use crate::discovery_net::shown;
use std::fmt;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// What the app calls itself to devices.
pub(crate) const USER_AGENT: &str = "stream-to-speaker/0.1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    Http10,
    Http11,
}

/// One request. `Host` is sent first, then `headers` in this order, so
/// what devices receive is exactly what the caller lists.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub version: Version,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// For opening the connection.
    pub connect: Duration,
    /// For the whole exchange, connecting included.
    pub deadline: Duration,
    /// Bytes accepted from the device, head and body together.
    pub max_bytes: usize,
}

impl Limits {
    /// These limits, with no more than `left` for connecting and for the
    /// whole exchange.
    pub fn within(&self, left: Duration) -> Limits {
        Limits { connect: self.connect.min(left), deadline: self.deadline.min(left), max_bytes: self.max_bytes }
    }
}

#[derive(Debug)]
pub(crate) struct Response {
    pub status: u16,
    /// The reason phrase, possibly empty.
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first header called `name`, compared without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `HTTP 500 "Internal Server Error"`, for messages: the reason phrase
    /// is the device's text, so it is quoted and bounded like any other.
    pub fn status_text(&self) -> String {
        if self.reason.is_empty() {
            format!("HTTP {}", self.status)
        } else {
            format!("HTTP {} {}", self.status, shown(&self.reason))
        }
    }
}

/// Why an exchange produced no answer to look at.
#[derive(Debug)]
pub(crate) enum HttpError {
    /// The address can't be used: unparseable, no host, or not `http`.
    BadUrl(String),
    /// No connection: refused, unreachable, or not accepted in time.
    Connect(std::io::Error),
    /// Connected, but no complete answer before the deadline.
    NoAnswer(Duration),
    /// The device closed or reset the connection before any byte of an
    /// answer.
    Closed,
    /// The connection failed part-way through the answer.
    CutOff(std::io::Error),
    /// What came back isn't an HTTP answer.
    Malformed(String),
    /// The answer is larger than allowed.
    TooLarge(usize),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::BadUrl(why) => write!(f, "unusable address: {why}"),
            HttpError::Connect(e) => write!(f, "couldn't connect: {e}"),
            HttpError::NoAnswer(d) => write!(f, "no answer within {} s", d.as_secs_f32()),
            HttpError::Closed => write!(f, "the device closed the connection without answering"),
            HttpError::CutOff(e) => write!(f, "the answer was cut off: {e}"),
            HttpError::Malformed(why) => write!(f, "not an HTTP answer: {why}"),
            HttpError::TooLarge(max) => write!(f, "answer larger than {} KB", max / 1024),
        }
    }
}

impl std::error::Error for HttpError {}

/// Send `req` and read the answer, within `limits`.
pub(crate) fn exchange(req: &Request<'_>, limits: &Limits) -> Result<Response, HttpError> {
    let deadline = Instant::now() + limits.deadline;
    let url = url::Url::parse(req.url).map_err(|e| HttpError::BadUrl(format!("{} ({e})", shown(req.url))))?;
    if url.scheme() != "http" {
        return Err(HttpError::BadUrl(format!("{} (only http:// is supported)", shown(req.url))));
    }
    let host = url.host_str().ok_or_else(|| HttpError::BadUrl(format!("{} has no host", shown(req.url))))?;
    let port = url.port().unwrap_or(80);
    let addr = url
        .socket_addrs(|| Some(80))
        .map_err(HttpError::Connect)?
        .into_iter()
        .next()
        .ok_or_else(|| HttpError::BadUrl(format!("{} resolves to no address", shown(host))))?;

    let mut target = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    let version = match req.version {
        Version::Http10 => "HTTP/1.0",
        Version::Http11 => "HTTP/1.1",
    };
    let mut head = format!("{} {} {}\r\nHost: {}:{}\r\n", req.method, target, version, host, port);
    for (k, v) in req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(req.body);

    let mut stream = TcpStream::connect_timeout(&addr, limits.connect.min(remaining(deadline)))
        .map_err(HttpError::Connect)?;
    let _ = stream.set_nodelay(true);
    // One write: head and body leave in one go.
    stream
        .set_write_timeout(Some(remaining(deadline)))
        .and_then(|_| stream.write_all(&bytes))
        .and_then(|_| stream.flush())
        .map_err(|e| io_failure(e, limits, false))?;

    // Every request here asks for the close; one that does is answered
    // knowing the device will end the connection after its answer.
    let closes = req.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"));
    let mut reader = Reader { stream, buf: Vec::new(), drained: 0, deadline, limits: *limits };
    read_response(&mut reader, req.method, closes)
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))
}

/// What a failed read or write means. `answered`: some of an answer had
/// arrived.
fn io_failure(e: std::io::Error, limits: &Limits, answered: bool) -> HttpError {
    use std::io::ErrorKind::*;
    match e.kind() {
        TimedOut | WouldBlock => HttpError::NoAnswer(limits.deadline),
        ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof if !answered => HttpError::Closed,
        _ => HttpError::CutOff(e),
    }
}

/// The connection's bytes so far, and how to get more.
struct Reader {
    stream: TcpStream,
    buf: Vec<u8>,
    /// Bytes of interim (1xx) answers already set aside, which count
    /// against the size bound too.
    drained: usize,
    deadline: Instant,
    limits: Limits,
}

impl Reader {
    /// Read whatever arrives next. Ok(false) once the device has closed.
    fn more(&mut self) -> Result<bool, HttpError> {
        let mut tmp = [0u8; 8192];
        loop {
            if Instant::now() >= self.deadline {
                return Err(HttpError::NoAnswer(self.limits.deadline));
            }
            self.stream
                .set_read_timeout(Some(remaining(self.deadline)))
                .map_err(HttpError::CutOff)?;
            match self.stream.read(&mut tmp) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    if self.drained + self.buf.len() + n > self.limits.max_bytes {
                        return Err(HttpError::TooLarge(self.limits.max_bytes));
                    }
                    self.buf.extend_from_slice(&tmp[..n]);
                    return Ok(true);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_failure(e, &self.limits, !self.buf.is_empty())),
            }
        }
    }
}

/// Where a head ends: after its blank line, CRLF or bare LF. The bytes
/// before `from` are known not to hold one, so the search resumes there.
fn head_end(buf: &[u8], from: usize) -> Option<usize> {
    let from = from.saturating_sub(3).min(buf.len());
    let crlf = buf[from..].windows(4).position(|w| w == b"\r\n\r\n").map(|p| from + p + 4);
    let lf = buf[from..].windows(2).position(|w| w == b"\n\n").map(|p| from + p + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

struct Head {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
}

fn parse_head(bytes: &[u8]) -> Result<Head, HttpError> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let status_line = lines.next().unwrap_or("");
    let malformed = || HttpError::Malformed(shown(status_line));
    let rest = status_line.strip_prefix("HTTP/").ok_or_else(malformed)?;
    let mut parts = rest.splitn(3, ' ');
    let _version = parts.next();
    let code = parts.next().unwrap_or("");
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    let status: u16 = code.parse().map_err(|_| malformed())?;
    let reason = parts.next().unwrap_or("").trim().to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(Head { status, reason, headers })
}

enum Framing {
    None,
    Length(usize),
    Chunked,
    ToClose,
}

fn framing(method: &str, head: &Head) -> Result<Framing, HttpError> {
    if method.eq_ignore_ascii_case("HEAD") || head.status == 204 || head.status == 304 {
        return Ok(Framing::None);
    }
    fn values<'h>(head: &'h Head, name: &'static str) -> impl Iterator<Item = &'h str> {
        head.headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    if let Some(te) = values(head, "transfer-encoding").last() {
        let last = te.rsplit(',').next().unwrap_or("").trim();
        return Ok(if last.eq_ignore_ascii_case("chunked") { Framing::Chunked } else { Framing::ToClose });
    }
    let mut length = None;
    for v in values(head, "content-length") {
        let v = v.trim();
        let n = (!v.is_empty() && v.len() <= 19 && v.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.parse::<usize>().ok())
            .flatten()
            .ok_or_else(|| HttpError::Malformed(format!("Content-Length {}", shown(v))))?;
        if length.is_some_and(|l| l != n) {
            return Err(HttpError::Malformed("two different Content-Length values".into()));
        }
        length = Some(n);
    }
    Ok(length.map_or(Framing::ToClose, Framing::Length))
}

/// Read the answer to `method`. `closes`: the request asked the device to
/// close the connection after answering.
fn read_response(r: &mut Reader, method: &str, closes: bool) -> Result<Response, HttpError> {
    // Interim (1xx) answers come before the real one.
    let (head, body_start) = loop {
        let mut scanned = 0;
        let end = loop {
            if let Some(end) = head_end(&r.buf, scanned) {
                break end;
            }
            scanned = r.buf.len();
            if !r.more()? {
                return Err(if r.buf.is_empty() {
                    HttpError::Closed
                } else {
                    HttpError::Malformed(format!("{} bytes and no end of the head", r.buf.len()))
                });
            }
        };
        let head = parse_head(&r.buf[..end])?;
        if (100..200).contains(&head.status) && head.status != 101 {
            r.drained += end;
            r.buf.drain(..end);
            continue;
        }
        break (head, end);
    };

    let body = match framing(method, &head)? {
        Framing::None => Vec::new(),
        Framing::Length(n) => {
            let stated_end = body_start.saturating_add(n);
            while r.buf.len() < stated_end {
                if !r.more()? {
                    // Closed early: the request asked for the connection
                    // to close, so take what came.
                    break;
                }
            }
            if closes && r.buf.len() > stated_end {
                // More than the stated length came with it, on a connection
                // the device was asked to close after its answer: the length
                // is wrong (a device counting characters, not bytes). Read
                // on to the close, within the same bounds, and take it all.
                // A device that states its length right gets no extra wait.
                loop {
                    match r.more() {
                        Ok(true) => {}
                        Ok(false) | Err(HttpError::NoAnswer(_)) => break,
                        Err(e) => return Err(e),
                    }
                }
                r.buf[body_start..].to_vec()
            } else {
                r.buf[body_start..stated_end.min(r.buf.len())].to_vec()
            }
        }
        Framing::Chunked => {
            let mut chunks = Chunks::default();
            loop {
                if chunks.advance(&r.buf[body_start..])? {
                    break;
                }
                if !r.more()? {
                    // Closed without the last chunk: fine between chunks,
                    // a cut-off answer inside one.
                    if chunks.at_boundary(&r.buf[body_start..]) {
                        break;
                    }
                    return Err(HttpError::CutOff(std::io::ErrorKind::UnexpectedEof.into()));
                }
            }
            chunks.out
        }
        Framing::ToClose => {
            while r.more()? {}
            r.buf[body_start..].to_vec()
        }
    };
    Ok(Response { status: head.status, reason: head.reason, headers: head.headers, body })
}

/// Chunked-body decoding over the bytes received so far, resuming where
/// it stopped as more arrive.
#[derive(Default)]
struct Chunks {
    /// Offset of the next chunk-size line, or trailer line.
    pos: usize,
    /// Past the last chunk, reading the trailer section.
    in_trailer: bool,
    out: Vec<u8>,
}

/// Longest chunk-size or trailer line accepted. Real ones are a few
/// bytes; the bound also keeps each search for a line's end short.
const MAX_CHUNK_LINE: usize = 8 * 1024;

/// One line from `pos`, without its line ending, and where the next
/// begins. None until the line is complete.
fn line_at(data: &[u8], pos: usize) -> Result<Option<(&[u8], usize)>, HttpError> {
    let rest = data.get(pos..).unwrap_or(&[]);
    let window = &rest[..rest.len().min(MAX_CHUNK_LINE + 1)];
    match window.iter().position(|&b| b == b'\n') {
        Some(nl) => {
            let line = &rest[..nl];
            Ok(Some((line.strip_suffix(b"\r").unwrap_or(line), pos + nl + 1)))
        }
        None if window.len() > MAX_CHUNK_LINE => Err(HttpError::Malformed("chunk line too long".into())),
        None => Ok(None),
    }
}

impl Chunks {
    /// Decode what is complete. True once the last chunk and the trailer
    /// section have arrived.
    fn advance(&mut self, data: &[u8]) -> Result<bool, HttpError> {
        loop {
            if self.in_trailer {
                // Trailer fields, up to an empty line, each read once.
                let Some((line, next)) = line_at(data, self.pos)? else { return Ok(false) };
                self.pos = next;
                if line.is_empty() {
                    return Ok(true);
                }
                continue;
            }
            let Some((line, start)) = line_at(data, self.pos)? else { return Ok(false) };
            let size = std::str::from_utf8(line).unwrap_or("");
            let size = size.split(';').next().unwrap_or("").trim();
            if size.is_empty() || size.len() > 15 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(HttpError::Malformed(format!("chunk size {}", shown(&String::from_utf8_lossy(line)))));
            }
            let size = usize::from_str_radix(size, 16).map_err(|_| HttpError::Malformed("chunk size".into()))?;
            if size == 0 {
                self.in_trailer = true;
                self.pos = start;
                continue;
            }
            let Some(end) = start.checked_add(size) else {
                return Err(HttpError::Malformed("chunk size".into()));
            };
            let Some(after) = data.get(end..) else { return Ok(false) };
            let skip = if after.starts_with(b"\r\n") {
                2
            } else if after.starts_with(b"\n") {
                1
            } else if after.is_empty() || after == b"\r" {
                return Ok(false);
            } else {
                return Err(HttpError::Malformed("chunk longer than its size".into()));
            };
            self.out.extend_from_slice(&data[start..end]);
            self.pos = end + skip;
        }
    }

    /// Whether everything received so far ends on a chunk boundary.
    fn at_boundary(&self, data: &[u8]) -> bool {
        self.pos == data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_renderer::{FakeRenderer, Reply};

    const WAIT: Duration = Duration::from_secs(10);

    fn limits() -> Limits {
        Limits { connect: Duration::from_secs(5), deadline: WAIT, max_bytes: 64 * 1024 }
    }

    fn get(url: &str, limits: &Limits) -> Result<Response, HttpError> {
        exchange(
            &Request { method: "GET", url, version: Version::Http11, headers: &[("Connection", "close")], body: &[] },
            limits,
        )
    }

    fn body(r: &Response) -> &str {
        std::str::from_utf8(&r.body).unwrap()
    }

    #[test]
    fn sends_exactly_the_request_it_is_given_in_one_go() {
        let fake = FakeRenderer::start(|_| Reply::ok("<ok/>"));
        let url = fake.url("/ctl/AVT?x=1");
        let r = exchange(
            &Request {
                method: "POST",
                url: &url,
                version: Version::Http11,
                headers: &[("Content-Type", "text/xml"), ("Content-Length", "5"), ("SOAPAction", "\"a#b\"")],
                body: b"<b/>!",
            },
            &limits(),
        )
        .unwrap();
        assert_eq!((r.status, r.reason.as_str(), body(&r)), (200, "OK", "<ok/>"));
        let req = fake.next_request(WAIT).unwrap();
        let port = fake.addr().port();
        assert_eq!(
            String::from_utf8_lossy(&req.raw),
            format!(
                "POST /ctl/AVT?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: text/xml\r\nContent-Length: 5\r\n\
                 SOAPAction: \"a#b\"\r\n\r\n<b/>!"
            )
        );
    }

    #[test]
    fn an_answer_is_complete_at_its_length_while_the_connection_stays_open() {
        let fake = FakeRenderer::start(|_| Reply::ok("<whole/>").then_hold_open());
        let start = Instant::now();
        let r = get(&fake.url("/"), &limits()).unwrap();
        assert_eq!(body(&r), "<whole/>");
        assert!(start.elapsed() < WAIT / 4, "{:?}", start.elapsed());
    }

    #[test]
    fn a_chunked_answer_is_decoded_as_its_chunks_arrive() {
        // Split inside a size line, inside a chunk and inside the trailer.
        let fake = FakeRenderer::start(|_| {
            Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel")
                .then(Duration::from_millis(50), "lo\r\n0")
                .then(Duration::from_millis(50), "b;name=x\r\n, the world\r\n0\r\nX-T: 1\r")
                .then(Duration::from_millis(50), "\n\r\n")
                .then_hold_open()
        });
        let r = get(&fake.url("/"), &limits()).unwrap();
        assert_eq!(body(&r), "hello, the world");
    }

    #[test]
    fn chunks_end_where_the_header_says_not_where_the_body_looks_like_one() {
        // A Content-Length body that starts like a chunk-size line.
        let fake = FakeRenderer::start(|_| Reply::ok("1\r\nnot a chunk"));
        assert_eq!(body(&get(&fake.url("/"), &limits()).unwrap()), "1\r\nnot a chunk");
        // `Transfer-Encoding` names chunked last among several codings.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n2\r\nab\r\n0\r\n\r\n"));
        assert_eq!(get(&fake.url("/"), &limits()).unwrap().body, b"ab");
    }

    #[test]
    fn a_chunked_answer_closed_between_chunks_is_taken_and_inside_one_is_cut_off() {
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n"));
        assert_eq!(get(&fake.url("/"), &limits()).unwrap().body, b"ab");
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nab"));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::CutOff(_))));
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nab\r\n"));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Malformed(_))));
    }

    #[test]
    fn without_length_or_chunks_the_answer_ends_with_the_connection() {
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.0 200 OK\r\n\r\n<all/>").then(Duration::from_millis(50), "<of it/>"));
        assert_eq!(body(&get(&fake.url("/"), &limits()).unwrap()), "<all/><of it/>");
    }

    #[test]
    fn an_answer_closed_before_its_length_keeps_what_came() {
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n<short/>"));
        assert_eq!(body(&get(&fake.url("/"), &limits()).unwrap()), "<short/>");
    }

    #[test]
    fn the_largest_lengths_a_device_can_name_are_bounded_not_trusted() {
        // The longest Content-Length accepted, then the device closes.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 9999999999999999999\r\n\r\n<short/>"));
        assert_eq!(body(&get(&fake.url("/"), &limits()).unwrap()), "<short/>");
        // The same, with the connection left open: the size bound ends it.
        let small = Limits { max_bytes: 1000, ..limits() };
        let fake = FakeRenderer::start(|_| {
            Reply::raw(format!("HTTP/1.1 200 OK\r\nContent-Length: 9999999999999999999\r\n\r\n{}", "x".repeat(2000))).then_hold_open()
        });
        assert!(matches!(get(&fake.url("/"), &small), Err(HttpError::TooLarge(1000))));
        // The longest chunk size accepted, cut short; one digit more is refused.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffff\r\nab"));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::CutOff(_))));
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nab"));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Malformed(_))));
        // Twenty digits of length: refused, not wrapped.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999\r\n\r\nab"));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Malformed(_))));
    }

    #[test]
    fn a_length_stated_too_short_gives_way_to_what_came_before_the_close() {
        // Five bytes claimed, eleven sent, then closed, on a request that
        // asked for the close: the device miscounted.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello world"));
        assert_eq!(body(&get(&fake.url("/"), &limits()).unwrap()), "hello world");
        // The same with the connection left open: what came by the deadline.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello world").then_hold_open());
        let quick = Limits { deadline: Duration::from_millis(300), ..limits() };
        assert_eq!(body(&get(&fake.url("/"), &quick).unwrap()), "hello world");
        // A request that didn't ask for the close keeps the stated length.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello world"));
        let url = fake.url("/");
        let r = exchange(&Request { method: "GET", url: &url, version: Version::Http11, headers: &[], body: &[] }, &limits()).unwrap();
        assert_eq!(body(&r), "hello");
    }

    #[test]
    fn interim_answers_count_against_the_size_bound() {
        // One interim answer at a time, each read and set aside before the
        // next arrives: together far more than the bound.
        let small = Limits { max_bytes: 1000, ..limits() };
        let fake = FakeRenderer::start(|_| {
            let mut r = Reply::raw("HTTP/1.1 100 Continue\r\n\r\n");
            for _ in 0..100 {
                r = r.then(Duration::from_millis(2), "HTTP/1.1 100 Continue\r\n\r\n");
            }
            r.then(Duration::from_millis(2), "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
        });
        assert!(matches!(get(&fake.url("/"), &small), Err(HttpError::TooLarge(1000))));
    }

    #[test]
    fn a_chunk_size_line_without_an_end_is_refused_early() {
        let fake = FakeRenderer::start(|_| {
            Reply::raw(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{}", "0".repeat(20_000))).then_hold_open()
        });
        let start = Instant::now();
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Malformed(_))));
        assert!(start.elapsed() < WAIT / 4, "{:?}", start.elapsed());
    }

    #[test]
    fn heads_with_bare_line_feeds_and_without_reason_phrases_are_read() {
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200\nContent-Length: 3\n\nabc"));
        let r = get(&fake.url("/"), &limits()).unwrap();
        assert_eq!((r.status, r.reason.as_str(), body(&r)), (200, "", "abc"));
        assert_eq!(r.status_text(), "HTTP 200");
        assert_eq!(r.header("content-length"), Some("3"));
    }

    #[test]
    fn interim_answers_are_skipped_and_no_content_has_no_body() {
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"));
        let r = get(&fake.url("/"), &limits()).unwrap();
        assert_eq!((r.status, body(&r)), (200, "ok"));
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 204 No Content\r\n\r\n").then_hold_open());
        let r = get(&fake.url("/"), &limits()).unwrap();
        assert_eq!((r.status, r.body.len()), (204, 0));
    }

    #[test]
    fn what_is_not_http_is_said_to_be_so() {
        for reply in [
            "SIP/2.0 200 OK\r\n\r\n",
            "HTTP/1.1 OK\r\n\r\n",
            "HTTP/1.1 2000 OK\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nabc",
            "HTTP/1.1 200 OK\r\nContent-Length: -2\r\n\r\nab",
        ] {
            let fake = FakeRenderer::start(move |_| Reply::raw(reply));
            assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Malformed(_))), "{reply:?}");
        }
        // Closed before a head was complete.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-"));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Malformed(_))));
        // Closed with nothing said: told apart from an answer cut short.
        let fake = FakeRenderer::start(|_| Reply::raw(""));
        assert!(matches!(get(&fake.url("/"), &limits()), Err(HttpError::Closed)));
    }

    #[test]
    fn an_answer_over_the_size_bound_is_refused() {
        let small = Limits { max_bytes: 1000, ..limits() };
        let big = "x".repeat(2000);
        let fake = FakeRenderer::start(move |_| Reply::ok(&big).then_hold_open());
        assert!(matches!(get(&fake.url("/"), &small), Err(HttpError::TooLarge(1000))));
        // Head and body count together.
        let fake = FakeRenderer::start(|_| Reply::ok(&"y".repeat(990)));
        assert!(matches!(get(&fake.url("/"), &small), Err(HttpError::TooLarge(1000))));
        let fake = FakeRenderer::start(|_| Reply::ok(&"y".repeat(900)));
        assert!(get(&fake.url("/"), &small).is_ok());
    }

    #[test]
    fn no_answer_in_time_and_no_connection_are_different_failures() {
        let fake = FakeRenderer::start(|_| Reply::silent());
        let quick = Limits { deadline: Duration::from_millis(300), ..limits() };
        let start = Instant::now();
        let e = get(&fake.url("/"), &quick).unwrap_err();
        assert!(matches!(e, HttpError::NoAnswer(d) if d == quick.deadline), "{e}");
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
        // An answer trickling in past the deadline doesn't stretch it.
        let fake = FakeRenderer::start(|_| {
            let mut r = Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n");
            for _ in 0..50 {
                r = r.then(Duration::from_millis(30), "x");
            }
            r
        });
        assert!(matches!(get(&fake.url("/"), &quick), Err(HttpError::NoAnswer(_))));

        // Nothing listening.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = get(&format!("http://127.0.0.1:{port}/"), &limits()).unwrap_err();
        assert!(matches!(e, HttpError::Connect(_)), "{e}");
    }

    #[test]
    fn only_plain_http_addresses_are_used() {
        for url in ["https://127.0.0.1/x", "not a url", "file:///etc/hosts"] {
            assert!(matches!(get(url, &limits()), Err(HttpError::BadUrl(_))), "{url}");
        }
    }

    #[test]
    fn the_reason_phrase_is_device_text_quoted_and_bounded() {
        let r = Response {
            status: 500,
            reason: format!("Bad\u{1b}[31m\u{202e}{}", "x".repeat(500)),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let t = r.status_text();
        assert!(t.starts_with("HTTP 500 \"Bad\\u{1b}[31m\\u{202e}xxx"), "{t}");
        assert!(t.ends_with("…\"") && !t.chars().any(|c| c.is_control() || c == '\u{202e}'), "{t}");
        assert!(t.chars().count() < 240, "{t}");
        let ok = Response { status: 503, reason: "Service Unavailable".into(), headers: Vec::new(), body: Vec::new() };
        assert_eq!(ok.status_text(), "HTTP 503 \"Service Unavailable\"");
    }

    #[test]
    fn a_connection_reset_before_any_answer_is_closed_not_cut_off() {
        // Reset as the request arrives: what the device "said" is nothing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let s = socket2::Socket::from(conn);
                let _ = s.set_linger(Some(Duration::ZERO));
                drop(s);
            }
        });
        let e = get(&format!("http://127.0.0.1:{port}/"), &limits()).unwrap_err();
        assert!(matches!(e, HttpError::Closed), "{e:?}");
        // Some of an answer, then the connection breaks: cut off.
        let fake = FakeRenderer::start(|_| Reply::raw("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nab"));
        assert_eq!(body(&get(&fake.url("/"), &limits()).unwrap()), "ab");
    }
}
