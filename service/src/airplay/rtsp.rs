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
//!   OPTIONS * or POST /auth-setup   — the opener (see `session::Opener`)
//!   ANNOUNCE rtsp://ip/<sess>       — declare codec [+ rsaaeskey + aesiv] (SDP)
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
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::airplay::crypto::{base64_nopad, random_apple_challenge, Cipher};

/// User-Agent string the RTSP requests carry.
///
/// Several RAOP receivers (notably some Sonos firmware revisions) gate
/// playback on a recognised iTunes-style identifier. This is the exact
/// string iTunes-for-Windows 12.13.10 sends (packet-capture verified
/// against a Sonos on AirTunes/366 firmware). The actual product
/// identity is carried in `Client-Instance` / DACP-ID so receivers
/// that DO want our identity can pick it up.
const RTSP_USER_AGENT: &str =
    "iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)";

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
    /// The receiver's `Audio-Latency` (samples) if it states one already
    /// at SETUP — Apple TV and AirPort Express do; RECORD may repeat or
    /// revise it. Sonos and most speakers never send it.
    pub audio_latency: Option<u32>,
}

/// What a receiver answered to OPTIONS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionsReply {
    pub status: u16,
    pub reason: String,
    /// The methods its `Public` header lists, as sent.
    pub methods: Vec<String>,
}

impl OptionsReply {
    fn of(resp: &RtspResponse) -> Self {
        let methods = resp
            .headers
            .get("public")
            .map(|p| p.split(',').map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect())
            .unwrap_or_default();
        Self { status: resp.status_code, reason: resp.status_text.clone(), methods }
    }

    /// Does `Public` list `method`?
    pub fn lists(&self, method: &str) -> bool {
        self.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// The receiver's advertised `Audio-Latency` header (samples), if any.
/// Sent on SETUP and/or RECORD responses depending on the receiver.
fn audio_latency_of(resp: &RtspResponse) -> Option<u32> {
    resp.headers
        .get("audio-latency")
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Why a request got no usable answer. It is the root cause of the
/// client's errors (`err.downcast_ref::<RtspError>()`), so callers can
/// tell a receiver that is slow from one that has gone.
#[derive(Debug)]
pub enum RtspError {
    /// Nothing of the reply arrived within the time limit. The connection
    /// is still in step: if the reply comes later, its CSeq marks it as
    /// an earlier request's and it is skipped.
    NoAnswer { request: String, waited: Duration },
    /// The receiver closed or reset the connection.
    ClosedByReceiver { request: String },
    /// This connection can't carry another request: a reply arrived only
    /// in part or malformed, a request went out only in part, or the
    /// connection was shut down here. The receiver may still hold the
    /// session.
    Broken { request: String, why: String },
}

impl std::fmt::Display for RtspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RtspError::NoAnswer { request, waited } => {
                write!(f, "no answer to {} within {:.1} s", request, waited.as_secs_f32())
            }
            RtspError::ClosedByReceiver { request } => write!(f, "the receiver closed the connection ({})", request),
            RtspError::Broken { request, why } => write!(f, "{}: {}", request, why),
        }
    }
}

impl std::error::Error for RtspError {}

/// A 401 that ends the connect: what the receiver wants for a sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// It asks for a password and none is stored.
    PasswordNeeded,
    /// It refused the stored password.
    PasswordRejected,
    /// It asks for a sign-in this client can't answer: no Digest
    /// challenge (`scheme` is the one it named, quoted, if any).
    Unsupported { scheme: Option<String> },
}

impl std::fmt::Display for SignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignIn::PasswordNeeded => f.write_str("the receiver asks for a password (401) and none is stored"),
            SignIn::PasswordRejected => f.write_str("the receiver refused the password (401 again)"),
            SignIn::Unsupported { scheme: Some(s) } => write!(f, "the receiver asks for a sign-in this app can't do (401, {})", s),
            SignIn::Unsupported { scheme: None } => f.write_str("the receiver asks for a sign-in this app can't do (401 with no challenge)"),
        }
    }
}

impl std::error::Error for SignIn {}

/// The receiver answered 453 (Not Enough Bandwidth): it is busy.
/// shairport-sync answers ANNOUNCE so when another connection holds its
/// play lock (`rtsp.c`, `handle_announce`); other receivers are reported
/// to use it the same way.
#[derive(Debug)]
pub struct ReceiverBusy {
    pub request: &'static str,
}

impl std::fmt::Display for ReceiverBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} was answered 453 Not Enough Bandwidth: the receiver is busy", self.request)
    }
}

impl std::error::Error for ReceiverBusy {}

/// What a TEARDOWN may add to ending a session: on the session's own
/// connection the reply is awaited this long; on a new connection, half
/// of it goes to connecting and half to the reply.
pub const TEARDOWN_LIMIT: Duration = Duration::from_secs(2);

/// See [`RtspClient::abandon_handle`].
pub struct AbandonHandle {
    stream: TcpStream,
    abandoned: Arc<AtomicBool>,
}

impl AbandonHandle {
    /// Shut the connection down, so a request waiting on it fails at
    /// once. The client then counts the connection as broken here, and
    /// its TEARDOWN goes on a new connection.
    pub fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The state of the control connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// Every reply so far was read whole: the next request can go out.
    InStep,
    /// Can't carry another request (see [`RtspError::Broken`]); the
    /// receiver may still hold the session.
    Broken,
    /// The receiver closed or reset it.
    ClosedByReceiver,
    /// Ended here, after TEARDOWN.
    Closed,
}

