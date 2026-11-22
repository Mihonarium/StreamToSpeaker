//! What each kind of AirPlay 1 receiver is sent on a connect that works,
//! request by request and byte by byte, recorded against the scripted
//! receiver in [`crate::airplay::fake_raop`].
//!
//! These transcripts are what the app sends today to receivers that
//! connect. Nobody can try a change on every kind of real receiver, so a
//! change that alters one of them must be deliberate: the test fails, and
//! the transcript is updated only with a reason.
//!
//! Values that are random per session (session id, client identifiers,
//! challenges, keys, RTP start values, digests that cover the session
//! URI) are replaced by placeholders; a placeholder for an identifier
//! stands for one value used consistently across the whole transcript.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::airplay::discovery::{AirPlayRenderer, FEAT_AUDIO, FEAT_TRANSIENT_PAIRING};
use crate::airplay::fake_raop::{Answer, Event, FakeRaop, Reply, Request};
use crate::airplay::session::{AirPlaySession, AirPlaySessionConfig};

/// A receiver record pointing at `rx`.
pub(crate) fn receiver(rx: &FakeRaop, mac: &str, et: &[u8]) -> AirPlayRenderer {
    AirPlayRenderer {
        friendly_name: "Test Speaker".into(),
        mac_id: mac.into(),
        ip: "127.0.0.1".parse().unwrap(),
        port: rx.port(),
        airplay_port: None,
        encryption_types: et.to_vec(),
        codecs: vec![0, 1],
        password_protected: false,
        encryption_key_required: false,
        features: None,
        pk: None,
        model: Some("Speaker1,1".into()),
        group: None,
        pair: None,
    }
}

/// The session settings the app uses, for `renderer`.
pub(crate) fn config(renderer: AirPlayRenderer, samples: crossbeam_channel::Receiver<crate::http_server::PcmFrame>) -> AirPlaySessionConfig {
    AirPlaySessionConfig {
        renderer,
        local_ip: "127.0.0.1".parse::<IpAddr>().unwrap(),
        samples_rx: samples,
        initial_volume: Some(80),
        connect_timeout: Duration::from_secs(3),
        mfi_encryption: false,
        uncompressed_alac: false,
        password: None,
        latency_ms: 2000,
    }
}

/// Connect, stop at once, and return what the receiver was sent.
fn connect_and_stop(rx: &FakeRaop, cfg: AirPlaySessionConfig) -> String {
    let session = AirPlaySession::start(cfg).expect("the session starts");
    session.stop();
    // Everything went on the first connection, which is closed now.
    assert!(
        rx.wait_until(Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Event::Closed { conn: 0, .. }))),
        "the connection stays open after stop"
    );
    transcript(&rx.requests())
}

/// Placeholders for the values one session picks at random.
#[derive(Default)]
struct Learned {
    values: Vec<(String, String)>,
}

impl Learned {
    /// The placeholder for `value` of kind `name`, the same one every
    /// time; a second value of the same kind gets a numbered one.
    fn name(&mut self, name: &str, value: &str) -> String {
        if let Some((_, p)) = self.values.iter().find(|(v, p)| v == value && p.starts_with(&format!("<{}", name))) {
            return p.clone();
        }
        let n = self.values.iter().filter(|(_, p)| p.starts_with(&format!("<{}", name))).count();
        let p = if n == 0 { format!("<{}>", name) } else { format!("<{}#{}>", name, n + 1) };
        self.values.push((value.to_string(), p.clone()));
        p
    }
}

fn session_id_of(uri: &str) -> Option<&str> {
    uri.strip_prefix("rtsp://127.0.0.1/")
}

