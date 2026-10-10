//! AirPlay 2 RTSP client: HomeKit pairing, then an encrypted control
//! channel carrying binary-plist `SETUP` / `RECORD` / `SET_PARAMETER`.
//!
//! Connection lifecycle (mirrors OwnTone's `airplay.c`):
//!
//! ```text
//!   TCP connect to the _airplay._tcp port (usually 7000)
//!   POST /pair-setup  (plaintext, X-Apple-HKP: 4)  ×2   → SessionKeys
//!   ── channel now ChaCha20-Poly1305 encrypted ──
//!   SETUP  rtsp://ip/<sid>  {timingProtocol:NTP, timingPort,…}  → eventPort
//!   SETUP  rtsp://ip/<sid>  {streams:[{type:96, shk, …}]}       → data/control ports
//!   RECORD rtsp://ip/<sid>
//!   SET_PARAMETER … (volume)
//!   TEARDOWN on close
//! ```
//!
//! Request/response framing on the encrypted channel is the HAP block
//! format implemented in [`crate::airplay::ap2_crypto::ChannelCipher`].

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, info};
use plist::Value;
use rand::Rng;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::airplay::ap2_crypto::{ChannelCipher, HapReader, SessionKeys};
use crate::airplay::hap_pairing::{PairSetupPin, PairVerify, PairingCredentials, X_APPLE_HKP_PERSISTENT};
use crate::airplay::pairing::{TransientPairing, X_APPLE_HKP_VALUE};

const USER_AGENT: &str = "AirPlay/665.13.1";

/// RTSP status code AirPlay 2 receivers return when they refuse transient
/// pairing and require PIN (persistent HomeKit) verification — an Apple TV
/// with access control. OwnTone calls this `RTSP_CONNECTION_AUTH_REQUIRED`
/// and switches `pair_type` to `PAIR_CLIENT_HOMEKIT_NORMAL`.
const RTSP_CONNECTION_AUTH_REQUIRED: u16 = 470;

/// How long TEARDOWN waits for its reply.
const TEARDOWN_WAIT: Duration = Duration::from_millis(300);
/// Response header block limit in strict mode; anything larger is a
/// protocol error.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Header block limit otherwise (the long-standing bound).
const LENIENT_MAX_HEADER_BYTES: usize = 1 << 20;
/// Sent-but-unanswered requests tracked for reply attribution.
const PENDING_CAP: usize = 64;
/// Response body limit (a `GET /info` plist is a few KiB).
const MAX_BODY_BYTES: usize = 1 << 20;
/// Late (stale-CSeq) response statuses kept for a caller that stopped
/// waiting, so a `/feedback` reply consumed by a later request still
/// counts.
const LATE_STATUS_CAP: usize = 16;

/// Outcome of an attempted transient pair-setup.
pub enum TransientOutcome {
    /// Paired — the channel is now encrypted; carries the 32-byte audio key.
    Paired([u8; 32]),
    /// The receiver returned 470: it requires one-time PIN pairing. The
    /// caller runs the pin_start → pair_setup_pin ceremony, then reconnects
    /// and pair-verifies.
    NeedsPin,
}

/// Why a `pair-verify` failed. The distinction is load-bearing: stored
/// long-term credentials may only be discarded on [`Rejected`] — a response
/// was received and the receiver (or its crypto) refused the pairing. A
/// [`Transport`] failure (socket timeout, reset, half-open hold) proves
/// nothing about the credentials, and wiping them on it would force the
/// user through a new PIN ceremony after every Wi-Fi blip. Mirrors
/// OwnTone, which clears `auth_key` only in the pair-verify response
/// handlers' error paths, never on connection failures.
///
/// [`Rejected`]: PairVerifyError::Rejected
/// [`Transport`]: PairVerifyError::Transport
#[derive(Debug, thiserror::Error)]
pub enum PairVerifyError {
    /// Socket-level failure — no response received. Credentials unproven.
    #[error("pair-verify transport failure: {0:#}")]
    Transport(#[source] anyhow::Error),
    /// The receiver answered and refused (non-200 status, TLV error, or a
    /// failed signature check). Credentials are stale — re-pair.
    #[error("pair-verify rejected by receiver: {0:#}")]
    Rejected(#[source] anyhow::Error),
}

/// Outcome of a volume read-back via `GET /info`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InfoVolume {
    Db(f64),
    /// Answered, but without a usable `initialVolume` (never invent one).
    Missing,
    /// The receiver refused `GET /info` with this status.
    Refused(u16),
}

/// State of a pending `GET /info` volume read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InfoPoll {
    Pending,
    Done(InfoVolume),
    /// The reply was consumed by a later request (or never matched).
    Lost,
}

/// Ports the receiver assigned for the audio stream (from the second
/// SETUP response).
#[derive(Debug, Clone, Copy)]
pub struct StreamPorts {
    /// UDP port on the receiver we send audio RTP packets to.
    pub data: u16,
    /// UDP port on the receiver for the control (sync/resend) channel.
    pub control: u16,
}

/// Ports the receiver returned from the first (timing) SETUP.
#[derive(Debug, Clone, Copy)]
pub struct TimingSetup {
    /// TCP event-channel port — must be connected before RECORD.
    pub event_port: u16,
    /// The receiver's NTP timing port (0 if absent / PTP mode).
    pub timing_port: u16,
}

/// A live AirPlay 2 RTSP control connection.
pub struct Ap2Rtsp {
    stream: TcpStream,
    cseq: u32,
    local_ip: IpAddr,
    receiver_ip: IpAddr,
    session_id: u32,
    device_id_mac: String,
    client_instance: String,
    active_remote: String,
    session_uuid: String,
    /// Default per-request response timeout.
    timeout: Duration,
    /// Outbound cipher / inbound record decoder — None until pairing
    /// completes.
    writer: Option<ChannelCipher>,
    reader: Option<HapReader>,
    /// Event-channel `(writer, reader)` ciphers from the same pairing,
    /// handed to the event-channel thread once.
    event_ciphers: Option<(ChannelCipher, ChannelCipher)>,
    /// Decrypted (or plaintext) response bytes not yet consumed.
    rx_buf: Vec<u8>,
    /// `(CSeq, status)` of responses that arrived after their request
    /// stopped waiting.
    late: VecDeque<(u32, u16)>,
    /// Set when the connection can no longer be trusted: a write failed
    /// partway (an encrypted record can't be retried with a fresh nonce),
    /// a record failed authentication, the peer closed, or a response
    /// carried a CSeq we never sent (strict mode). Every later request
    /// fails fast.
    poisoned: bool,
    /// CSeqs sent and not yet answered, oldest first. RTSP answers in
    /// order, so a reply without a CSeq belongs to the oldest of these —
    /// never to a newer request after an earlier one timed out.
    pending: VecDeque<u32>,
    /// Strict mode (HomePod sessions): duplicate or unparseable framing
    /// headers and chunked replies are protocol errors, an unknown CSeq
    /// poisons the connection, and TEARDOWN waits only briefly. Off,
    /// framing is read leniently as it always was.
    strict: bool,
    /// True once a TEARDOWN has been sent — makes `teardown` idempotent
    /// and lets Drop skip the duplicate.
    torn_down: bool,
}