/// A live RTSP control connection.
pub struct RtspClient {
    stream: TcpStream,
    /// Bytes read from the connection and not used yet: the start of the
    /// next reply, when a receiver sends more than one reply at once.
    received: Vec<u8>,
    link: Link,
    /// Set by [`AbandonHandle::abandon`] just before it shuts the
    /// connection down, so the read that then fails is not taken for the
    /// receiver closing it.
    abandoned: Arc<AtomicBool>,
    /// How long one reply may take (the socket's read and write timeout).
    reply_limit: Duration,
    /// See [`TEARDOWN_LIMIT`].
    teardown_limit: Duration,
    cseq: u32,
    /// Receiver IP, used in URI and SDP origin field.
    receiver_ip: IpAddr,
    /// The receiver's RTSP port, for a TEARDOWN on a new connection.
    port: u16,
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
    /// True once ANNOUNCE succeeded — the receiver now holds session
    /// state that must be TEARDOWNed even if a later step fails.
    announced: bool,
    /// True once TEARDOWN has been sent (makes it idempotent).
    torn_down: bool,
    /// RTSP Digest auth (RFC 2617) for password-protected receivers.
    /// `password` comes from config/UI; `realm`/`nonce` are learned from
    /// the first `401 WWW-Authenticate`. Once we have all three, every
    /// request carries an `Authorization: Digest …` header (mirrors
    /// OwnTone, which re-adds it on every request).
    password: Option<String>,
    realm: Option<String>,
    nonce: Option<String>,
    /// gen-1 AirPort Express digest quirk: username `"iTunes"` +
    /// uppercase hex. Set when the device advertises no `am` model —
    /// exactly OwnTone's `RAOP_DEV_APEX1_80211G` detection.
    auth_quirk_itunes: bool,
}

impl RtspClient {
    /// Open a TCP connection to the receiver, derive the per-session
    /// identifiers, and prepare for the OPTIONS handshake. `timeout`
    /// bounds the TCP connect and each reply until
    /// [`set_reply_limit`](Self::set_reply_limit) changes it.
    pub fn connect(
        receiver_ip: IpAddr,
        port: u16,
        local_ip: IpAddr,
        timeout: Duration,
    ) -> Result<Self> {
        Self::connect_auth(receiver_ip, port, local_ip, timeout, None, false)
    }

    /// [`connect`](Self::connect) with RTSP Digest credentials for a
    /// password-protected receiver. `password` is the AirPlay password;
    /// `auth_quirk_itunes` selects the gen-1 AirPort Express digest
    /// convention (username `"iTunes"` + uppercase hex).
    pub fn connect_auth(
        receiver_ip: IpAddr,
        port: u16,
        local_ip: IpAddr,
        timeout: Duration,
        password: Option<String>,
        auth_quirk_itunes: bool,
    ) -> Result<Self> {
        let addr = SocketAddr::new(receiver_ip, port);
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("connecting RTSP control TCP to {}", addr))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        let mut rng = rand::thread_rng();
        // 32-bit id: iTunes uses u32-range session ids (capture:
        // 3865844950); some receivers plausibly parse it as one.
        let session_id: u64 = rng.gen::<u32>() as u64;
        let client_instance = format!("{:016X}", rng.gen::<u64>());
        let dacp_id = format!("{:016X}", rng.gen::<u64>());
        let active_remote = format!("{}", rng.gen::<u32>());

