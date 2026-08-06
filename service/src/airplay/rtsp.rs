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
    /// True once ANNOUNCE succeeded — the receiver now holds session
    /// state that must be TEARDOWNed even if a later step fails.
    announced: bool,
    /// True once TEARDOWN has been sent (makes it idempotent).
    torn_down: bool,
    /// Set when a request failed mid-response (read timeout / short
    /// read): the TCP stream may be positioned mid-message, so any
    /// further request would mis-pair responses. Once poisoned, every
    /// request fails fast — the periodic keepalive surfaces it and the
    /// session is effectively dead.
    poisoned: bool,
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
    /// identifiers, and prepare for the OPTIONS handshake.
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
            cseq: 0,
            receiver_ip,
            local_ip,
            session_id,
            client_instance,
            dacp_id,
            active_remote,
            session_token: None,
            announced: false,
            torn_down: false,
            poisoned: false,
            password,
            realm: None,
            nonce: None,
            auth_quirk_itunes,
        })
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
        let Some(rest) = hdr.strip_prefix("Digest ") else {
            return false;
        };
        let mut realm = None;
        let mut nonce = None;
        for part in rest.split(',') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("realm=") {
                realm = Some(v.trim().trim_matches('"').to_string());
            } else if let Some(v) = part.strip_prefix("nonce=") {
                nonce = Some(v.trim().trim_matches('"').to_string());
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
        // what we do explicitly by accepting 200 and 501 alike. Only a
        // read/write failure (which poisons the connection) is session death.
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
        if resp.body.len() < 32 {
            bail!(
                "auth-setup response too short ({}B, need ≥32 for the receiver pubkey)",
                resp.body.len()
            );
        }
        let mut their_pub = [0u8; 32];
        their_pub.copy_from_slice(&resp.body[..32]);
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
        let audio_latency = resp
            .headers
            .get("audio-latency")
            .and_then(|s| s.trim().parse::<u32>().ok());
        Ok(audio_latency)
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
        if self.poisoned {
            bail!("RTSP connection is poisoned");
        }
        self.cseq += 1;
        let uri = self.session_uri();
        let mut head = String::new();
        head.push_str(&format!("SET_PARAMETER {} RTSP/1.0\r\n", uri));
        head.push_str(&format!("CSeq: {}\r\n", self.cseq));
        head.push_str(&format!("User-Agent: {}\r\n", RTSP_USER_AGENT));
        head.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
        head.push_str(&format!("DACP-ID: {}\r\n", self.dacp_id));
        head.push_str(&format!("Active-Remote: {}\r\n", self.active_remote));
        if let Some(auth) = self.auth_header("SET_PARAMETER", &uri) {
            head.push_str(&format!("Authorization: {}\r\n", auth));
        }
        if let Some(token) = &self.session_token {
            head.push_str(&format!("Session: {}\r\n", token));
        }
        head.push_str(&format!("RTP-Info: rtptime={}\r\n", rtptime));
        head.push_str("Content-Type: application/x-dmap-tagged\r\n");
        head.push_str(&format!("Content-Length: {}\r\n\r\n", dmap_body.len()));

        debug!("RTSP > SET_PARAMETER {} (metadata, {}B)", uri, dmap_body.len());
        let mut raw = head.into_bytes();
        raw.extend_from_slice(dmap_body);
        let resp = self.send_and_read(&raw)?;
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

    /// TEARDOWN — close the session. Best-effort; receivers eventually
    /// time out anyway. We swallow errors so a half-broken receiver
    /// doesn't block shutdown.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
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
        self.request_opts(method, uri, extra_headers, body, true)
    }

    /// Like [`request`](Self::request) but with control over whether the
    /// Session header is attached. iTunes omits it on OPTIONS keepalives.
    ///
    /// Handles RTSP Digest auth transparently: once a realm/nonce is
    /// known each request carries the `Authorization` header, and a
    /// `401` on the first (un-authed) attempt triggers a challenge parse
    /// + one retry — mirroring OwnTone's re-run-with-auth flow.
    fn request_opts(
        &mut self,
        method: &str,
        uri: &str,
        extra_headers: &[(String, String)],
        body: &str,
        include_session: bool,
    ) -> Result<RtspResponse> {
        for attempt in 0..2 {
            self.cseq += 1;
            let mut req = String::new();
            req.push_str(&format!("{} {} RTSP/1.0\r\n", method, uri));
            req.push_str(&format!("CSeq: {}\r\n", self.cseq));
            req.push_str(&format!("User-Agent: {}\r\n", RTSP_USER_AGENT));
            req.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
            req.push_str(&format!("DACP-ID: {}\r\n", self.dacp_id));
            req.push_str(&format!("Active-Remote: {}\r\n", self.active_remote));
            if let Some(auth) = self.auth_header(method, uri) {
                req.push_str(&format!("Authorization: {}\r\n", auth));
            }
            if include_session {
                if let Some(token) = &self.session_token {
                    req.push_str(&format!("Session: {}\r\n", token));
                }
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
            let resp = self.send_and_read(req.as_bytes())?;
            if let Some(r) = self.handle_auth(resp, attempt)? {
                return Ok(r);
            }
        }
        unreachable!("auth retry loop always returns")
    }

    /// Shared 401 handling for the request builders. Returns `Some(resp)`
    /// to hand back to the caller, or `None` to retry the request (now
    /// with the just-learned credentials).
    fn handle_auth(&mut self, resp: RtspResponse, attempt: usize) -> Result<Option<RtspResponse>> {
        if resp.status_code != 401 {
            return Ok(Some(resp));
        }
        // First 401: parse the challenge and retry with auth.
        if attempt == 0 && self.parse_auth_challenge(&resp) {
            if self.password.is_none() {
                bail!(
                    "{} is password-protected — a password is required",
                    self.receiver_ip
                );
            }
            return Ok(None); // retry
        }
        // Second 401 (we already sent auth) or an unparseable challenge:
        // the password is wrong (or the scheme unsupported).
        bail!(
            "authentication failed for {} — wrong AirPlay password?",
            self.receiver_ip
        );
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
        for attempt in 0..2 {
            self.cseq += 1;
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
            let resp = self.send_and_read(&raw)?;
            if let Some(r) = self.handle_auth(resp, attempt)? {
                return Ok(r);
            }
        }
        unreachable!("auth retry loop always returns")
    }

    /// Write a fully-serialised request and read its response, with two
    /// protections the keepalive-era connection needs:
    ///
    /// * **Poisoning** — a request that fails mid-response (read timeout,
    ///   short read) may leave the TCP stream positioned inside a message;
    ///   any later read would mis-pair or mis-parse. The connection is
    ///   marked poisoned and every subsequent request fails fast.
    /// * **CSeq pairing** — a response that arrives *after* its request
    ///   timed out (but cleanly, between messages) carries a stale CSeq;
    ///   it's discarded and the read retried so responses can't shift
    ///   off-by-one against requests.
    fn send_and_read(&mut self, raw: &[u8]) -> Result<RtspResponse> {
        if self.poisoned {
            bail!("RTSP connection is poisoned (an earlier request failed mid-response)");
        }
        if let Err(e) = self.stream.write_all(raw).and_then(|_| self.stream.flush()) {
            self.poisoned = true;
            return Err(e).context("writing RTSP request");
        }
        // Up to 3 stale responses discarded before giving up — more than
        // one means something is deeply wrong with the receiver.
        for _ in 0..3 {
            let resp = match read_response(&mut self.stream) {
                Ok(r) => r,
                Err(e) => {
                    self.poisoned = true;
                    return Err(e).context("reading RTSP response");
                }
            };
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
        self.poisoned = true;
        bail!("RTSP connection desynced: >3 stale responses in a row");
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