impl Ap2Rtsp {
    pub fn connect(
        receiver_ip: IpAddr,
        port: u16,
        local_ip: IpAddr,
        timeout: Duration,
    ) -> Result<Self> {
        let addr = SocketAddr::new(receiver_ip, port);
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("connecting AirPlay 2 RTSP to {}", addr))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true).ok();

        let mut rng = rand::thread_rng();
        let session_id: u32 = rng.gen();
        let mac: [u8; 6] = rng.gen();
        let device_id_mac = mac
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(":");
        let client_instance = format!("{:016X}", rng.gen::<u64>());
        let active_remote = format!("{}", rng.gen::<u32>());
        let session_uuid = format_uuid(rng.gen());

        Ok(Self {
            stream,
            cseq: 0,
            local_ip,
            receiver_ip,
            session_id,
            device_id_mac,
            client_instance,
            active_remote,
            session_uuid,
            timeout,
            writer: None,
            reader: None,
            event_ciphers: None,
            rx_buf: Vec::with_capacity(4096),
            late: VecDeque::new(),
            poisoned: false,
            pending: VecDeque::new(),
            strict: false,
            torn_down: false,
        })
    }

    fn session_uri(&self) -> String {
        match self.receiver_ip {
            IpAddr::V4(_) => format!("rtsp://{}/{}", self.local_ip, self.session_id),
            IpAddr::V6(_) => format!("rtsp://[{}]/{}", self.local_ip, self.session_id),
        }
    }

    /// GET /info — the canonical first request of an AirPlay 2 session
    /// (iOS sends it before pairing; spec 7.1.1). The response is a
    /// receiver-capabilities plist; we log a couple of fields. Plaintext,
    /// pre-pairing.
    pub fn get_info(&mut self) -> Result<()> {
        let extra = vec![("X-Apple-ProtocolVersion".to_string(), "1".to_string())];
        let resp = self.request("GET", "/info", &extra, None, &[])?;
        if resp.status != 200 {
            bail!("GET /info → {} {}", resp.status, resp.status_text);
        }
        if let Ok(v) = plist::from_bytes::<Value>(&resp.body) {
            if let Some(d) = v.as_dictionary() {
                debug!(
                    "AP2 /info: model={:?} srcvers={:?} ({} keys)",
                    d.get("model").and_then(|v| v.as_string()),
                    d.get("sourceVersion").and_then(|v| v.as_string()),
                    d.len(),
                );
                // Load-bearing diagnostic: the receiver's own codec table.
                // audioFormats[].audioOutputFormats for type-103 entries is
                // the authoritative answer to "does this device play ALAC
                // or only AAC in buffered mode".
                if let Some(formats) = d.get("audioFormats") {
                    info!("AP2 /info audioFormats: {:?}", formats);
                }
            }
        }
        Ok(())
    }

    /// Send `GET /info` on the paired (encrypted) connection without
    /// waiting, to read the receiver's `initialVolume` back; collect the
    /// answer with [`Ap2Rtsp::poll_info_volume`].
    pub fn send_info(&mut self) -> Result<u32> {
        self.send_request("GET", "/info", &[], None, &[])
    }

    /// Wait up to `wait` for the reply to the `GET /info` sent as `cseq`.
    /// `Err` only for transport failures.
    pub fn poll_info_volume(&mut self, cseq: u32, wait: Duration) -> Result<InfoPoll> {
        // Another request already read past it: the body is gone.
        if self.take_late(cseq).is_some() || self.cseq != cseq {
            return Ok(InfoPoll::Lost);
        }
        let Some(resp) = self.await_response(cseq, Instant::now() + wait, false)? else {
            return Ok(InfoPoll::Pending);
        };
        if resp.status != 200 {
            return Ok(InfoPoll::Done(InfoVolume::Refused(resp.status)));
        }
        Ok(InfoPoll::Done(match parse_initial_volume(&resp.body) {
            Some(db) => InfoVolume::Db(db),
            None => InfoVolume::Missing,
        }))
    }

    /// Mark the connection unusable — strict mode only; otherwise the
    /// connection is used as long as the socket allows, as it always was.
    fn poison(&mut self) {
        if self.strict {
            self.poisoned = true;
        }
    }

    /// Turn on strict mode (see the `strict` field).
    pub fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
    }

    /// Take the event-channel ciphers derived during pairing (once).
    pub fn take_event_ciphers(&mut self) -> Option<(ChannelCipher, ChannelCipher)> {
        self.event_ciphers.take()
    }

    fn install_keys(&mut self, keys: &SessionKeys) {
        self.writer = Some(keys.control_writer());
        self.reader = Some(HapReader::new(keys.control_reader()));
        self.event_ciphers = Some(keys.event_ciphers());
    }

    /// FLUSHBUFFERED — flush the buffered stream up to (seq, ts); sent
    /// before TEARDOWN when stopping a buffered session (spec 7.1.15;
    /// `flushUntilSeq`/`flushUntilTS` are the required keys).
    pub fn flush_buffered(&mut self, until_seq: u32, until_ts: u32) -> Result<()> {
        let mut dict = plist::Dictionary::new();
        dict.insert("flushUntilSeq".into(), Value::Integer((until_seq as u64).into()));
        dict.insert("flushUntilTS".into(), Value::Integer((until_ts as u64).into()));
        let body = to_binary_plist(&Value::Dictionary(dict))?;
        let uri = self.session_uri();
        let resp = self.request("FLUSHBUFFERED", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!("FLUSHBUFFERED → {} {}", resp.status, resp.status_text);
        }
        Ok(())
    }

    /// Run HomeKit transient pair-setup. On success the channel becomes
    /// encrypted and the 32-byte audio key is returned for the stream
    /// SETUP `shk`. A 470 on the first request means the receiver refuses
    /// transient pairing and wants PIN verification — reported as
    /// [`TransientOutcome::NeedsPin`] so the caller can run the PIN
    /// ceremony instead of failing hard.
    pub fn pair_setup_transient(&mut self) -> Result<TransientOutcome> {
        let mut pairing = TransientPairing::new();

        let hkp = vec![("X-Apple-HKP".to_string(), X_APPLE_HKP_VALUE.to_string())];

        let m1 = pairing.start();
        let r1 = self.request("POST", "/pair-setup", &hkp, Some("application/octet-stream"), &m1)?;
        if r1.status == RTSP_CONNECTION_AUTH_REQUIRED {
            debug!("AirPlay 2: transient pair-setup got 470 — receiver requires PIN pairing");
            return Ok(TransientOutcome::NeedsPin);
        }
        if r1.status != 200 {
            bail!("pair-setup M1 → {} {}", r1.status, r1.status_text);
        }

        let m3 = pairing.handle_m2(&r1.body).context("pair-setup M2→M3")?;
        let r2 = self.request("POST", "/pair-setup", &hkp, Some("application/octet-stream"), &m3)?;
        if r2.status != 200 {
            bail!("pair-setup M3 → {} {}", r2.status, r2.status_text);
        }

        let keys: SessionKeys = pairing.handle_m4(&r2.body).context("pair-setup M4")?;
        let audio_key = keys.audio_key();
        // From here on, everything is encrypted.
        self.install_keys(&keys);
        debug!("AirPlay 2: transient pairing complete, channel encrypted");
        Ok(TransientOutcome::Paired(audio_key))
    }

    /// POST `/pair-pin-start` (X-Apple-HKP: 3) — ask the receiver to enter
    /// persistent-pairing mode and display a PIN. Sent once, before the PIN
    /// pair-setup; mirrors OwnTone's `AIRPLAY_SEQ_PIN_START`. Plaintext
    /// (pairing hasn't produced keys yet).
    pub fn pin_start(&mut self) -> Result<()> {
        let hkp = vec![("X-Apple-HKP".to_string(), X_APPLE_HKP_PERSISTENT.to_string())];
        let resp = self.request("POST", "/pair-pin-start", &hkp, None, &[])?;
        if resp.status != 200 {
            bail!("/pair-pin-start → {} {}", resp.status, resp.status_text);
        }
        debug!("AirPlay 2: /pair-pin-start accepted — receiver is displaying its PIN");
        Ok(())
    }

    /// Persistent HomeKit pair-setup M1–M6 (X-Apple-HKP: 3) using the
    /// on-screen `pin`, returning the long-term [`PairingCredentials`] to
    /// persist. Unlike transient pairing this exchanges Ed25519 long-term
    /// keys and does **not** encrypt the channel — a later `pair_verify`
    /// (on a fresh connection) derives the session keys. Call `pin_start`
    /// first so the receiver is showing the PIN.
    ///
    /// `controller_id`/`controller_seed_hex` are the install's persistent
    /// pairing identity — reusing them makes a re-pair replace the
    /// accessory's stored record instead of consuming another of its
    /// finite pairing slots.
    pub fn pair_setup_pin(
        &mut self,
        pin: &str,
        controller_id: &str,
        controller_seed_hex: &str,
    ) -> Result<PairingCredentials> {
        let hkp = vec![("X-Apple-HKP".to_string(), X_APPLE_HKP_PERSISTENT.to_string())];
        let mut ps = PairSetupPin::with_identity(pin, controller_id.to_string(), controller_seed_hex)?;

        let m1 = ps.start();
        let r1 = self.request("POST", "/pair-setup", &hkp, Some("application/octet-stream"), &m1)?;
        if r1.status != 200 {
            bail!("PIN pair-setup M1 → {} {}{}", r1.status, r1.status_text, describe_error_body(&r1.body));
        }
        let m3 = ps.handle_m2(&r1.body).context("PIN pair-setup M2→M3")?;
        let r2 = self.request("POST", "/pair-setup", &hkp, Some("application/octet-stream"), &m3)?;
        if r2.status != 200 {
            bail!("PIN pair-setup M3 → {} {}{}", r2.status, r2.status_text, describe_error_body(&r2.body));
        }
        let m5 = ps.handle_m4(&r2.body).context("PIN pair-setup M4→M5 (wrong PIN?)")?;
        let r3 = self.request("POST", "/pair-setup", &hkp, Some("application/octet-stream"), &m5)?;
        if r3.status != 200 {
            bail!("PIN pair-setup M5 → {} {}{}", r3.status, r3.status_text, describe_error_body(&r3.body));
        }
        let creds = ps.handle_m6(&r3.body).context("PIN pair-setup M6")?;
        debug!("AirPlay 2: PIN pair-setup complete — stored long-term credentials");
        Ok(creds)
    }

    /// HomeKit pair-verify M1–M4 (X-Apple-HKP: 3) using stored
    /// [`PairingCredentials`]. On success the channel becomes encrypted
    /// (like transient pairing) and the 32-byte audio key is returned. The
    /// session key is the X25519 shared secret run through the same
    /// derivation as transient (OwnTone's `session_cipher_setup`).
    ///
    /// The error type distinguishes transport failures from receiver
    /// rejections so the caller only invalidates stored credentials when
    /// the receiver actually refused them.
    pub fn pair_verify(
        &mut self,
        creds: &PairingCredentials,
    ) -> std::result::Result<[u8; 32], PairVerifyError> {
        use PairVerifyError::{Rejected, Transport};
        let hkp = vec![("X-Apple-HKP".to_string(), X_APPLE_HKP_PERSISTENT.to_string())];
        let mut pv = PairVerify::new(creds.clone());

        let m1 = pv.start();
        let r1 = self
            .request("POST", "/pair-verify", &hkp, Some("application/octet-stream"), &m1)
            .map_err(Transport)?;
        if r1.status != 200 {
            return Err(Rejected(anyhow!(
                "pair-verify M1 → {} {}{}",
                r1.status,
                r1.status_text,
                describe_error_body(&r1.body)
            )));
        }
        let m3 = pv
            .handle_m2(&r1.body)
            .context("pair-verify M2→M3")
            .map_err(Rejected)?;
        let r2 = self
            .request("POST", "/pair-verify", &hkp, Some("application/octet-stream"), &m3)
            .map_err(Transport)?;
        if r2.status != 200 {
            return Err(Rejected(anyhow!(
                "pair-verify M3 → {} {}{}",
                r2.status,
                r2.status_text,
                describe_error_body(&r2.body)
            )));
        }
        let mut shared = pv.finish(&r2.body).context("pair-verify M4").map_err(Rejected)?;
        let keys = SessionKeys::from_shared(&shared);
        // The shared secret is the IKM for every session key — wipe our
        // stack copy once the (zeroize-on-Drop) SessionKeys are derived.
        {
            use zeroize::Zeroize;
            shared.zeroize();
        }
        let audio_key = keys.audio_key();
        self.install_keys(&keys);
        debug!("AirPlay 2: pair-verify complete, channel encrypted");
        Ok(audio_key)
    }

    /// First SETUP — declare the timing protocol and our timing port,
    /// learn the receiver's event + timing ports. NTP timing (the classic
    /// RAOP timing packets) is what OwnTone shipped for AirPlay 2 for
    /// years — it's the known-working combination with Sonos.
    pub fn setup_timing_ntp(&mut self, timing_port: u16) -> Result<TimingSetup> {
        let mut dict = plist::Dictionary::new();
        dict.insert("deviceID".into(), self.device_id_mac.clone().into());
        dict.insert("sessionUUID".into(), self.session_uuid.clone().into());
        dict.insert("timingProtocol".into(), "NTP".into());
        dict.insert("timingPort".into(), Value::Integer((timing_port as u64).into()));
        let body = to_binary_plist(&Value::Dictionary(dict))?;

        let uri = self.session_uri();
        let resp = self.request("SETUP", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!("SETUP(timing) → {} {}", resp.status, resp.status_text);
        }
        Ok(parse_timing_setup(&resp.body))
    }

    /// First SETUP, PTP variant — declare IEEE-1588 timing and advertise
    /// **our own clock** as the timing peer: in AirPlay 2 the sender is the
    /// PTP grandmaster and the receiver follows it (see
    /// [`crate::airplay::ap2_ptp`]). `clock_id` is the u64 clock identity
    /// our PTP master serves; `clock_uuid` is its UUID string. The peer
    /// dict shape mirrors OwnTone's `payload_make_setup_session` PTP
    /// variant: ID (UUID), ClockID (int64), DeviceType, Addresses,
    /// SupportsClockPortMatchingOverride — plus a `timingPeerList` copy.
    pub fn setup_timing_ptp(&mut self, clock_id: u64, clock_uuid: &str) -> Result<TimingSetup> {
        let mut peer = plist::Dictionary::new();
        peer.insert(
            "Addresses".into(),
            Value::Array(vec![Value::String(self.local_ip.to_string())]),
        );
        peer.insert("ClockID".into(), Value::Integer((clock_id as i64).into()));
        peer.insert("DeviceType".into(), Value::Integer(0u64.into()));
        peer.insert("ID".into(), clock_uuid.to_string().into());
        peer.insert("SupportsClockPortMatchingOverride".into(), Value::Boolean(false));

        let mut dict = plist::Dictionary::new();
        dict.insert("deviceID".into(), self.device_id_mac.clone().into());
        dict.insert("sessionUUID".into(), self.session_uuid.clone().into());
        dict.insert("timingProtocol".into(), "PTP".into());
        dict.insert("timingPeerInfo".into(), Value::Dictionary(peer.clone()));
        dict.insert(
            "timingPeerList".into(),
            Value::Array(vec![Value::Dictionary(peer)]),
        );
        let body = to_binary_plist(&Value::Dictionary(dict))?;

        let uri = self.session_uri();
        let resp = self.request("SETUP", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!("SETUP(timing/PTP) → {} {}", resp.status, resp.status_text);
        }
        Ok(parse_timing_setup(&resp.body))
    }

    /// SETPEERS — hand the receiver the full PTP peer address list (ours
    /// + its own) so it knows who to clock against. Required by some
    /// HomePod firmwares before they'll honour PTP timing.
    pub fn set_peers(&mut self, peers: &[IpAddr]) -> Result<()> {
        let arr: Vec<Value> = peers.iter().map(|ip| Value::String(ip.to_string())).collect();
        let body = to_binary_plist(&Value::Array(arr))?;
        let uri = self.session_uri();
        let resp = self.request("SETPEERS", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!("SETPEERS → {} {}", resp.status, resp.status_text);
        }
        Ok(())
    }

    /// Second SETUP — declare the realtime (type 96, UDP) ALAC audio
    /// stream and ship the 32-byte `shk`. Returns data + control ports.
    pub fn setup_stream(
        &mut self,
        audio_key: &[u8; 32],
        control_port: u16,
        anchor_latency: u32,
    ) -> Result<StreamPorts> {
        self.setup_stream_typed(audio_key, control_port, 0x60, 2, 352, 0x40000, anchor_latency)
    }

    /// Second SETUP, buffered variant — type 103 over TCP, the stream kind
    /// modern iOS senders use (feature bit 40). Codec-parameterized:
    /// AAC-LC = (ct 4, spf 1024, audioFormat 0x400000) — what iOS sends;
    /// ALAC   = (ct 2, spf 352,  audioFormat 0x40000).
    /// Playback is anchored via SETRATEANCHORTIME instead of control-port
    /// sync packets. The returned `data` port is **TCP**.
    pub fn setup_stream_buffered(
        &mut self,
        audio_key: &[u8; 32],
        control_port: u16,
        ct: u64,
        spf: u64,
        audio_format: u64,
        anchor_latency: u32,
    ) -> Result<StreamPorts> {
        self.setup_stream_typed(audio_key, control_port, 0x67, ct, spf, audio_format, anchor_latency)
    }

    fn setup_stream_typed(
        &mut self,
        audio_key: &[u8; 32],
        control_port: u16,
        stream_type: u64,
        ct: u64,
        spf: u64,
        audio_format: u64,
        anchor_latency: u32,
    ) -> Result<StreamPorts> {
        let mut stream = plist::Dictionary::new();
        stream.insert("audioFormat".into(), Value::Integer(audio_format.into()));
        stream.insert("audioMode".into(), "default".into());
        stream.insert("controlPort".into(), Value::Integer((control_port as u64).into()));
        stream.insert("ct".into(), Value::Integer(ct.into()));
        stream.insert("isMedia".into(), Value::Boolean(true));
        // iOS advertises 11025..88200 (0.25-2 s). Widen the window to
        // include the anchor we will actually send in sync packets, so a
        // receiver that clamps to the declared range can't quietly pin a
        // low-latency anchor back to 250 ms (or a deep one to 2 s).
        let latency_min = anchor_latency.min(11025) as u64;
        let latency_max = anchor_latency.max(88200) as u64;
        stream.insert("latencyMax".into(), Value::Integer(latency_max.into()));
        stream.insert("latencyMin".into(), Value::Integer(latency_min.into()));
        stream.insert("shk".into(), Value::Data(audio_key.to_vec()));
        stream.insert("spf".into(), Value::Integer(spf.into()));
        stream.insert("sr".into(), Value::Integer(44100u64.into()));
        stream.insert("type".into(), Value::Integer(stream_type.into()));
        stream.insert("supportsDynamicStreamID".into(), Value::Boolean(false));
        stream.insert("streamConnectionID".into(), Value::Integer((self.session_id as u64).into()));

        let mut dict = plist::Dictionary::new();
        dict.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let body = to_binary_plist(&Value::Dictionary(dict))?;

        let uri = self.session_uri();
        let resp = self.request("SETUP", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!(
                "SETUP(stream type {}) → {} {}{}",
                stream_type,
                resp.status,
                resp.status_text,
                describe_error_body(&resp.body)
            );
        }
        parse_stream_ports(&resp.body)
    }

    /// SETRATEANCHORTIME — anchor the buffered stream: "RTP timestamp
    /// `rtp_time` plays at `network_ns` on the timeline `timeline_id`"
    /// (our PTP grandmaster clock). `rate` 1 = play, 0 = pause. Field
    /// semantics verified against shairport-sync's handler: `networkTimeFrac`
    /// is a 64-bit binary fraction of a second.
    pub fn set_rate_anchor_time(&mut self, rate: u64, rtp_time: u32, network_ns: u64, timeline_id: u64) -> Result<()> {
        let (secs, frac) = network_time_parts(network_ns);
        debug!(
            "AP2 SETRATEANCHORTIME: rate={} rtpTime={} secs={} frac={:#018x} timeline={:#018x}",
            rate, rtp_time, secs, frac, timeline_id
        );
        let mut dict = plist::Dictionary::new();
        dict.insert("rate".into(), Value::Integer(rate.into()));
        dict.insert("rtpTime".into(), Value::Integer((rtp_time as u64).into()));
        dict.insert("networkTimeSecs".into(), Value::Integer(secs.into()));
        dict.insert("networkTimeFrac".into(), Value::Integer(frac.into()));
        // Same signed cast as the SETUP `ClockID`, so both references to
        // our timeline are byte-identical on the wire (the id has bit 63
        // cleared, so the cast is lossless). Real-world receivers disagree
        // on this key's name — shairport-sync reads `networkTimeTimelineID`,
        // goplay2 reads `networkTimeId` — so send both; plist consumers
        // ignore keys they don't read.
        dict.insert("networkTimeTimelineID".into(), Value::Integer((timeline_id as i64).into()));
        dict.insert("networkTimeId".into(), Value::Integer((timeline_id as i64).into()));
        let body = to_binary_plist(&Value::Dictionary(dict))?;
        let uri = self.session_uri();
        let resp = self.request("SETRATEANCHORTIME", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!(
                "SETRATEANCHORTIME → {} {}{}",
                resp.status,
                resp.status_text,
                describe_error_body(&resp.body)
            );
        }
        Ok(())
    }

    /// Send `POST /feedback` — the ~2 s keepalive iOS senders emit — without
    /// waiting for the reply. Returns its CSeq for [`Ap2Rtsp::poll_status`],
    /// so a slow receiver never blocks the connection (volume traffic
    /// shares it) and the request is never re-sent while one is
    /// outstanding.
    pub fn send_feedback(&mut self) -> Result<u32> {
        self.send_request("POST", "/feedback", &[], None, &[])
    }

    /// `POST /feedback`, waiting for the reply like any other request.
    pub fn feedback_status(&mut self) -> Result<u16> {
        Ok(self.request("POST", "/feedback", &[], None, &[])?.status)
    }

    /// Wait up to `wait` for the status of the request sent with `cseq`.
    /// `Ok(None)` = not answered yet. A reply that a later request already
    /// read past (it arrived late) is still reported here.
    pub fn poll_status(&mut self, cseq: u32, wait: Duration) -> Result<Option<u16>> {
        if let Some(st) = self.take_late(cseq) {
            return Ok(Some(st));
        }
        let newest = self.cseq;
        let got = self.await_response(newest, Instant::now() + wait, false)?;
        if newest == cseq {
            return Ok(got.map(|r| r.status));
        }
        if let Some(r) = got {
            self.remember_late(newest, r.status);
        }
        Ok(self.take_late(cseq))
    }

    /// RECORD — flip the receiver to playback. Empty body (matches iOS).
    pub fn record(&mut self) -> Result<()> {
        let uri = self.session_uri();
        let resp = self.request("RECORD", &uri, &[], None, &[])?;
        if resp.status != 200 {
            bail!("RECORD → {} {}", resp.status, resp.status_text);
        }
        Ok(())
    }

    /// SET_PARAMETER volume. RAOP dB semantics: -30.0 … 0.0, -144 = mute.
    pub fn set_volume(&mut self, db: f32) -> Result<()> {
        let body = format!("volume: {:.6}\r\n", db).into_bytes();
        let uri = self.session_uri();
        let resp = self.request("SET_PARAMETER", &uri, &[], Some("text/parameters"), &body)?;
        if resp.status != 200 {
            bail!("SET_PARAMETER(volume) → {} {}", resp.status, resp.status_text);
        }
        Ok(())
    }

    /// TEARDOWN — best-effort close. Idempotent. The request always goes
    /// out (receivers otherwise hold a half-open session); the reply is
    /// waited for only briefly so stopping never stalls on a dead peer.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        let uri = self.session_uri();
        let wait = if self.strict { TEARDOWN_WAIT } else { self.timeout };
        // Sent even on a connection marked unusable: a receiver left with a
        // half-open session refuses new connections for a while.
        self.poisoned = false;
        if let Ok(cseq) = self.send_request("TEARDOWN", &uri, &[], None, &[]) {
            let _ = self.await_response(cseq, Instant::now() + wait, !self.strict);
        }
    }

    // -------------------------------------------------------------------
    // Request / response plumbing
    // -------------------------------------------------------------------

    fn request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(String, String)],
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Resp> {
        let cseq = self.send_request(method, uri, extra, content_type, body)?;
        // Strict: one deadline for the whole reply. Otherwise the timeout
        // applies per read, as it always has.
        match self.await_response(cseq, Instant::now() + self.timeout, !self.strict)? {
            Some(resp) => Ok(resp),
            // The connection stays usable: if the reply turns up later, the
            // next request skips it by CSeq instead of taking it as its own.
            None => bail!("AP2 RTSP: {} {} timed out after {:?}", method, uri, self.timeout),
        }
    }

    fn send_request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(String, String)],
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<u32> {
        if self.poisoned {
            bail!("AP2 RTSP: connection unusable after an earlier failure");
        }
        self.cseq += 1;
        let mut req = String::new();
        req.push_str(&format!("{} {} RTSP/1.0\r\n", method, uri));
        req.push_str(&format!("CSeq: {}\r\n", self.cseq));
        req.push_str(&format!("User-Agent: {}\r\n", USER_AGENT));
        req.push_str(&format!("Client-Instance: {}\r\n", self.client_instance));
        req.push_str(&format!("DACP-ID: {}\r\n", self.client_instance));
        req.push_str(&format!("Active-Remote: {}\r\n", self.active_remote));
        if let Some(ct) = content_type {
            req.push_str(&format!("Content-Type: {}\r\n", ct));
        }
        for (k, v) in extra {
            req.push_str(&format!("{}: {}\r\n", k, v));
        }
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        req.push_str("\r\n");

        let mut raw = req.into_bytes();
        raw.extend_from_slice(body);

        debug!("AP2 RTSP > {} {} (CSeq={}, body={}B, enc={})", method, uri, self.cseq, body.len(), self.writer.is_some());

        let res = match self.writer.as_mut() {
            Some(w) => {
                let framed = w.encrypt(&raw);
                self.stream.write_all(&framed)
            }
            None => self.stream.write_all(&raw),
        }
        .and_then(|_| self.stream.flush());
        if let Err(e) = res {
            self.poison();
            return Err(e.into());
        }
        if self.pending.len() >= PENDING_CAP {
            self.pending.pop_front();
        }
        self.pending.push_back(self.cseq);
        Ok(self.cseq)
    }

    /// Read until the response to `cseq` is complete or `deadline` passes
    /// (`Ok(None)`). Responses to earlier requests are skipped and their
    /// status remembered; a response to a request we never sent poisons
    /// the connection.
    fn await_response(&mut self, cseq: u32, mut deadline: Instant, per_read: bool) -> Result<Option<Resp>> {
        loop {
            let parsed = parse_response(&mut self.rx_buf, self.strict);
            match parsed {
                Err(e) => {
                    self.poison();
                    return Err(e);
                }
                Ok(Some(resp)) if !self.strict => {
                    // Lenient: CSeq values are not trusted (receivers may
                    // echo a constant). A reply is only skipped when it
                    // names — or, lacking a CSeq, falls to — an earlier
                    // request that timed out and is still unanswered.
                    let earlier = match resp.cseq {
                        Some(c) => Some(c).filter(|c| *c != cseq && self.pending.contains(c)),
                        None => self.pending.front().copied().filter(|p| *p != cseq),
                    };
                    if let Some(c) = earlier {
                        self.pending.retain(|p| *p != c);
                        debug!("AP2 RTSP: skipping late response to CSeq {} ({})", c, resp.status);
                        self.remember_late(c, resp.status);
                        continue;
                    }
                    self.pending.retain(|p| *p != cseq);
                    debug!("AP2 RTSP < {} {} ({}B body)", resp.status, resp.status_text, resp.body.len());
                    return Ok(Some(resp));
                }
                Ok(Some(resp)) => {
                    // No CSeq: replies come in order, so it answers the
                    // oldest request still waiting.
                    let c = resp.cseq.or_else(|| self.pending.front().copied()).unwrap_or(cseq);
                    if let Some(i) = self.pending.iter().position(|p| *p == c) {
                        self.pending.remove(i);
                    }
                    if c < cseq {
                        debug!("AP2 RTSP: skipping late response CSeq {} ({})", c, resp.status);
                        self.remember_late(c, resp.status);
                        continue;
                    }
                    if c > cseq && self.strict {
                        self.poison();
                        bail!("AP2 RTSP protocol error: response CSeq {} while awaiting {}", c, cseq);
                    }
                    self.pending.retain(|p| *p != cseq);
                    debug!("AP2 RTSP < {} {} ({}B body)", resp.status, resp.status_text, resp.body.len());
                    return Ok(Some(resp));
                }
                Ok(None) => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if self.fill(deadline - now)? && per_read {
                deadline = Instant::now() + self.timeout;
            }
        }
    }

    /// Read whatever arrives within `wait` into `rx_buf` (decrypting
    /// complete records on the encrypted channel). A timeout is not an
    /// error; partial records stay buffered in the decoder. Returns whether
    /// any bytes arrived.
    fn fill(&mut self, wait: Duration) -> Result<bool> {
        self.stream
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        let mut tmp = [0u8; 4096];
        let n = match self.stream.read(&mut tmp) {
            Ok(0) => {
                self.poison();
                bail!("AP2 RTSP: connection closed");
            }
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(false);
            }
            Err(e) => {
                self.poison();
                return Err(e.into());
            }
        };
        match self.reader.as_mut() {
            Some(r) => match r.push(&tmp[..n]) {
                Ok(plain) => self.rx_buf.extend_from_slice(&plain),
                Err(e) => {
                    self.poison();
                    bail!("AP2 RTSP: {}", e);
                }
            },
            None => self.rx_buf.extend_from_slice(&tmp[..n]),
        }
        Ok(true)
    }

    fn remember_late(&mut self, cseq: u32, status: u16) {
        if self.late.len() >= LATE_STATUS_CAP {
            self.late.pop_front();
        }
        self.late.push_back((cseq, status));
    }

    fn take_late(&mut self, cseq: u32) -> Option<u16> {
        let i = self.late.iter().position(|(c, _)| *c == cseq)?;
        self.late.remove(i).map(|(_, st)| st)
    }
}