        Ok(Self {
            stream,
            received: Vec::new(),
            link: Link::InStep,
            abandoned: Arc::new(AtomicBool::new(false)),
            reply_limit: timeout,
            teardown_limit: TEARDOWN_LIMIT,
            cseq: 0,
            receiver_ip,
            port,
            local_ip,
            session_id,
            client_instance,
            dacp_id,
            active_remote,
            session_token: None,
            announced: false,
            torn_down: false,
            password,
            realm: None,
            nonce: None,
            auth_quirk_itunes,
        })
    }

    /// How long each reply from now on may take to arrive.
    pub fn set_reply_limit(&mut self, limit: Duration) {
        // A zero timeout is refused by the socket; the smallest real one
        // stands for it.
        let limit = limit.max(Duration::from_millis(1));
        self.reply_limit = limit;
        let _ = self.stream.set_read_timeout(Some(limit));
        let _ = self.stream.set_write_timeout(Some(limit));
    }

    /// See [`TEARDOWN_LIMIT`].
    pub fn set_teardown_limit(&mut self, limit: Duration) {
        self.teardown_limit = limit;
    }

    /// True while the connection can carry another request.
    pub fn is_usable(&self) -> bool {
        self.link == Link::InStep
    }

    /// A handle that can cut a stalled request short from another thread.
    pub fn abandon_handle(&self) -> std::io::Result<AbandonHandle> {
        Ok(AbandonHandle { stream: self.stream.try_clone()?, abandoned: self.abandoned.clone() })
    }

    /// Compute the `Authorization: Digest …` value for one request, if we
    /// have learned a realm/nonce and hold a password. `None` otherwise.
    fn auth_header(&self, method: &str, uri: &str) -> Option<String> {
        let realm = self.realm.as_deref()?;
        let nonce = self.nonce.as_deref()?;
        let password = self.password.as_deref()?;
        let username = if self.auth_quirk_itunes { "iTunes" } else { "" };
        Some(crate::airplay::crypto::digest_auth_header(
            username,
            realm,
            nonce,
            password,
            method,
            uri,
            self.auth_quirk_itunes,
        ))
    }

    /// Parse `WWW-Authenticate: Digest realm="…", nonce="…"` from a 401
    /// response into `self.realm`/`self.nonce`. Returns true on success.
    fn parse_auth_challenge(&mut self, resp: &RtspResponse) -> bool {
        let Some(hdr) = resp.headers.get("www-authenticate") else {
            return false;
        };
        // The scheme and parameter names are case-insensitive (RFC 7235).
        let Some((scheme, rest)) = hdr.trim_start().split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("Digest") {
            return false;
        }
        let mut realm = None;
        let mut nonce = None;
        for part in rest.split(',') {
            let Some((k, v)) = part.split_once('=') else { continue };
            let v = Some(v.trim().trim_matches('"').to_string());
            match k.trim() {
                k if k.eq_ignore_ascii_case("realm") => realm = v,
                k if k.eq_ignore_ascii_case("nonce") => nonce = v,
                _ => {}
            }
        }
        match (realm, nonce) {
            (Some(r), Some(n)) if !r.is_empty() && !n.is_empty() => {
                self.realm = Some(r);
                self.nonce = Some(n);
                true
            }
            _ => false,
        }
    }

    /// Construct the per-session URI used in ANNOUNCE/SETUP/RECORD/TEARDOWN.
    /// Packet-capture verified: iTunes uses the **sender's** IP here
    /// (`rtsp://<local>/<session>`), not the receiver's.
    fn session_uri(&self) -> String {
        format!("rtsp://{}/{}", self.local_ip, self.session_id)
    }

    /// Send OPTIONS * with an Apple-Challenge nonce. The receiver will
    /// respond with `Apple-Response: <base64-rsa-signed-nonce>`. We
    /// don't currently verify the signature (it would require Apple's
    /// public key for *verification*, which we don't have a clean way
    /// to derive — receivers sign with their own key); just sending
    /// the challenge is sufficient to make most receivers happy.
    ///
    /// Whatever the status, the handshake goes on (some receivers don't
    /// implement OPTIONS and answer 501); the caller gets the answer.
    pub fn options(&mut self) -> Result<OptionsReply> {
        let challenge = random_apple_challenge();
        let challenge_b64 = base64_nopad(&challenge);
        let extra = vec![("Apple-Challenge".to_string(), challenge_b64)];
        let resp = self.request("OPTIONS", "*", &extra, "")?;
        debug!(
            "RTSP OPTIONS: Public='{}', Apple-Response present: {}",
            resp.headers.get("public").map(|s| s.as_str()).unwrap_or(""),
            resp.headers.contains_key("apple-response"),
        );
        Ok(OptionsReply::of(&resp))
    }

    /// `OPTIONS *` with nothing added, the opener OwnTone sends: no
    /// challenge (OwnTone adds one only to an encrypted ANNOUNCE).
    pub fn options_plain(&mut self) -> Result<OptionsReply> {
        let resp = self.request("OPTIONS", "*", &[], "")?;
        debug!(
            "RTSP OPTIONS: Public='{}'",
            resp.headers.get("public").map(|s| s.as_str()).unwrap_or("")
        );
        Ok(OptionsReply::of(&resp))
    }

    /// Bare `OPTIONS *` keepalive — sent every couple of seconds during
    /// streaming (iTunes cadence is 2.0 s; libraop proves 25 s also
    /// suffices). Matches iTunes's keepalives exactly: no Apple-Challenge
    /// and no Session header.
    pub fn options_keepalive(&mut self) -> Result<()> {
        let resp = self.request_opts("OPTIONS", "*", &[], "", false)?;
        // The keepalive's job is to keep the connection warm so the
        // receiver doesn't reap the session; any answered response proves
        // that. Some receivers don't implement `OPTIONS *` and reply 501
        // (Not Implemented) — libraop suppresses the error there and its
        // caller ignores the return, i.e. treats it as non-fatal, which is
        // what we do explicitly by accepting 200 and 501 alike. The session
        // keepalive decides what a failure means (see `session.rs`
        // `keepalive_loop`): no answer within the limit, or a connection
        // that broke or was closed, ends the session; a refusal is
        // forgiven once.
        if resp.status_code != 200 && resp.status_code != 501 {
            bail!(
                "OPTIONS keepalive → {} {}",
                resp.status_code,
                resp.status_text
            );
        }
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
            // et=4 MFi: ship the *wrapped* audio key (`mfiaeskey`) plus the
            // plaintext audio IV (`aesiv`). Matches iTunes's ANNOUNCE to
            // AirTunes/366 Sonos firmware.
            Cipher::Mfi(mfi) => {
                let key_b64 = base64_nopad(&mfi.mfiaeskey);
                let iv_b64 = base64_nopad(&mfi.audio.iv);
                format!("a=mfiaeskey:{}\r\na=aesiv:{}\r\n", key_b64, iv_b64)
            }
        };

        // `o=` carries the SENDER address; `c=` the RECEIVER's. Packet-
        // capture verified against iTunes → Sonos (and matches OwnTone's
        // `raop.c`, which puts `rs->address` = the receiver in `c=`). A
        // prior revision put the sender in `c=` following node_airtunes2;
        // the proven-on-this-device reference uses the receiver.
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
            // Throwaway unlock — this fallback fires only for non-MFi ciphers
            // (the et=4 path already did auth-setup up front and built its
            // key from that secret), so the derived secret is discarded here.
            let _ = self.auth_setup().context("auth-setup after ANNOUNCE 403")?;
            resp = self.request("ANNOUNCE", &uri, &extra, &sdp)?;
        }
        if resp.status_code == 453 {
            return Err(ReceiverBusy { request: "ANNOUNCE" }.into());
        }
        if resp.status_code != 200 {
            bail!("ANNOUNCE failed: {} {}", resp.status_code, resp.status_text);
        }
        self.announced = true;
        Ok(())
    }

    /// MFi `/auth-setup` handshake. We send a curve25519 (X25519) public
    /// key prefixed with the `0x01` "unencrypted" selector; the receiver
    /// replies with its own key (first 32 bytes) plus a signed MFi
    /// certificate + signature.
    ///
    /// Returns the **X25519 shared secret**. For the plain/RSA audio paths
    /// the caller ignores it (just completing the exchange unlocks
    /// receivers that gate ANNOUNCE on it); for the et=4 MFi path it's the
    /// key-encryption-key input that wraps the audio key (see
    /// [`crate::airplay::crypto::MfiKey::derive`]).
    pub fn auth_setup(&mut self) -> Result<[u8; 32]> {
        use x25519_dalek::{EphemeralSecret, PublicKey};
        let secret = EphemeralSecret::random_from_rng(rand::thread_rng());
        let public = PublicKey::from(&secret);

        let mut body = Vec::with_capacity(33);
        body.push(0x01); // 0x01 = no encryption; 0x10 would request MFi-SAP
        body.extend_from_slice(public.as_bytes());

        let resp = self
            .request_bytes("POST", "/auth-setup", "application/octet-stream", &body)
            .context("sending /auth-setup")?;
        if resp.status_code != 200 {
            bail!("auth-setup → {} {}", resp.status_code, resp.status_text);
        }
        // Response: [32B receiver X25519 pubkey][4B certlen][cert]
        //           [4B siglen][sig]. We need the first 32 bytes to
        // complete the ECDH.
        let their_pub: [u8; 32] = resp.body.get(..32).and_then(|b| b.try_into().ok()).ok_or_else(|| {
            anyhow!(
                "auth-setup response too short ({}B, need ≥32 for the receiver pubkey)",
                resp.body.len()
            )
        })?;
        let shared = secret.diffie_hellman(&PublicKey::from(their_pub));
        debug!(
            "auth-setup OK ({}B response, ECDH shared secret derived)",
            resp.body.len()
        );
        Ok(*shared.as_bytes())
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
        if resp.status_code == 453 {
            return Err(ReceiverBusy { request: "SETUP" }.into());
        }
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
        let audio_latency = audio_latency_of(&resp);
        Ok(ServerPorts { audio, control, timing, audio_latency })
    }

    /// RECORD — flip the receiver to "expect audio". `initial_seq` and
    /// `initial_rtptime` go in the `RTP-Info` header so the receiver
    /// can prime its jitter buffer.
    ///
    /// Returns the receiver's advertised `Audio-Latency` (in samples) if
    /// present. Big-DSP AVRs (Denon/Yamaha) report large values here and
    /// the sender must anchor that far back or audio arrives late; Sonos
    /// and most speakers omit it (→ `None`, we keep our default anchor).
    pub fn record(&mut self, initial_seq: u16, initial_rtptime: u32) -> Result<Option<u32>> {
        let uri = self.session_uri();
        // Header set matches iTunes 12.13.10's RECORD exactly (packet-
        // capture verified): just Range + RTP-Info. An earlier build
        // added X-Apple-ProtocolVersion (node_airtunes2 habit); iTunes
        // doesn't send it, so neither do we.
        let extra = vec![
            ("Range".to_string(), "npt=0-".to_string()),
            (
                "RTP-Info".to_string(),
                format!("seq={};rtptime={}", initial_seq, initial_rtptime),
            ),
        ];
        let resp = self.request("RECORD", &uri, &extra, "")?;
        if resp.status_code != 200 {
            bail!("RECORD failed: {} {}", resp.status_code, resp.status_text);
        }
        Ok(audio_latency_of(&resp))
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

    /// SET_PARAMETER track metadata — a DMAP-tagged body (see
    /// [`crate::airplay::dmap`]) so the receiver shows the current
    /// title/artist/album on its display or app. Best-effort: metadata is
    /// non-fatal on every reference receiver, so callers should treat a
    /// failure as cosmetic.
    pub fn set_metadata(&mut self, dmap_body: &[u8], rtptime: u32) -> Result<()> {
        self.cseq += 1;
        let uri = self.session_uri();
        let extra = [
            ("RTP-Info".to_string(), format!("rtptime={}", rtptime)),
            ("Content-Type".to_string(), "application/x-dmap-tagged".to_string()),
        ];
        let raw = self.build_request("SET_PARAMETER", &uri, &extra, dmap_body, true);
        debug!("RTSP > SET_PARAMETER {} (metadata, {}B)", uri, dmap_body.len());
        let resp = self.send_and_read(&raw, "SET_PARAMETER")?;
        if resp.status_code != 200 {
            bail!(
                "SET_PARAMETER metadata → {} {}",
                resp.status_code,
                resp.status_text
            );
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

    /// TEARDOWN — end the session, then close the connection. A receiver
    /// that is not told keeps the session for a while and refuses or
    /// ignores new ones meanwhile (the Sonos holds it for tens of
    /// seconds, packet capture), so this is sent whenever the receiver
    /// may hold one: on the session's connection while it is in step, on
    /// a new connection when this one broke. It is not sent when the
    /// receiver closed the connection itself, or before ANNOUNCE was
    /// accepted. Either way it takes at most [`TEARDOWN_LIMIT`], and
    /// failures are only logged.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        if self.abandoned.load(Ordering::Acquire) && self.link == Link::InStep {
            // Shut down by an abandon handle between two requests.
            self.link = Link::Broken;
        }
        if self.announced {
            match self.link {
                Link::InStep => {
                    self.set_reply_limit(self.teardown_limit);
                    let uri = self.session_uri();
                    if let Err(e) = self.request("TEARDOWN", &uri, &[], "") {
                        warn!("RTSP TEARDOWN failed (continuing anyway): {:#}", e);
                    }
                }
                Link::Broken => self.teardown_on_new_connection(),
                Link::ClosedByReceiver | Link::Closed => {}
            }
        }
        let _ = self.stream.shutdown(Shutdown::Both);
        self.link = Link::Closed;
    }

    /// TEARDOWN for a session whose connection broke: shut that one down
    /// first, so the receiver sees it end at once (a receiver that keeps
    /// one session per connection lets go of it there), then send
    /// TEARDOWN naming the session on a new connection, for receivers
    /// that keep it by its Session, DACP-ID and Active-Remote.
    fn teardown_on_new_connection(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        let half = (self.teardown_limit / 2).max(Duration::from_millis(1));
        let addr = SocketAddr::new(self.receiver_ip, self.port);
        match TcpStream::connect_timeout(&addr, half) {
            Ok(stream) => {
                debug!("RTSP: the session's connection broke; sending TEARDOWN on a new connection");
                self.stream = stream;
                self.received.clear();
                self.link = Link::InStep;
                self.set_reply_limit(half);
                let uri = self.session_uri();
                if let Err(e) = self.request("TEARDOWN", &uri, &[], "") {
                    warn!("RTSP TEARDOWN on a new connection failed (continuing anyway): {:#}", e);
                }
            }
            Err(e) => warn!("RTSP TEARDOWN: couldn't open a new connection to {} ({})", addr, e),
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
        self.request_opts(method, uri, extra_headers, body, true)
    }

    /// Like [`request`](Self::request) but with control over whether the
    /// Session header is attached. iTunes omits it on OPTIONS keepalives.
    fn request_opts(
        &mut self,
        method: &str,
        uri: &str,
        extra_headers: &[(String, String)],
        body: &str,
        include_session: bool,
    ) -> Result<RtspResponse> {
        self.exchange(method, uri, extra_headers, body.as_bytes(), include_session)
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
        let extra = [("Content-Type".to_string(), content_type.to_string())];
        self.exchange(method, uri, &extra, body, true)
    }

    /// Send one request and read its reply, handling RTSP Digest auth
    /// transparently: once a realm/nonce is known each request carries
    /// the `Authorization` header, and a `401` on the first (un-authed)
    /// attempt triggers a challenge parse + one retry — mirroring
    /// OwnTone's re-run-with-auth flow.
    fn exchange(
        &mut self,
        method: &str,
        uri: &str,
        extra_headers: &[(String, String)],
        body: &[u8],
        include_session: bool,
    ) -> Result<RtspResponse> {
        let label = if method == "POST" { format!("POST {}", uri) } else { method.to_string() };
        for attempt in 0..2 {
            self.cseq += 1;
            let raw = self.build_request(method, uri, extra_headers, body, include_session);
            debug!(
                "RTSP > {} {} (CSeq={}, body={}B)",
                method,
                uri,
                self.cseq,
                body.len()
            );
            let resp = self.send_and_read(&raw, &label)?;
            if let Some(r) = self.handle_auth(resp, attempt)? {
                return Ok(r);
            }
        }
        bail!("{} was refused twice with 401", label)
    }

    /// One request's bytes, with the current CSeq. Header order is what
    /// iTunes sends (packet capture): the identifiers, then the
    /// request's own headers, then Content-Length when there is a body.
    fn build_request(
        &self,
        method: &str,
        uri: &str,
        extra_headers: &[(String, String)],
        body: &[u8],
        include_session: bool,
    ) -> Vec<u8> {
        let mut head = String::new();
        head.push_str(&format!("{} {} RTSP/1.0\r\n", method, uri));
        head.push_str(&format!("CSeq: {}\r\n", self.cseq));
        head.push_str(&format!("User-Agent: {}\r\n", RTSP_USER_AGENT));
        head.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
        head.push_str(&format!("DACP-ID: {}\r\n", self.dacp_id));
        head.push_str(&format!("Active-Remote: {}\r\n", self.active_remote));
        if let Some(auth) = self.auth_header(method, uri) {
            head.push_str(&format!("Authorization: {}\r\n", auth));
        }
        if include_session {
            if let Some(token) = &self.session_token {
                head.push_str(&format!("Session: {}\r\n", token));
            }
        }
        for (k, v) in extra_headers {
            head.push_str(&format!("{}: {}\r\n", k, v));
        }
        if !body.is_empty() {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    /// Shared 401 handling for the request builders. Returns `Some(resp)`
    /// to hand back to the caller, or `None` to retry the request (now
    /// with the just-learned credentials).
    fn handle_auth(&mut self, resp: RtspResponse, attempt: usize) -> Result<Option<RtspResponse>> {
        if resp.status_code != 401 {
            return Ok(Some(resp));
        }
        if !self.parse_auth_challenge(&resp) {
            // No Digest challenge we can answer.
            let scheme = resp
                .headers
                .get("www-authenticate")
                .and_then(|h| h.split_whitespace().next())
                .map(crate::discovery_net::shown);
            return Err(SignIn::Unsupported { scheme }.into());
        }
        if self.password.is_none() {
            return Err(SignIn::PasswordNeeded.into());
        }
        if attempt == 0 {
            return Ok(None); // retry, now with the password
        }
        // Refused again with the password sent: it is wrong.
        Err(SignIn::PasswordRejected.into())
    }

    /// Write a fully-serialised request and read its reply, keeping
    /// requests and replies paired:
    ///
    /// * A reply that doesn't come within the time limit leaves the
    ///   connection in step as long as none of it arrived ([`RtspError::NoAnswer`]).
    ///   When it does come, later, it carries that earlier request's
    ///   CSeq and is skipped here, so replies can't shift one place
    ///   against requests.
    /// * A reply that arrived only in part, or a request written only in
    ///   part, leaves the stream inside a message: the connection is
    ///   marked broken and every later request fails at once.
    /// * Bytes read past the end of a reply are kept for the next one.
    fn send_and_read(&mut self, raw: &[u8], label: &str) -> Result<RtspResponse> {
        if let Some(e) = self.unusable(label) {
            return Err(e.into());
        }
        if let Err(e) = self.stream.write_all(raw).and_then(|_| self.stream.flush()) {
            return Err(self.lost(label, &e, "writing the request").into());
        }
        // Up to 3 stale replies skipped before giving up — more than
        // one means something is deeply wrong with the receiver.
        for _ in 0..3 {
            let resp = self.read_reply(label)?;
            match resp
                .headers
                .get("cseq")
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                Some(c) if c < self.cseq => {
                    debug!(
                        "RTSP: discarding stale response (CSeq {} < current {})",
                        c, self.cseq
                    );
                    continue;
                }
                // Matching CSeq, no CSeq header (trust positionally), or
                // a from-the-future CSeq (nothing sane to do but accept).
                _ => return Ok(resp),
            }
        }
        self.link = Link::Broken;
        Err(RtspError::Broken { request: label.to_string(), why: "three stale replies in a row".to_string() }.into())
    }

    /// Why a request can't go out now, if it can't.
    fn unusable(&self, label: &str) -> Option<RtspError> {
        let request = label.to_string();
        match self.link {
            Link::InStep => None,
            Link::Broken => Some(RtspError::Broken { request, why: "an earlier request broke the connection".to_string() }),
            Link::ClosedByReceiver => Some(RtspError::ClosedByReceiver { request }),
            Link::Closed => Some(RtspError::Broken { request, why: "the session has ended".to_string() }),
        }
    }

    /// The connection failed with `e` while `doing` something: a reset
    /// or broken pipe is the receiver ending it, anything else leaves it
    /// broken here.
    fn lost(&mut self, label: &str, e: &std::io::Error, doing: &str) -> RtspError {
        let by_receiver = matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::BrokenPipe)
            && !self.abandoned.load(Ordering::Acquire);
        if by_receiver {
            self.link = Link::ClosedByReceiver;
            RtspError::ClosedByReceiver { request: label.to_string() }
        } else {
            self.link = Link::Broken;
            RtspError::Broken { request: label.to_string(), why: format!("{}: {}", doing, e) }
        }
    }

    /// Read one reply: from what was already received if a whole one is
    /// there, else from the connection, waiting at most the reply limit
    /// for each read.
    fn read_reply(&mut self, label: &str) -> Result<RtspResponse, RtspError> {
        let started = Instant::now();
        let mut tmp = [0u8; 2048];
        let mut skipped = 0usize;
        loop {
            skipped = skipped.saturating_add(drop_between_replies(&mut self.received));
            if skipped > MAX_HEAD_BYTES {
                self.link = Link::Broken;
                return Err(RtspError::Broken {
                    request: label.to_string(),
                    why: format!("more than {} bytes that aren't a reply", MAX_HEAD_BYTES),
                });
            }
            match parse_reply(&self.received) {
                Ok(Some((resp, used))) => {
                    self.received.drain(..used.min(self.received.len()));
                    debug!(
                        "RTSP < {} {} ({} headers, {}B body)",
                        resp.status_code,
                        resp.status_text,
                        resp.headers.len(),
                        resp.body.len()
                    );
                    return Ok(resp);
                }
                Ok(None) => {}
                Err(why) => {
                    self.link = Link::Broken;
                    return Err(RtspError::Broken { request: label.to_string(), why });
                }
            }
            if started.elapsed() >= RESPONSE_DEADLINE {
                self.link = Link::Broken;
                return Err(RtspError::Broken {
                    request: label.to_string(),
                    why: format!("the reply took over {} s", RESPONSE_DEADLINE.as_secs()),
                });
            }
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    if self.abandoned.load(Ordering::Acquire) {
                        self.link = Link::Broken;
                        return Err(RtspError::Broken { request: label.to_string(), why: "shut down here".to_string() });
                    }
                    self.link = Link::ClosedByReceiver;
                    return Err(RtspError::ClosedByReceiver { request: label.to_string() });
                }
                Ok(n) => self.received.extend_from_slice(tmp.get(..n).unwrap_or_default()),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if self.received.is_empty() && !self.abandoned.load(Ordering::Acquire) {
                        return Err(RtspError::NoAnswer { request: label.to_string(), waited: started.elapsed() });
                    }
                    self.link = Link::Broken;
                    return Err(RtspError::Broken {
                        request: label.to_string(),
                        why: "the reply stopped part way".to_string(),
                    });
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(self.lost(label, &e, "reading the reply")),
            }
        }
    }
}

