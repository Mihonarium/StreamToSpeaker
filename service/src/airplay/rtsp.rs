//! RTSP/1.0 client for the AirPlay control plane.
//!
//! One persistent TCP connection per session. Synchronous request /
//! response — no pipelining. We hand-roll the protocol because
//! standard HTTP libraries don't speak RTSP's verb set (ANNOUNCE,
//! SETUP, RECORD, FLUSH, TEARDOWN, SET_PARAMETER) and a couple of
//! mandatory headers (CSeq, Apple-Challenge, Active-Remote, DACP-ID).
//!
//! ## Sequence
//!
//! ```text
//!   OPTIONS *                       — verify reachable, send Apple-Challenge
//!   ANNOUNCE rtsp://ip/<sess>       — declare codec + rsaaeskey + aesiv (SDP)
//!   SETUP    rtsp://ip/<sess>       — request RTP/UDP server ports
//!   RECORD   rtsp://ip/<sess>       — flip to streaming
//!     (... audio packets on UDP in parallel ...)
//!   SET_PARAMETER rtsp://ip/<sess>  — volume
//!   TEARDOWN rtsp://ip/<sess>       — close
//! ```

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, warn};
use rand::Rng;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::airplay::crypto::{base64_nopad, random_apple_challenge, Cipher};

/// User-Agent string the RTSP requests carry.
///
/// Several RAOP receivers (notably some Sonos firmware revisions) gate
/// playback on a recognised iTunes-style identifier. We send the well-
/// known iTunes 7.6 string used by every open-source sender in the
/// wild — receivers treat it as known-good. The actual product
/// identity is carried in `Client-Instance` / DACP-ID so receivers
/// that DO want our identity can pick it up.
const RTSP_USER_AGENT: &str = "iTunes/7.6.2 (Windows; N;)";

/// SDP "session id" — the random 64-bit number we identify our session
/// by, both in the ANNOUNCE URL and the SDP `o=` line. Once chosen at
/// session start, fixed for the lifetime of the session.
pub type SessionId = u64;

/// SETUP response — the server tells us which UDP ports it bound for
/// audio data, control (sync) and timing.
#[derive(Debug, Clone, Copy)]
pub struct ServerPorts {
    pub audio: u16,
    pub control: u16,
    pub timing: u16,
}

/// A live RTSP control connection.
pub struct RtspClient {
    stream: TcpStream,
    cseq: u32,
    /// Receiver IP, used in URI and SDP origin field.
    receiver_ip: IpAddr,
    /// Local IP, used in SDP `o=` line.
    local_ip: IpAddr,
    /// Persistent session id.
    pub session_id: SessionId,
    /// Per-receiver client instance id (UUID-like 8-byte hex). Apple's
    /// receivers gate certain features on this being stable across
    /// requests within the session.
    pub client_instance: String,
    /// DACP-ID — the iTunes Direct Audio Control Protocol session id,
    /// a fixed 16-hex-char identifier. Some receivers gate playback on
    /// its presence even though we don't currently handle the DACP
    /// callback channel.
    pub dacp_id: String,
    /// Active-Remote header value, paired with DACP-ID. Receivers send
    /// remote-control requests (volume up, play/pause) back to us via
    /// this token.
    pub active_remote: String,
    /// Session token returned by the server in the SETUP response;
    /// echoed in every subsequent request's Session header.
    pub session_token: Option<String>,
}

impl RtspClient {
    /// Open a TCP connection to the receiver, derive the per-session
    /// identifiers, and prepare for the OPTIONS handshake.
    pub fn connect(
        receiver_ip: IpAddr,
        port: u16,
        local_ip: IpAddr,
        timeout: Duration,
    ) -> Result<Self> {
        let addr = SocketAddr::new(receiver_ip, port);
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("connecting RTSP control TCP to {}", addr))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        let mut rng = rand::thread_rng();
        let session_id: u64 = rng.gen();
        let client_instance = format!("{:016X}", rng.gen::<u64>());
        let dacp_id = format!("{:016X}", rng.gen::<u64>());
        let active_remote = format!("{}", rng.gen::<u32>());