impl Drop for Ap2Rtsp {
    /// A session abandoned mid-bring-up (e.g. a SETUP timed out and `?`
    /// unwound) must still TEARDOWN: AirPlay receivers hold the half-open
    /// session for tens of seconds otherwise, and every retry in that
    /// window times out at pairing. Only fires after pairing established
    /// the encrypted channel — before that there's no session to tear
    /// down — and is a no-op when teardown was already sent.
    fn drop(&mut self) {
        if self.writer.is_some() {
            self.teardown();
        }
    }
}

struct Resp {
    status: u16,
    status_text: String,
    cseq: Option<u32>,
    body: Vec<u8>,
}

/// Take one complete RTSP/HTTP response off the front of `buf`, or
/// `Ok(None)` if it isn't complete yet. In `strict` mode bodies over
/// 1 MiB, headers over 16 KiB, chunked transfer, duplicate
/// or unparseable framing headers (`CSeq`, `Content-Length`); otherwise
/// those are read leniently (last value wins, unparseable = absent).
fn parse_response(buf: &mut Vec<u8>, strict: bool) -> Result<Option<Resp>> {
    let header_limit = if strict { MAX_HEADER_BYTES } else { LENIENT_MAX_HEADER_BYTES };
    let Some(header_end) = find_subsequence(buf, b"\r\n\r\n") else {
        if buf.len() > header_limit {
            bail!("AP2 RTSP response headers too large");
        }
        return Ok(None);
    };
    if header_end > header_limit {
        bail!("AP2 RTSP response headers too large");
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().ok_or_else(|| anyhow!("empty RTSP status line"))?;
    let (status, status_text) = parse_status_line(status_line)?;
    let mut content_length: Option<usize> = None;
    let mut cseq: Option<u32> = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "content-length" if strict => {
                if content_length.is_some() {
                    bail!("AP2 RTSP response has duplicate Content-Length");
                }
                content_length = Some(v.parse().context("parsing Content-Length")?);
            }
            "content-length" => content_length = v.parse().ok(),
            "cseq" if strict => {
                if cseq.is_some() {
                    bail!("AP2 RTSP response has duplicate CSeq");
                }
                cseq = Some(v.parse().context("parsing CSeq")?);
            }
            "cseq" => cseq = v.parse().ok(),
            "transfer-encoding" if strict && v.eq_ignore_ascii_case("chunked") => {
                bail!("AP2 RTSP chunked responses are not supported");
            }
            _ => {}
        }
    }
    let content_length = content_length.unwrap_or(0);
    if strict && content_length > MAX_BODY_BYTES {
        bail!("AP2 RTSP response body too large ({} bytes)", content_length);
    }
    let body_start = header_end + 4;
    if buf.len() < body_start.saturating_add(content_length) {
        return Ok(None);
    }
    let body = buf[body_start..body_start + content_length].to_vec();
    buf.drain(..body_start + content_length);
    Ok(Some(Resp { status, status_text, cseq, body }))
}