/// The requests, as text with the random values replaced.
pub(crate) fn transcript(reqs: &[Request]) -> String {
    let mut learned = Learned::default();
    let mut out = String::new();
    for r in reqs {
        out.push_str(&format!("--- connection {}\n", r.conn));
        let sid = reqs.iter().find_map(|q| session_id_of(&q.uri)).map(str::to_string);
        let with_sid = |s: &str, learned: &mut Learned| match &sid {
            Some(sid) if s.contains(sid.as_str()) => s.replace(sid.as_str(), &learned.name("session", sid)),
            _ => s.to_string(),
        };
        for (i, line) in r.head.split("\r\n").enumerate() {
            if line.is_empty() {
                continue;
            }
            if i == 0 {
                out.push_str(&with_sid(line, &mut learned));
                out.push('\n');
                continue;
            }
            let (k, v) = line.split_once(": ").unwrap_or((line, ""));
            let v = match k {
                "Client-Instance" | "DACP-ID" | "Active-Remote" => learned.name(k, v),
                "Apple-Challenge" => {
                    assert_eq!(v.len(), 22, "a 16-byte challenge in unpadded base64");
                    "<challenge>".to_string()
                }
                "RTP-Info" | "Transport" => {
                    let mut parts = Vec::new();
                    for kv in v.split(';') {
                        match kv.split_once('=') {
                            Some((pk, pv)) if ["seq", "rtptime", "control_port", "timing_port"].contains(&pk) => {
                                parts.push(format!("{}={}", pk, learned.name(pk, pv)))
                            }
                            _ => parts.push(kv.to_string()),
                        }
                    }
                    parts.join(";")
                }
                "Authorization" => {
                    let v = with_sid(v, &mut learned);
                    match v.split_once("response=\"") {
                        Some((before, rest)) => {
                            let digest = rest.trim_end_matches('"');
                            assert_eq!(digest.len(), 32, "an MD5 digest in hex");
                            format!("{}response=\"<digest>\"", before)
                        }
                        None => v,
                    }
                }
                "Content-Length" => {
                    assert_eq!(v.parse::<usize>().ok(), Some(r.body.len()), "Content-Length matches the body");
                    "<length of the body>".to_string()
                }
                _ => v.to_string(),
            };
            out.push_str(&format!("{}: {}\n", k, v));
        }
        if r.method == "POST" && r.uri == "/auth-setup" {
            assert_eq!(r.body.len(), 33);
            assert_eq!(r.body[0], 0x01);
            out.push_str("[0x01, then a 32-byte X25519 public key]\n");
        } else if r.body.iter().any(|&b| !(b.is_ascii_graphic() || b == b' ' || b == b'\r' || b == b'\n')) {
            let hex: Vec<String> = r.body.iter().map(|b| format!("{:02x}", b)).collect();
            out.push_str(&format!("  [bytes {}]\n", hex.join(" ")));
        } else if !r.body.is_empty() {
            let text = std::str::from_utf8(&r.body).expect("a text body");
            for line in text.split("\r\n") {
                if line.is_empty() {
                    continue;
                }
                let line = with_sid(line, &mut learned);
                let line = match line.split_once(':') {
                    Some((k @ ("a=rsaaeskey" | "a=mfiaeskey" | "a=aesiv"), b64)) => format!("{}:<{} base64 characters>", k, b64.len()),
                    _ => line,
                };
                out.push_str(&format!("  {}\n", line));
            }
        }
    }
    out
}

fn samples() -> (crossbeam_channel::Sender<crate::http_server::PcmFrame>, crossbeam_channel::Receiver<crate::http_server::PcmFrame>) {
    crossbeam_channel::unbounded()
}

const PLAIN: &str = r#"--- connection 0
OPTIONS * RTSP/1.0
CSeq: 1
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Apple-Challenge: <challenge>
--- connection 0
ANNOUNCE rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 2
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Content-Type: application/sdp
Content-Length: <length of the body>
  v=0
  o=iTunes <session> 0 IN IP4 127.0.0.1
  s=iTunes
  c=IN IP4 127.0.0.1
  t=0 0
  m=audio 0 RTP/AVP 96
  a=rtpmap:96 AppleLossless
  a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