        Ok(Self {
            stream,
            cseq: 0,
            receiver_ip,
            local_ip,
            session_id,
            client_instance,
            dacp_id,
            active_remote,
            session_token: None,
        })
    }

    /// Construct the per-session URI used in ANNOUNCE/SETUP/RECORD/TEARDOWN.
    fn session_uri(&self) -> String {
        format!("rtsp://{}/{}", self.receiver_ip, self.session_id)
    }

    /// Send OPTIONS * with an Apple-Challenge nonce. The receiver will
    /// respond with `Apple-Response: <base64-rsa-signed-nonce>`. We
    /// don't currently verify the signature (it would require Apple's
    /// public key for *verification*, which we don't have a clean way
    /// to derive — receivers sign with their own key); just sending
    /// the challenge is sufficient to make most receivers happy.
    pub fn options(&mut self) -> Result<()> {
        let challenge = random_apple_challenge();
        let challenge_b64 = base64_nopad(&challenge);
        let extra = vec![("Apple-Challenge".to_string(), challenge_b64)];
        let resp = self.request("OPTIONS", "*", &extra, "")?;
        debug!(
            "RTSP OPTIONS: Public='{}', Apple-Response present: {}",
            resp.headers.get("public").map(|s| s.as_str()).unwrap_or(""),
            resp.headers.contains_key("apple-response"),
        );
        Ok(())
    }

    /// ANNOUNCE — send the SDP that declares our codec and (for the
    /// encrypted path) ships the RSA-wrapped AES key + IV.
    ///
    /// SDP format (`o=` and `s=` say "iTunes" because some receivers,
    /// notably some Sonos AP2 firmwares, gate on the iTunes brand;
    /// our real identity goes in `Client-Instance`/`DACP-ID`):
    ///
    /// ```text
    /// v=0
    /// o=iTunes <session_id> 0 IN IP4 <local_ip>
    /// s=iTunes
    /// c=IN IP4 <receiver_ip>
    /// t=0 0
    /// m=audio 0 RTP/AVP 96
    /// a=rtpmap:96 AppleLossless
    /// a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
    /// [a=rsaaeskey:<base64-nopad of 256-byte RSA-OAEP ciphertext>]
    /// [a=aesiv:<base64-nopad of 16-byte IV>]
    /// ```
    ///
    /// The last two lines are only emitted for [`Cipher::AesRsa`].
    /// For [`Cipher::None`] the receiver expects raw (unencrypted)
    /// audio RTP packets and we skip the SDP key material entirely.
    pub fn announce(&mut self, cipher: &Cipher) -> Result<()> {
        let crypto_lines = match cipher {
            Cipher::None => String::new(),
            Cipher::AesRsa(key) => {
                let rsa_wrapped = key.rsa_wrapped_key()?;
                let key_b64 = base64_nopad(&rsa_wrapped);
                let iv_b64 = base64_nopad(&key.iv);
                format!("a=rsaaeskey:{}\r\na=aesiv:{}\r\n", key_b64, iv_b64)
            }
        };

        let sdp = format!(
            "v=0\r\n\
             o=iTunes {sid} 0 IN IP4 {local}\r\n\
             s=iTunes\r\n\
             c=IN IP4 {receiver}\r\n\
             t=0 0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 AppleLossless\r\n\
             a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
             {crypto}",
            sid = self.session_id,
            local = self.local_ip,
            receiver = self.receiver_ip,
            crypto = crypto_lines,
        );

        let uri = self.session_uri();
        let extra = vec![(
            "Content-Type".to_string(),
            "application/sdp".to_string(),
        )];
        let mut resp = self.request("ANNOUNCE", &uri, &extra, &sdp)?;
        if resp.status_code == 403 {
            // Some receivers (AirPort Express fw 7.8+, certain Sonos) gate
            // ANNOUNCE behind the MFi `/auth-setup` key exchange. Do it
            // and retry once. Only fires on 403, so receivers that work
            // without it are untouched.
            warn!("ANNOUNCE returned 403; attempting /auth-setup then retrying");
            self.auth_setup().context("auth-setup after ANNOUNCE 403")?;
            resp = self.request("ANNOUNCE", &uri, &extra, &sdp)?;
        }
        if resp.status_code != 200 {
            bail!("ANNOUNCE failed: {} {}", resp.status_code, resp.status_text);
        }
        Ok(())
    }

    /// MFi `/auth-setup` handshake. We send a curve25519 (X25519) public
    /// key prefixed with the `0x01` "unencrypted" selector; the receiver
    /// replies with its own key plus a signed MFi certificate. For the
    /// unencrypted audio path we don't need the response contents — just
    /// completing the exchange unlocks receivers that demand it.
    pub fn auth_setup(&mut self) -> Result<()> {
        use x25519_dalek::{EphemeralSecret, PublicKey};
        let secret = EphemeralSecret::random_from_rng(rand::thread_rng());
        let public = PublicKey::from(&secret);

        let mut body = Vec::with_capacity(33);
        body.push(0x01); // 0x01 = no encryption; 0x02 would request MFi-SAP
        body.extend_from_slice(public.as_bytes());

        let resp = self
            .request_bytes("POST", "/auth-setup", "application/octet-stream", &body)
            .context("sending /auth-setup")?;
        if resp.status_code != 200 {
            bail!("auth-setup → {} {}", resp.status_code, resp.status_text);
        }
        debug!("auth-setup OK ({}B response)", resp.body.len());
        Ok(())
    }

    /// SETUP — request RTP/UDP transport, telling the server which
    /// local UDP ports we'll send/listen on. Parses the server's
    /// `Transport: ...;server_port=X;control_port=Y;timing_port=Z` to
    /// get the receiver's port assignments.
    pub fn setup(
        &mut self,
        client_control_port: u16,
        client_timing_port: u16,
    ) -> Result<ServerPorts> {
        let uri = self.session_uri();
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
            client_control_port, client_timing_port
        );
        let extra = vec![("Transport".to_string(), transport)];
        let resp = self.request("SETUP", &uri, &extra, "")?;
        if resp.status_code != 200 {
            bail!("SETUP failed: {} {}", resp.status_code, resp.status_text);
        }
        // Capture the Session token for subsequent requests.
        if let Some(sess) = resp.headers.get("session") {
            // Format: "<token>;timeout=60"
            let token = sess.split(';').next().unwrap_or(sess).trim().to_string();
            self.session_token = Some(token);
        }
        let transport = resp
            .headers
            .get("transport")
            .ok_or_else(|| anyhow!("SETUP response missing Transport header"))?;

        let mut server_port = None;
        let mut control_port = None;
        let mut timing_port = None;
        for part in transport.split(';') {
            let (k, v) = match part.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };
            match k.trim() {
                "server_port" => server_port = v.split('-').next().and_then(|s| s.parse().ok()),
                "control_port" => control_port = v.parse().ok(),
                "timing_port" => timing_port = v.parse().ok(),
                _ => {}
            }
        }
        let audio = server_port.ok_or_else(|| anyhow!("SETUP: no server_port in transport"))?;
        let control = control_port.unwrap_or(audio);
        let timing = timing_port.unwrap_or(audio);
        Ok(ServerPorts { audio, control, timing })
    }

    /// RECORD — flip the receiver to "expect audio". `initial_seq` and
    /// `initial_rtptime` go in the `RTP-Info` header so the receiver
    /// can prime its jitter buffer.
    pub fn record(&mut self, initial_seq: u16, initial_rtptime: u32) -> Result<()> {
        let uri = self.session_uri();
        let extra = vec![
            ("Range".to_string(), "npt=0-".to_string()),
            (
                "RTP-Info".to_string(),
                format!("seq={};rtptime={}", initial_seq, initial_rtptime),
            ),
            // iTunes/node_airtunes2 parity.
            ("X-Apple-ProtocolVersion".to_string(), "1".to_string()),
        ];
        let resp = self.request("RECORD", &uri, &extra, "")?;
        if resp.status_code != 200 {
            bail!("RECORD failed: {} {}", resp.status_code, resp.status_text);
        }
        Ok(())
    }

    /// SET_PARAMETER volume — RAOP volume is a float in dB, ranging
    /// from -30.0 (very quiet) to 0.0 (loud), with the sentinel
    /// -144.0 meaning fully muted.
    pub fn set_volume(&mut self, db: f32) -> Result<()> {
        let body = format!("volume: {:.6}\r\n", db);
        let uri = self.session_uri();
        let extra = vec![(
            "Content-Type".to_string(),
            "text/parameters".to_string(),
        )];
        let resp = self.request("SET_PARAMETER", &uri, &extra, &body)?;
        if resp.status_code != 200 {
            bail!("SET_PARAMETER volume failed: {} {}", resp.status_code, resp.status_text);
        }
        Ok(())
    }

    /// FLUSH — tell the receiver to drop everything it has buffered up
    /// to `seq`. We use this when restarting playback after a pause.
    pub fn flush(&mut self, seq: u16, rtptime: u32) -> Result<()> {
        let uri = self.session_uri();
        let extra = vec![(
            "RTP-Info".to_string(),
            format!("seq={};rtptime={}", seq, rtptime),
        )];
        let resp = self.request("FLUSH", &uri, &extra, "")?;
        if resp.status_code != 200 {
            bail!("FLUSH failed: {} {}", resp.status_code, resp.status_text);
        }
        Ok(())
    }

    /// TEARDOWN — close the session. Best-effort; receivers eventually
    /// time out anyway. We swallow errors so a half-broken receiver
    /// doesn't block shutdown.
    pub fn teardown(&mut self) {
        let uri = self.session_uri();
        if let Err(e) = self.request("TEARDOWN", &uri, &[], "") {
            warn!("RTSP TEARDOWN failed (continuing anyway): {}", e);
        }
    }

    // -------------------------------------------------------------------
    // Internals
    // -------------------------------------------------------------------

    fn request(
        &mut self,
        method: &str,
        uri: &str,
        extra_headers: &[(String, String)],
        body: &str,
    ) -> Result<RtspResponse> {
        self.cseq += 1;
        let mut req = String::new();
        req.push_str(&format!("{} {} RTSP/1.0\r\n", method, uri));
        req.push_str(&format!("CSeq: {}\r\n", self.cseq));
        req.push_str(&format!("User-Agent: {}\r\n", RTSP_USER_AGENT));
        req.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
        req.push_str(&format!("DACP-ID: {}\r\n", self.dacp_id));
        req.push_str(&format!("Active-Remote: {}\r\n", self.active_remote));
        if let Some(token) = &self.session_token {
            req.push_str(&format!("Session: {}\r\n", token));
        }
        for (k, v) in extra_headers {
            req.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        req.push_str(body);

        debug!(
            "RTSP > {} {} (CSeq={}, body={}B)",
            method,
            uri,
            self.cseq,
            body.len()
        );
        self.stream.write_all(req.as_bytes())?;
        self.stream.flush()?;
        read_response(&mut self.stream)
    }

    /// Like [`request`](Self::request) but with a binary body — used for
    /// `/auth-setup`, whose payload is raw key bytes rather than text.
    fn request_bytes(
        &mut self,
        method: &str,
        uri: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<RtspResponse> {
        self.cseq += 1;
        let mut head = String::new();
        head.push_str(&format!("{} {} RTSP/1.0\r\n", method, uri));
        head.push_str(&format!("CSeq: {}\r\n", self.cseq));
        head.push_str(&format!("User-Agent: {}\r\n", RTSP_USER_AGENT));
        head.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
        head.push_str(&format!("DACP-ID: {}\r\n", self.dacp_id));
        head.push_str(&format!("Active-Remote: {}\r\n", self.active_remote));
        if let Some(token) = &self.session_token {
            head.push_str(&format!("Session: {}\r\n", token));
        }
        head.push_str(&format!("Content-Type: {}\r\n", content_type));
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

        debug!(
            "RTSP > {} {} (CSeq={}, body={}B)",
            method,
            uri,
            self.cseq,
            body.len()
        );
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        self.stream.write_all(&raw)?;
        self.stream.flush()?;
        read_response(&mut self.stream)
    }
}

#[derive(Debug)]
pub struct RtspResponse {
    pub status_code: u16,
    pub status_text: String,
    /// Lower-cased header names → trimmed values.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Read one RTSP/1.0 response off the stream. RTSP/1.0 framing matches
/// HTTP/1.0 closely: status line + headers + optional body delimited
/// by Content-Length.
fn read_response(stream: &mut TcpStream) -> Result<RtspResponse> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            bail!("RTSP connection closed while waiting for response");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            bail!("RTSP response headers too large ({} bytes, no end)", buf.len());
        }
    }

    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| anyhow!("RTSP response headers were not UTF-8"))?
        .to_string();
    let mut lines = head.lines();
    let status_line = lines.next().ok_or_else(|| anyhow!("empty status line"))?;
    let (status_code, status_text) = parse_status_line(status_line)?;
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4; // "\r\n\r\n"
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let need = content_length - body.len();
        let read_cap = tmp.len().min(need);
        let n = stream.read(&mut tmp[..read_cap])?;
        if n == 0 {
            bail!(
                "RTSP body short read: got {} of {} bytes",
                body.len(),
                content_length
            );
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    debug!(
        "RTSP < {} {} ({} headers, {}B body)",
        status_code,
        status_text,
        headers.len(),
        body.len()
    );
    Ok(RtspResponse {
        status_code,
        status_text,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_status_line(line: &str) -> Result<(u16, String)> {
    let mut parts = line.splitn(3, ' ');
    let _proto = parts
        .next()
        .ok_or_else(|| anyhow!("status line missing protocol"))?;
    let code_str = parts
        .next()
        .ok_or_else(|| anyhow!("status line missing code"))?;
    let text = parts.next().unwrap_or("").to_string();
    let code: u16 = code_str
        .parse()
        .with_context(|| format!("parsing status code {:?}", code_str))?;
    Ok((code, text))
}