/// `initialVolume` (dB) from a `GET /info` plist — a real or an integer.
/// Values that aren't finite or lie above 0 dB are rejected as `None`.
fn parse_initial_volume(body: &[u8]) -> Option<f64> {
    let v: Value = plist::from_bytes(body).ok()?;
    let raw = v.as_dictionary()?.get("initialVolume")?;
    let db = raw
        .as_real()
        .or_else(|| raw.as_signed_integer().map(|i| i as f64))?;
    (db.is_finite() && db <= 0.0).then_some(db)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_binary_plist(value: &Value) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, value).context("serialising binary plist")?;
    Ok(buf)
}

/// Pull the receiver's `eventPort` (+ optional `timingPort`) out of the
/// first SETUP response. The receiver withholds its RECORD response until
/// the sender opens a TCP event channel to the event port, so that one is
/// mandatory for AirPlay 2; the timing port is where the receiver expects
/// our NTP-mode timing traffic to originate.
fn parse_timing_setup(body: &[u8]) -> TimingSetup {
    let port = |key: &str| -> u16 {
        plist::from_bytes::<Value>(body)
            .ok()
            .and_then(|v| {
                v.as_dictionary()?
                    .get(key)
                    .and_then(|p| p.as_unsigned_integer())
            })
            .map(|p| p as u16)
            .unwrap_or(0)
    };
    TimingSetup {
        event_port: port("eventPort"),
        timing_port: port("timingPort"),
    }
}