--- connection 0
SETUP rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 3
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Transport: RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<control_port>;timing_port=<timing_port>
--- connection 0
RECORD rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 4
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Range: npt=0-
RTP-Info: seq=<seq>;rtptime=<rtptime>
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 5
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -6.060606
--- connection 0
TEARDOWN rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 6
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
"#;

/// A receiver with no encryption and no AirPlay 2: the `OPTIONS` opener.
#[test]
fn plain_receiver() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let cfg = config(receiver(&rx, "0A0000000001", &[0, 1]), srx);
    assert_eq!(connect_and_stop(&rx, cfg), PLAIN);
}

const AUTH_SETUP: &str = r#"--- connection 0
POST /auth-setup RTSP/1.0
CSeq: 1
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Content-Type: application/octet-stream
Content-Length: <length of the body>
[0x01, then a 32-byte X25519 public key]
--- connection 0
ANNOUNCE rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 2
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Content-Type: application/sdp
Content-Length: <length of the body>
  v=0
  o=iTunes <session> 0 IN IP4 127.0.0.1
  s=iTunes
  c=IN IP4 127.0.0.1
  t=0 0
  m=audio 0 RTP/AVP 96
  a=rtpmap:96 AppleLossless
  a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
--- connection 0
SETUP rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 3
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Transport: RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<control_port>;timing_port=<timing_port>
--- connection 0
RECORD rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 4
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Range: npt=0-
RTP-Info: seq=<seq>;rtptime=<rtptime>
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 5
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -6.060606
--- connection 0
TEARDOWN rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 6
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
"#;

/// A receiver whose `et` lists 4 (MFi), no AirPlay 2, `/auth-setup`
/// answered.
#[test]
fn mfi_receiver_with_auth_setup() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let cfg = config(receiver(&rx, "0A0000000002", &[0, 4]), srx);
    assert_eq!(connect_and_stop(&rx, cfg), AUTH_SETUP);
}

/// A Sonos-class receiver: `et=0,4` and an AirPlay 2 service.
#[test]
fn receiver_that_also_offers_airplay_2() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let mut r = receiver(&rx, "0A0000000003", &[0, 4]);
    r.features = Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING);
    r.airplay_port = Some(7000);
    let cfg = config(r, srx);
    assert_eq!(connect_and_stop(&rx, cfg), AUTH_SETUP);
}

/// An Apple-TV-class receiver: `et=0,1` and an AirPlay 2 service. It is
/// opened with `/auth-setup` too.
#[test]
fn plain_receiver_that_also_offers_airplay_2() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let mut r = receiver(&rx, "0A0000000004", &[0, 1]);
    r.features = Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING);
    r.airplay_port = Some(7000);
    let cfg = config(r, srx);
    assert_eq!(connect_and_stop(&rx, cfg), AUTH_SETUP);
}

const RSA: &str = r#"--- connection 0
OPTIONS * RTSP/1.0
CSeq: 1
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Apple-Challenge: <challenge>
--- connection 0
ANNOUNCE rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 2
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Content-Type: application/sdp
Content-Length: <length of the body>
  v=0
  o=iTunes <session> 0 IN IP4 127.0.0.1
  s=iTunes
  c=IN IP4 127.0.0.1
  t=0 0
  m=audio 0 RTP/AVP 96
  a=rtpmap:96 AppleLossless
  a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
  a=rsaaeskey:<342 base64 characters>
  a=aesiv:<22 base64 characters>
--- connection 0
SETUP rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 3
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Transport: RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<control_port>;timing_port=<timing_port>
--- connection 0
RECORD rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 4
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Range: npt=0-
RTP-Info: seq=<seq>;rtptime=<rtptime>
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 5
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -6.060606
--- connection 0
TEARDOWN rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 6
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
"#;

/// A classic AirPort Express: RSA-wrapped key.
#[test]
fn airport_express_with_rsa() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let mut r = receiver(&rx, "0A0000000005", &[0, 1]);
    r.model = Some("AirPort4,107".into());
    let cfg = config(r, srx);
    assert_eq!(connect_and_stop(&rx, cfg), RSA);
}