impl Drop for RtspClient {
    /// A session abandoned mid-bring-up (e.g. SETUP timed out) must still
    /// TEARDOWN: the receiver holds the announced session for tens of
    /// seconds otherwise, and every retry in that window — including the
    /// AirPlay 2 fallback on the same port — times out.
    fn drop(&mut self) {
        if self.announced && !self.torn_down {
            self.teardown();
        }
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

/// Largest response body accepted. RAOP replies are a few hundred bytes;
/// anything larger is refused rather than buffered.
const MAX_BODY_BYTES: usize = 1 << 20;

/// Overall time allowed for one reply, however slowly it trickles in.
/// The reply limit bounds each individual read.
const RESPONSE_DEADLINE: Duration = Duration::from_secs(30);

/// Most bytes of status line and headers accepted before their end.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Drop from the front of `buf` what can come between replies but can't
/// start one: anything before the next `RTSP/` (or `HTTP/`, which the
/// status line has always been allowed to name) — a line end after a
/// reply, a body longer than its Content-Length. A tail that may be the
/// start of one is kept for the next read. Returns how many bytes went.
/// The client always tolerated these (it used to drop everything read
/// past a reply); keeping later bytes for the next reply must not change
/// that.
fn drop_between_replies(buf: &mut Vec<u8>) -> usize {
    const STARTS: [&[u8]; 2] = [b"RTSP/", b"HTTP/"];
    let found = buf.windows(5).position(|w| STARTS.iter().any(|s| w == *s));
    let cut = match found {
        Some(at) => at,
        None => {
            let keep = (1..5)
                .rev()
                .find(|&k| STARTS.iter().any(|s| s.get(..k).is_some_and(|prefix| buf.ends_with(prefix))))
                .unwrap_or(0);
            buf.len().saturating_sub(keep)
        }
    };
    buf.drain(..cut.min(buf.len()));
    cut
}

/// Parse one RTSP/1.0 reply from the front of `buf`. RTSP/1.0 framing
/// matches HTTP/1.0 closely: status line + headers + optional body
/// delimited by Content-Length. `Ok(None)` while the reply is still
/// incomplete; otherwise the reply and how many bytes of `buf` it took.
/// `Err` says why the bytes can't be a reply.
fn parse_reply(buf: &[u8]) -> Result<Option<(RtspResponse, usize)>, String> {
    let Some(header_end) = find_header_end(buf) else {
        if buf.len() > MAX_HEAD_BYTES {
            return Err(format!("reply headers too large ({} bytes, no end)", buf.len()));
        }
        return Ok(None);
    };
    let head = buf
        .get(..header_end)
        .and_then(|h| std::str::from_utf8(h).ok())
        .ok_or_else(|| "reply headers were not UTF-8".to_string())?;
    let mut lines = head.lines();
    let status_line = lines.next().ok_or_else(|| "empty status line".to_string())?;
    let (status_code, status_text) = parse_status_line(status_line).map_err(|e| format!("{:#}", e))?;
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
    if content_length > MAX_BODY_BYTES {
        return Err(format!("reply body too large ({} bytes)", content_length));
    }
    let body_start = header_end.saturating_add(4); // "\r\n\r\n"
    let Some(end) = body_start.checked_add(content_length) else {
        return Err("reply body too large".to_string());
    };
    let Some(body) = buf.get(body_start..end) else {
        return Ok(None);
    };
    Ok(Some((
        RtspResponse {
            status_code,
            status_text,
            headers,
            body: body.to_vec(),
        },
        end,
    )))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::fake_raop::{Answer, Event, FakeRaop, Reply, Request};

    /// The per-request time limit these tests give the client.
    const LIMIT: Duration = Duration::from_secs(1);

    fn client(rx: &FakeRaop) -> RtspClient {
        RtspClient::connect("127.0.0.1".parse().unwrap(), rx.port(), "127.0.0.1".parse().unwrap(), LIMIT).unwrap()
    }

    /// Connect and get as far as SETUP, which `setup` answers.
    fn announced(rx: &FakeRaop) -> RtspClient {
        let mut c = client(rx);
        c.announce(&Cipher::None).expect("ANNOUNCE answered");
        c
    }

    fn connected(rx: &FakeRaop, conn: usize) -> bool {
        rx.wait_until(Duration::from_millis(500), |ev| ev.iter().any(|e| matches!(e, Event::Connected { conn: c, .. } if *c == conn)))
    }

    fn teardowns(rx: &FakeRaop) -> Vec<Request> {
        rx.requests().into_iter().filter(|r| r.method == "TEARDOWN").collect()
    }

    #[test]
    fn a_setup_left_unanswered_is_torn_down_on_the_same_connection() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::Ignore,
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        assert!(c.setup(1, 2).is_err());
        drop(c);
        assert!(rx.wait_for_requests(3, Duration::from_secs(10)), "{:?}", rx.lines());
        let lines = rx.lines();
        assert_eq!(lines[2], format!("0 TEARDOWN rtsp://127.0.0.1/{}", lines[1].rsplit('/').next().unwrap()));
        // And the connection is closed straight after.
        assert!(rx.wait_until(Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Event::Closed { conn: 0, .. }))));
    }

    #[test]
    fn a_late_reply_is_skipped_and_the_next_request_gets_its_own() {
        // SETUP's reply comes after the client gave up on it, and says 500,
        // so a keepalive paired with it would fail.
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::late(LIMIT * 3, Answer::Reply(Reply::status(500, "Late"))),
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        assert!(c.setup(1, 2).is_err());
        assert!(c.is_usable());
        // Long enough for the late reply to come in before the
        // keepalive's own.
        c.set_reply_limit(LIMIT * 10);
        c.options_keepalive().expect("the keepalive gets its own reply");
        c.options_keepalive().expect("and so does the next one");
        assert_eq!(rx.lines()[2..], ["0 OPTIONS *", "0 OPTIONS *"]);
    }

    #[test]
    fn two_replies_read_at_once_are_both_used() {
        // The receiver answers the first keepalive and, in the same write,
        // the second one before it was even asked.
        let rx = FakeRaop::start(|req| match req.cseq() {
            Some(1) => Answer::Raw(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\nRTSP/1.0 501 Not Implemented\r\nCSeq: 2\r\n\r\n".to_vec()),
            _ => Answer::Ignore,
        });
        let mut c = client(&rx);
        c.options_keepalive().expect("first reply");
        // 501 is accepted by the keepalive: it can only have come from the
        // buffered second reply, since the receiver answers nothing else.
        c.options_keepalive().expect("second reply, from what was already read");
    }

    /// What a receiver sends after the end of a reply, in the same write,
    /// is ignored as it always was: a stray line end, or a body longer
    /// than its Content-Length.
    #[test]
    fn bytes_after_a_reply_do_not_break_the_next_request() {
        for extra in [&b"\r\n"[..], b"\n", b"\r\n\r\n"] {
            let mut first = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n".to_vec();
            first.extend_from_slice(extra);
            let rx = FakeRaop::start(move |req| match req.cseq() {
                Some(1) => Answer::Raw(first.clone()),
                Some(2) => Answer::Normal,
                _ => Answer::Ignore,
            });
            let mut c = client(&rx);
            c.options_keepalive().expect("first");
            c.options_keepalive().expect("second, after the stray bytes");
            // And an unanswered request after them is just unanswered.
            let e = c.options_keepalive().expect_err("third gets no answer");
            assert!(matches!(e.downcast_ref::<RtspError>(), Some(RtspError::NoAnswer { .. })), "{e:#}");
            assert!(c.is_usable());
        }
        let rx = FakeRaop::start(|req| match req.cseq() {
            Some(1) => Answer::Raw(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\nabXYZ".to_vec()),
            _ => Answer::Normal,
        });
        let mut c = client(&rx);
        c.options_keepalive().expect("first");
        c.options_keepalive().expect("second, after the overlong body");
    }

    /// Junk without any reply in it is still bounded.
    #[test]
    fn endless_junk_breaks_the_connection() {
        let rx = FakeRaop::start(|_| Answer::Raw(vec![b'x'; MAX_HEAD_BYTES + 10]));
        let mut c = client(&rx);
        assert!(c.options_keepalive().is_err());
        assert!(!c.is_usable());
    }

    #[test]
    fn a_reply_cut_off_part_way_breaks_the_connection_and_teardown_uses_a_new_one() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("SETUP", 0) => Answer::Raw(b"RTSP/1.0 200 OK\r\nCSeq: ".to_vec()),
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        assert!(c.setup(1, 2).is_err());
        // The connection can't carry another request now.
        assert!(c.options_keepalive().is_err());
        assert_eq!(rx.requests().len(), 2, "nothing more was sent on the broken connection");
        drop(c);
        assert!(rx.wait_for_requests(3, Duration::from_secs(10)));
        let reqs = rx.requests();
        let (announce, teardown) = (&reqs[0], &reqs[2]);
        assert_eq!((teardown.conn, teardown.method.as_str()), (1, "TEARDOWN"));
        assert_eq!(teardown.uri, announce.uri);
        for h in ["Client-Instance", "DACP-ID", "Active-Remote", "User-Agent"] {
            assert_eq!(teardown.header(h), announce.header(h), "{h}");
        }
        // The old connection was shut down before TEARDOWN went out on the
        // new one (event order, not timing).
        let ev = rx.events();
        let closed = ev.iter().position(|e| matches!(e, Event::Closed { conn: 0, .. })).expect("the broken connection is closed");
        let teardown_at = ev.iter().position(|e| matches!(e, Event::Request(r) if r.conn == 1)).unwrap();
        assert!(closed < teardown_at, "{ev:?}");
    }

    #[test]
    fn a_teardown_on_a_new_connection_names_the_session() {
        // SETUP answered (so there is a session token), RECORD cut off.
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("RECORD", 0) => Answer::Raw(b"RTSP/1.0 2".to_vec()),
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        c.setup(1, 2).expect("SETUP answered");
        assert!(c.record(1, 1).is_err());
        drop(c);
        assert!(rx.wait_for_requests(4, Duration::from_secs(10)));
        let teardown = &rx.requests()[3];
        assert_eq!((teardown.conn, teardown.method.as_str()), (1, "TEARDOWN"));
        assert_eq!(teardown.header("Session"), Some("1"));
    }

    #[test]
    fn a_connection_the_receiver_closed_gets_no_teardown() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::Close,
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        assert!(c.setup(1, 2).is_err());
        drop(c);
        assert!(!connected(&rx, 1), "a new connection was opened");
        assert!(teardowns(&rx).is_empty());
    }

    #[test]
    fn nothing_is_torn_down_before_announce() {
        let rx = FakeRaop::start(|_| Answer::Ignore);
        let mut c = client(&rx);
        assert!(c.auth_setup().is_err());
        drop(c);
        assert!(!connected(&rx, 1));
        assert!(teardowns(&rx).is_empty());
        assert_eq!(rx.lines(), ["0 POST /auth-setup"]);
    }

    #[test]
    fn an_unanswered_teardown_is_given_up_on_quickly() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" | "TEARDOWN" => Answer::Ignore,
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        c.set_teardown_limit(LIMIT);
        assert!(c.setup(1, 2).is_err());
        let started = Instant::now();
        drop(c);
        let took = started.elapsed();
        assert!(took >= LIMIT && took < LIMIT * 10, "{:?}", took);
        assert_eq!(teardowns(&rx).len(), 1);
    }

    #[test]
    fn a_teardown_on_a_new_connection_is_given_up_on_quickly() {
        let rx = FakeRaop::start(|req| match (req.method.as_str(), req.conn) {
            ("SETUP", 0) => Answer::Raw(b"RTSP/1.0".to_vec()),
            ("TEARDOWN", _) => Answer::Ignore,
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        c.set_teardown_limit(LIMIT);
        assert!(c.setup(1, 2).is_err());
        let started = Instant::now();
        drop(c);
        let took = started.elapsed();
        assert!(took < LIMIT * 10, "{:?}", took);
        assert_eq!(teardowns(&rx).len(), 1);
    }

    #[test]
    fn a_reply_written_in_pieces_is_read_whole() {
        let rx = FakeRaop::start(|req| match req.method.as_str() {
            "SETUP" => Answer::in_pieces(Answer::Normal, &[3, 20, 40, 90], LIMIT / 10),
            _ => Answer::Normal,
        });
        let mut c = announced(&rx);
        let ports = c.setup(1, 2).expect("SETUP read whole");
        assert_eq!(ports.audio, rx.udp_ports().audio);
    }

    fn sign_in(rx: &FakeRaop, password: Option<&str>) -> SignIn {
        let ip = "127.0.0.1".parse().unwrap();
        let mut c = RtspClient::connect_auth(ip, rx.port(), ip, LIMIT, password.map(String::from), false).unwrap();
        let e = c.options().expect_err("refused");
        e.downcast_ref::<SignIn>().cloned().unwrap_or_else(|| panic!("not a sign-in error: {e:#}"))
    }

    /// A Digest challenge is recognised whatever the case of its scheme
    /// and parameter names.
    #[test]
    fn a_digest_challenge_in_other_case_is_answered() {
        let rx = FakeRaop::start(|req| match req.header("Authorization") {
            None => Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "digest REALM=\"raop\", Nonce=\"n1\"")),
            Some(_) => Answer::Normal,
        });
        let ip = "127.0.0.1".parse().unwrap();
        let mut c = RtspClient::connect_auth(ip, rx.port(), ip, LIMIT, Some("pw".into()), false).unwrap();
        c.options().expect("signed in");
        let auth = rx.requests()[1].header("Authorization").unwrap().to_string();
        assert!(auth.contains("realm=\"raop\"") && auth.contains("nonce=\"n1\""), "{auth}");
    }

    /// The three ways a password can stop a connect are told apart.
    #[test]
    fn sign_in_failures_are_told_apart() {
        let digest = || FakeRaop::start(|req| match req.header("Authorization") {
            None => Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"n1\"")),
            Some(_) => Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"n2\"")),
        });
        assert_eq!(sign_in(&digest(), None), SignIn::PasswordNeeded);
        let rx = digest();
        assert_eq!(sign_in(&rx, Some("wrong")), SignIn::PasswordRejected);
        assert_eq!(rx.requests().len(), 2, "one retry with the password");
        let basic = FakeRaop::start(|_| Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Basic realm=\"x\"")));
        assert_eq!(sign_in(&basic, Some("pw")), SignIn::Unsupported { scheme: Some("\"Basic\"".into()) });
        assert!(basic.requests().iter().all(|r| r.header("Authorization").is_none()), "nothing sent for a scheme we can't do");
        let bare = FakeRaop::start(|_| Answer::Reply(Reply::status(401, "Unauthorized")));
        assert_eq!(sign_in(&bare, None), SignIn::Unsupported { scheme: None });
    }

    #[test]
    fn parse_reply_reads_body_by_content_length_and_leaves_the_rest() {
        let buf = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 3\r\n\r\nabcRTSP/1.0 501 No\r\n\r\n";
        let (r, used) = parse_reply(buf).unwrap().unwrap();
        assert_eq!(r.status_code, 200);
        assert_eq!(r.body, b"abc");
        let (next, used2) = parse_reply(&buf[used..]).unwrap().unwrap();
        assert_eq!((next.status_code, next.status_text.as_str()), (501, "No"));
        assert_eq!(used + used2, buf.len());
    }

    #[test]
    fn only_what_cant_start_a_reply_is_dropped() {
        let cases: [(&[u8], &[u8]); 8] = [
            (b"\r\nRTSP/1.0 200 OK", b"RTSP/1.0 200 OK"),
            (b"\r\nHTTP/1.1 200 OK", b"HTTP/1.1 200 OK"),
            (b"xxHTT", b"HTT"),
            (b"XYZRTSP/1.0", b"RTSP/1.0"),
            (b"\r\n", b""),
            (b"abcRTS", b"RTS"),
            (b"R", b"R"),
            (b"RTSP/1.0 200 OK\r\n", b"RTSP/1.0 200 OK\r\n"),
        ];
        for (given, left) in cases {
            let mut b = given.to_vec();
            let dropped = drop_between_replies(&mut b);
            assert_eq!((b.as_slice(), dropped), (left, given.len() - left.len()), "{:?}", String::from_utf8_lossy(given));
        }
    }

    #[test]
    fn parse_reply_waits_for_the_whole_reply() {
        let full = b"RTSP/1.0 200 OK\r\nContent-Length: 4\r\n\r\nabcd";
        for cut in 0..full.len() {
            assert!(parse_reply(&full[..cut]).unwrap().is_none(), "{cut}");
        }
        assert!(parse_reply(full).unwrap().is_some());
    }

    #[test]
    fn parse_reply_refuses_what_cant_be_a_reply() {
        assert!(parse_reply(b"RTSP/1.0 200 OK\r\nContent-Length: 99999999999\r\n\r\n").is_err());
        let huge = format!("RTSP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", usize::MAX);
        assert!(parse_reply(huge.as_bytes()).is_err());
        assert!(parse_reply(b"RTSP/1.0 2x0 OK\r\n\r\n").is_err());
        assert!(parse_reply(b"\xff\xfe\r\n\r\n").is_err());
        assert!(parse_reply(b"\r\n\r\n").is_err());
        assert!(parse_reply(&vec![b'a'; MAX_HEAD_BYTES + 1]).is_err());
    }
}