/// Pull `streams[0].dataPort` and `.controlPort` out of a SETUP response.
fn parse_stream_ports(body: &[u8]) -> Result<StreamPorts> {
    let val: Value = plist::from_bytes(body).context("parsing SETUP(stream) plist response")?;
    let dict = val.as_dictionary().ok_or_else(|| anyhow!("SETUP response not a dict"))?;
    let streams = dict
        .get("streams")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("SETUP response missing streams[]"))?;
    let s0 = streams
        .first()
        .and_then(|v| v.as_dictionary())
        .ok_or_else(|| anyhow!("SETUP response streams[0] missing"))?;
    let data = s0
        .get("dataPort")
        .and_then(|v| v.as_unsigned_integer())
        .ok_or_else(|| anyhow!("SETUP response missing dataPort"))? as u16;
    // Some receivers echo controlPort; fall back to data if absent.
    let control = s0
        .get("controlPort")
        .and_then(|v| v.as_unsigned_integer())
        .map(|p| p as u16)
        .unwrap_or(data);
    Ok(StreamPorts { data, control })
}

/// Render a non-200 response body for error messages: receivers often
/// return a plist explaining the refusal. Falls back to a short hex dump.
fn describe_error_body(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    if let Ok(v) = plist::from_bytes::<Value>(body) {
        return format!(" (body: {:?})", v);
    }
    let n = body.len().min(64);
    let hex: String = body[..n].iter().map(|b| format!("{:02x}", b)).collect();
    format!(" (body {}B: {}{})", body.len(), hex, if body.len() > n { "…" } else { "" })
}

