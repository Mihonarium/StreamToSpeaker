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
use log::debug;
use plist::Value;
use rand::Rng;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::airplay::ap2_crypto::{ChannelCipher, SessionKeys, TAG_LEN};
use crate::airplay::pairing::{TransientPairing, X_APPLE_HKP_VALUE};

const USER_AGENT: &str = "AirPlay/665.13.1";

/// Ports the receiver assigned for the audio stream (from the second
/// SETUP response).
#[derive(Debug, Clone, Copy)]
pub struct StreamPorts {
    /// UDP port on the receiver we send audio RTP packets to.
    pub data: u16,
    /// UDP port on the receiver for the control (sync/resend) channel.
    pub control: u16,
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
    /// Outbound/inbound ciphers — None until pairing completes.
    writer: Option<ChannelCipher>,
    reader: Option<ChannelCipher>,
    /// Decrypted (or plaintext) response bytes not yet consumed.
    rx_buf: Vec<u8>,
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
            writer: None,
            reader: None,
            rx_buf: Vec::with_capacity(4096),
        })
    }

    fn session_uri(&self) -> String {
        match self.receiver_ip {
            IpAddr::V4(_) => format!("rtsp://{}/{}", self.local_ip, self.session_id),
            IpAddr::V6(_) => format!("rtsp://[{}]/{}", self.local_ip, self.session_id),
        }
    }

    /// Run HomeKit transient pair-setup. On success the channel becomes
    /// encrypted and the 32-byte audio key is returned for the stream
    /// SETUP `shk`.
    pub fn pair_setup_transient(&mut self) -> Result<[u8; 32]> {
        let mut pairing = TransientPairing::new();

        let hkp = vec![("X-Apple-HKP".to_string(), X_APPLE_HKP_VALUE.to_string())];

        let m1 = pairing.start();
        let r1 = self.request("POST", "/pair-setup", &hkp, Some("application/octet-stream"), &m1)?;
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
        self.writer = Some(keys.control_writer());
        self.reader = Some(keys.control_reader());
        debug!("AirPlay 2: transient pairing complete, channel encrypted");
        Ok(audio_key)
    }

    /// First SETUP — declare the timing protocol and our timing port,
    /// learn the receiver's event port. We use NTP timing (the classic
    /// RAOP timing packets), which works for AP2 receivers that don't
    /// mandate PTP.
    pub fn setup_timing_ntp(&mut self, timing_port: u16) -> Result<()> {
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
        Ok(())
    }

    /// Second SETUP — declare the realtime ALAC audio stream and ship the
    /// 32-byte `shk`. Returns the receiver's data + control ports.
    pub fn setup_stream(&mut self, audio_key: &[u8; 32], control_port: u16) -> Result<StreamPorts> {
        let mut stream = plist::Dictionary::new();
        stream.insert("audioFormat".into(), Value::Integer(0x40000u64.into())); // ALAC 44100/16/2
        stream.insert("audioMode".into(), "default".into());
        stream.insert("controlPort".into(), Value::Integer((control_port as u64).into()));
        stream.insert("ct".into(), Value::Integer(2u64.into())); // ALAC
        stream.insert("isMedia".into(), Value::Boolean(true));
        stream.insert("latencyMax".into(), Value::Integer(88200u64.into()));
        stream.insert("latencyMin".into(), Value::Integer(11025u64.into()));
        stream.insert("shk".into(), Value::Data(audio_key.to_vec()));
        stream.insert("spf".into(), Value::Integer(352u64.into())); // samples/packet
        stream.insert("sr".into(), Value::Integer(44100u64.into()));
        stream.insert("type".into(), Value::Integer(0x60u64.into())); // 96 realtime
        stream.insert("supportsDynamicStreamID".into(), Value::Boolean(false));
        stream.insert("streamConnectionID".into(), Value::Integer((self.session_id as u64).into()));

        let mut dict = plist::Dictionary::new();
        dict.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let body = to_binary_plist(&Value::Dictionary(dict))?;

        let uri = self.session_uri();
        let resp = self.request("SETUP", &uri, &[], Some("application/x-apple-binary-plist"), &body)?;
        if resp.status != 200 {
            bail!("SETUP(stream) → {} {}", resp.status, resp.status_text);
        }
        parse_stream_ports(&resp.body)
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

    /// TEARDOWN — best-effort close.
    pub fn teardown(&mut self) {
        let uri = self.session_uri();
        let _ = self.request("TEARDOWN", &uri, &[], None, &[]);
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

        match self.writer.as_mut() {
            Some(w) => {
                let framed = w.encrypt(&raw);
                self.stream.write_all(&framed)?;
            }
            None => self.stream.write_all(&raw)?,
        }
        self.stream.flush()?;
        self.read_response()
    }

    /// Pull more bytes into `rx_buf`. On the encrypted channel this reads
    /// exactly one HAP block (2-byte length + ciphertext + tag) and
    /// decrypts it; on the plaintext channel it reads whatever's available.
    fn fill(&mut self) -> Result<()> {
        if self.reader.is_some() {
            let mut len_buf = [0u8; 2];
            self.stream.read_exact(&mut len_buf)?;
            let block_len = u16::from_le_bytes(len_buf);
            let mut block = vec![0u8; block_len as usize + TAG_LEN];
            self.stream.read_exact(&mut block)?;
            let plain = self
                .reader
                .as_mut()
                .unwrap()
                .decrypt_block(block_len, &block)?;
            self.rx_buf.extend_from_slice(&plain);
        } else {
            let mut tmp = [0u8; 2048];
            let n = self.stream.read(&mut tmp)?;
            if n == 0 {
                bail!("AP2 RTSP: connection closed");
            }
            self.rx_buf.extend_from_slice(&tmp[..n]);
        }
        Ok(())
    }

    fn read_response(&mut self) -> Result<Resp> {
        // Read until we have the full header block.
        let header_end = loop {
            if let Some(pos) = find_subsequence(&self.rx_buf, b"\r\n\r\n") {
                break pos;
            }
            if self.rx_buf.len() > 1 << 20 {
                bail!("AP2 RTSP response headers too large");
            }
            self.fill()?;
        };

        let head = String::from_utf8_lossy(&self.rx_buf[..header_end]).to_string();
        let mut lines = head.lines();
        let status_line = lines.next().ok_or_else(|| anyhow!("empty RTSP status line"))?;
        let (status, status_text) = parse_status_line(status_line)?;
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

        let body_start = header_end + 4;
        while self.rx_buf.len() < body_start + content_length {
            self.fill()?;
        }
        let body = self.rx_buf[body_start..body_start + content_length].to_vec();
        // Drop the consumed bytes (responses shouldn't be pipelined, but
        // keep any surplus just in case).
        self.rx_buf.drain(..body_start + content_length);

        debug!("AP2 RTSP < {} {} ({}B body)", status, status_text, body.len());
        Ok(Resp { status, status_text, body })
    }
}

struct Resp {
    status: u16,
    status_text: String,
    body: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_binary_plist(value: &Value) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, value).context("serialising binary plist")?;
    Ok(buf)
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
    fn status_line_parsing() {
        assert_eq!(parse_status_line("RTSP/1.0 200 OK").unwrap(), (200, "OK".to_string()));
        assert_eq!(parse_status_line("RTSP/1.0 403 Forbidden").unwrap().0, 403);
    }

    #[test]
    fn uuid_format_is_canonical() {
        let u = format_uuid([0x01; 16]);
        assert_eq!(u, "01010101-0101-0101-0101-010101010101");
    }
}