const PASSWORD: &str = r#"--- connection 0
OPTIONS * RTSP/1.0
CSeq: 1
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Apple-Challenge: <challenge>
--- connection 0
OPTIONS * RTSP/1.0
CSeq: 2
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Authorization: Digest username="", realm="raop", nonce="8f2a", uri="*", response="<digest>"
Apple-Challenge: <challenge>
--- connection 0
ANNOUNCE rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 3
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Authorization: Digest username="", realm="raop", nonce="8f2a", uri="rtsp://127.0.0.1/<session>", response="<digest>"
Content-Type: application/sdp
Content-Length: <length of the body>
  v=0
  o=iTunes <session> 0 IN IP4 127.0.0.1
  s=iTunes
  c=IN IP4 127.0.0.1
  t=0 0
  m=audio 0 RTP/AVP 96
  a=rtpmap:96 AppleLossless
  a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
--- connection 0
SETUP rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 4
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Authorization: Digest username="", realm="raop", nonce="8f2a", uri="rtsp://127.0.0.1/<session>", response="<digest>"
Transport: RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<control_port>;timing_port=<timing_port>
--- connection 0
RECORD rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 5
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Authorization: Digest username="", realm="raop", nonce="8f2a", uri="rtsp://127.0.0.1/<session>", response="<digest>"
Session: 1
Range: npt=0-
RTP-Info: seq=<seq>;rtptime=<rtptime>
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 6
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Authorization: Digest username="", realm="raop", nonce="8f2a", uri="rtsp://127.0.0.1/<session>", response="<digest>"
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -6.060606
--- connection 0
TEARDOWN rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 7
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Authorization: Digest username="", realm="raop", nonce="8f2a", uri="rtsp://127.0.0.1/<session>", response="<digest>"
Session: 1
"#;

/// A password-protected receiver: a 401 with a Digest challenge, then
/// every request signed.
#[test]
fn password_protected_receiver() {
    let rx = FakeRaop::start(|req| match req.header("Authorization") {
        None => Answer::Reply(Reply::status(401, "Unauthorized").header("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"8f2a\"")),
        Some(_) => Answer::Normal,
    });
    let (_tx, srx) = samples();
    let mut r = receiver(&rx, "0A0000000006", &[0, 1]);
    r.password_protected = true;
    let mut cfg = config(r, srx);
    cfg.password = Some("hunter2".into());
    assert_eq!(connect_and_stop(&rx, cfg), PASSWORD);
}

const MFI_EXPERIMENT: &str = r#"--- connection 0
POST /auth-setup RTSP/1.0
CSeq: 1
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Content-Type: application/octet-stream
Content-Length: <length of the body>
[0x01, then a 32-byte X25519 public key]
--- connection 0
ANNOUNCE rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 2
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Content-Type: application/sdp
Content-Length: <length of the body>
  v=0
  o=iTunes <session> 0 IN IP4 127.0.0.1
  s=iTunes
  c=IN IP4 127.0.0.1
  t=0 0
  m=audio 0 RTP/AVP 96
  a=rtpmap:96 AppleLossless
  a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100
  a=mfiaeskey:<22 base64 characters>
  a=aesiv:<22 base64 characters>
--- connection 0
SETUP rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 3
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Transport: RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<control_port>;timing_port=<timing_port>
--- connection 0
RECORD rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 4
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Range: npt=0-
RTP-Info: seq=<seq>;rtptime=<rtptime>
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 5
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -6.060606
--- connection 0
TEARDOWN rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 6
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
"#;

/// The opt-in MFi encryption experiment on an `et=0,4` receiver.
#[test]
fn mfi_encryption_experiment() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let mut cfg = config(receiver(&rx, "0A0000000007", &[0, 4]), srx);
    cfg.mfi_encryption = true;
    assert_eq!(connect_and_stop(&rx, cfg), MFI_EXPERIMENT);
}