/// Split a nanosecond timeline value into SETRATEANCHORTIME's
/// (`networkTimeSecs`, `networkTimeFrac`) pair — frac is a 64-bit binary
/// fraction of a second (shairport-sync consumes it as such).
fn network_time_parts(ns: u64) -> (u64, u64) {
    let secs = ns / 1_000_000_000;
    let rem = ns % 1_000_000_000;
    let frac = (((rem as u128) << 64) / 1_000_000_000) as u64;
    (secs, frac)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_status_line(line: &str) -> Result<(u16, String)> {
    // "RTSP/1.0 200 OK"
    let mut parts = line.splitn(3, ' ');
    let _proto = parts.next();
    let code = parts
        .next()
        .ok_or_else(|| anyhow!("status line missing code"))?
        .parse::<u16>()
        .context("parsing status code")?;
    let text = parts.next().unwrap_or("").to_string();
    Ok((code, text))
}

fn format_uuid(bytes: [u8; 16]) -> String {
    let h: Vec<String> = bytes.iter().map(|b| format!("{:02X}", b)).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_plist_setup_stream_roundtrips() {
        // Build a stream dict like setup_stream and re-parse the ports
        // from a synthetic response to validate plist read/write wiring.
        let mut s0 = plist::Dictionary::new();
        s0.insert("dataPort".into(), Value::Integer(6010u64.into()));
        s0.insert("controlPort".into(), Value::Integer(6011u64.into()));
        s0.insert("type".into(), Value::Integer(0x60u64.into()));
        let mut dict = plist::Dictionary::new();
        dict.insert("streams".into(), Value::Array(vec![Value::Dictionary(s0)]));
        let body = to_binary_plist(&Value::Dictionary(dict)).unwrap();

        let ports = parse_stream_ports(&body).unwrap();
        assert_eq!(ports.data, 6010);
        assert_eq!(ports.control, 6011);
    }

    #[test]
    fn parse_response_waits_for_body_and_keeps_surplus() {
        let mut buf = b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nContent-Length: 3\r\n\r\nab".to_vec();
        assert!(parse_response(&mut buf, true).unwrap().is_none());
        buf.extend_from_slice(b"cRTSP/1.0 503 Busy\r\nCSeq: 5\r\n\r\n");
        let r = parse_response(&mut buf, true).unwrap().unwrap();
        assert_eq!((r.status, r.cseq, r.body.as_slice()), (200, Some(4), &b"abc"[..]));
        let r = parse_response(&mut buf, true).unwrap().unwrap();
        assert_eq!((r.status, r.cseq), (503, Some(5)));
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_response_rejects_ambiguous_framing() {
        for bad in [
            &b"RTSP/1.0 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n"[..],
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nCSeq: 2\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nContent-Length: 99999999\r\n\r\n",
        ] {
            assert!(parse_response(&mut bad.to_vec(), true).is_err());
        }
        let mut huge = vec![b'x'; MAX_HEADER_BYTES + 1];
        assert!(parse_response(&mut huge, true).is_err());
    }

    /// A reply without CSeq that arrives after its request timed out is
    /// attributed to that request, not to the next one.
    #[test]
    fn late_reply_without_cseq_is_not_paired_with_next_request() {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
            let mut out = sock;
            let read_request = |r: &mut std::io::BufReader<TcpStream>| loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            };
            read_request(&mut reader);
            std::thread::sleep(Duration::from_millis(400));
            out.write_all(b"RTSP/1.0 201 Late\r\n\r\n").unwrap();
            read_request(&mut reader);
            out.write_all(b"RTSP/1.0 202 Second\r\n\r\n").unwrap();
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let mut c = Ap2Rtsp::connect(ip, port, ip, Duration::from_millis(200)).unwrap();
        assert!(c.request("OPTIONS", "*", &[], None, &[]).is_err());
        std::thread::sleep(Duration::from_millis(400));
        c.timeout = Duration::from_secs(2);
        let r = c.request("OPTIONS", "*", &[], None, &[]).unwrap();
        assert_eq!(r.status, 202);
        assert_eq!(c.take_late(1), Some(201));
        server.join().unwrap();
    }

    /// Outside strict mode a receiver echoing a constant CSeq keeps
    /// working, and a parse failure doesn't block TEARDOWN.
    #[test]
    fn lenient_mode_ignores_cseq_values() {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
            let mut out = sock;
            let mut methods = Vec::new();
            for reply in [
                &b"RTSP/1.0 200 OK\r\nCSeq: 0\r\n\r\n"[..],
                b"RTSP/1.0 204 OK\r\nCSeq: 0\r\n\r\n",
                b"garbage\r\n\r\n",
            ] {
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                methods.push(first.split(' ').next().unwrap().to_string());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                out.write_all(reply).unwrap();
            }
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            methods.push(first.split(' ').next().unwrap().to_string());
            methods
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let mut c = Ap2Rtsp::connect(ip, port, ip, Duration::from_secs(2)).unwrap();
        assert_eq!(c.request("OPTIONS", "*", &[], None, &[]).unwrap().status, 200);
        assert_eq!(c.request("OPTIONS", "*", &[], None, &[]).unwrap().status, 204);
        assert!(c.request("OPTIONS", "*", &[], None, &[]).is_err());
        c.teardown();
        assert_eq!(server.join().unwrap().last().map(String::as_str), Some("TEARDOWN"));
    }

    #[test]
    fn lenient_parsing_tolerates_odd_framing() {
        let mut buf = b"RTSP/1.0 200 OK\r\nCSeq: x\r\nContent-Length: ?\r\n\r\n".to_vec();
        let r = parse_response(&mut buf, false).unwrap().unwrap();
        assert_eq!((r.status, r.cseq, r.body.len()), (200, None, 0));
        let mut buf = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nCSeq: 2\r\nContent-Length: 1\r\n\r\nz".to_vec();
        let r = parse_response(&mut buf, false).unwrap().unwrap();
        assert_eq!((r.cseq, r.body.as_slice()), (Some(2), &b"z"[..]));
        assert!(parse_response(&mut vec![b'x'; MAX_HEADER_BYTES + 1], false).unwrap().is_none());
    }

    #[test]
    fn initial_volume_real_integer_missing() {
        let body = |v: Option<Value>| {
            let mut d = plist::Dictionary::new();
            if let Some(v) = v {
                d.insert("initialVolume".into(), v);
            }
            to_binary_plist(&Value::Dictionary(d)).unwrap()
        };
        assert_eq!(parse_initial_volume(&body(Some(Value::Real(-12.5)))), Some(-12.5));
        assert_eq!(parse_initial_volume(&body(Some(Value::Integer((-30i64).into())))), Some(-30.0));
        assert_eq!(parse_initial_volume(&body(None)), None);
        assert_eq!(parse_initial_volume(&body(Some(Value::Real(3.0)))), None);
        assert_eq!(parse_initial_volume(b"garbage"), None);
    }

    #[test]
    fn status_line_parsing() {
        assert_eq!(parse_status_line("RTSP/1.0 200 OK").unwrap(), (200, "OK".to_string()));
        assert_eq!(parse_status_line("RTSP/1.0 403 Forbidden").unwrap().0, 403);
    }

    #[test]
    fn uuid_format_is_canonical() {
        let u = format_uuid([0x01; 16]);
        assert_eq!(u, "01010101-0101-0101-0101-010101010101");
    }

    #[test]
    fn describe_error_body_shapes() {
        // Decorates every pairing failure message the user acts on.
        assert_eq!(describe_error_body(&[]), "");
        // A plist body renders as a debug dump.
        let mut dict = plist::Dictionary::new();
        dict.insert("errorCode".into(), Value::Integer((-42i64).into()));
        let body = to_binary_plist(&Value::Dictionary(dict)).unwrap();
        let s = describe_error_body(&body);
        assert!(s.contains("body:"), "got: {}", s);
        assert!(s.contains("errorCode"), "got: {}", s);
        // Garbage falls back to a truncated hex dump.
        let s = describe_error_body(&[0xAB; 100]);
        assert!(s.contains("100B"), "got: {}", s);
        assert!(s.contains(&"ab".repeat(64)), "got: {}", s);
        assert!(s.ends_with("…)"), "got: {}", s);
    }

    #[test]
    fn network_time_parts_is_binary_fraction_of_second() {
        assert_eq!(network_time_parts(0), (0, 0));
        let (s, f) = network_time_parts(1_500_000_000);
        assert_eq!(s, 1);
        // 0.5 s → 0x8000_0000_0000_0000 (allow rounding slack in low bits).
        assert!((f as i128 - 0x8000_0000_0000_0000u64 as i128).abs() < 1_000_000_000);
        let (s2, f2) = network_time_parts(2_000_000_000);
        assert_eq!((s2, f2), (2, 0));
    }
}