/// A receiver that also offers AirPlay 2 and never answers the opener
/// (a Sonos whose AirPlay 1 service is vestigial) is given up on after
/// the opener's short time limit, with nothing else sent, so the app can
/// move on to AirPlay 2.
#[test]
fn receiver_that_never_answers_the_opener_is_given_up_quickly() {
    let rx = FakeRaop::start(|_| Answer::Ignore);
    let (_tx, srx) = samples();
    let mut r = receiver(&rx, "0A0000000008", &[0, 4]);
    r.features = Some(FEAT_AUDIO | FEAT_TRANSIENT_PAIRING);
    r.airplay_port = Some(7000);
    let mut cfg = config(r, srx);
    let limit = Duration::from_millis(300);
    cfg.connect_timeout = limit;
    let started = Instant::now();
    assert!(AirPlaySession::start(cfg).is_err());
    let took = started.elapsed();
    assert!(took >= limit && took < limit * 10, "{:?}", took);
    assert_eq!(rx.lines(), ["0 POST /auth-setup"]);
    assert!(rx.wait_until(Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Event::Closed { conn: 0, .. }))));
}

const KEEPALIVE_METADATA_VOLUME: &str = r#"--- connection 0
OPTIONS * RTSP/1.0
CSeq: 6
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 7
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
RTP-Info: rtptime=<rtptime>
Content-Type: application/x-dmap-tagged
Content-Length: <length of the body>
  [bytes 6d 6c 69 74 00 00 00 27 6d 69 6e 6d 00 00 00 04 53 6f 6e 67 61 73 61 72 00 00 00 06 41 72 74 69 73 74 61 73 61 6c 00 00 00 05 41 6c 62 75 6d]
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 8
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -18.181818
--- connection 0
SET_PARAMETER rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 9
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
Content-Type: text/parameters
Content-Length: <length of the body>
  volume: -144.000000
--- connection 0
TEARDOWN rtsp://127.0.0.1/<session> RTSP/1.0
CSeq: 10
User-Agent: iTunes/12.13.10 (Windows; Microsoft Windows 11 x64 (Build 26200); x64) (dt:2)
Client-Instance: <Client-Instance>
DACP-ID: <DACP-ID>
Active-Remote: <Active-Remote>
Session: 1
"#;

/// What a session sends after it is up: the keepalive, track text and a
/// volume change, then TEARDOWN.
#[test]
fn keepalive_track_text_and_volume() {
    let rx = FakeRaop::start(|_| Answer::Normal);
    let (_tx, srx) = samples();
    let cfg = config(receiver(&rx, "0A0000000009", &[0, 1]), srx);
    let session = AirPlaySession::start(cfg).expect("the session starts");
    // The first keepalive goes out about 2 s after the session is up.
    assert!(rx.wait_for_requests(6, Duration::from_secs(20)), "{:?}", rx.lines());
    session.metadata_handle().send("Song", "Artist", "Album").expect("track text answered");
    session.set_volume_pct(40).expect("volume answered");
    session.set_mute(true).expect("mute answered");
    session.stop();
    assert!(rx.wait_until(Duration::from_secs(10), |ev| ev.iter().any(|e| matches!(e, Event::Closed { conn: 0, .. }))));
    let reqs = rx.requests();
    // Keep the transcript to the requests whose order doesn't depend on
    // timing: the first keepalive, and everything from the track text on.
    let keepalive = reqs.iter().position(|r| r.method == "OPTIONS" && r.cseq() == Some(6)).expect("a keepalive");
    let text = reqs.iter().position(|r| r.header("Content-Type") == Some("application/x-dmap-tagged")).unwrap();
    let mut picked = vec![reqs[keepalive].clone()];
    picked.extend(reqs[text..].iter().cloned());
    picked.retain(|r| !(r.method == "OPTIONS" && r.cseq() != Some(6)));
    assert_eq!(transcript(&picked), KEEPALIVE_METADATA_VOLUME);
}
