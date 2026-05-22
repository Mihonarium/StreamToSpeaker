//! GENA — Generic Event Notification Architecture — subscriber.
//!
//! We SUBSCRIBE to the renderer's RenderingControl event URL and ask it to
//! POST NOTIFY messages back to our own HTTP server.  When notifications
//! arrive (delivered via the callback in `http_server.rs`), the body looks
//! like:
//!
//! ```xml
//! <e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
//!   <e:property>
//!     <LastChange>&lt;Event ...&gt;&lt;/Event&gt;</LastChange>
//!   </e:property>
//! </e:propertyset>
//! ```
//!
//! The `<LastChange>` payload is itself an XML document (entity-encoded);
//! we decode it and pull out Volume / Mute changes.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A simple Volume / Mute change decoded from a LastChange notification.
#[derive(Debug, Clone)]
pub struct RenderingChange {
    pub volume: Option<u32>,
    pub mute: Option<bool>,
}

/// Holds a live subscription's identifier so it can be RENEWed.
#[derive(Debug, Clone)]
pub struct SubscriptionState {
    pub sid: String,
    pub timeout_secs: u64,
    pub event_url: String,
    /// Wall-clock instant when we last renewed; the renewer thread uses
    /// this to schedule the next renew.
    pub last_renew: Instant,
}

/// Manager that keeps a subscription alive in the background and parses
/// incoming notifies.
pub struct GenaManager {
    state: Mutex<Option<SubscriptionState>>,
    /// Public callback URL the renderer should call back on.
    callback_url: String,
}

impl GenaManager {
    pub fn new(callback_url: String) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(None),
            callback_url,
        })
    }

    /// SUBSCRIBE to `event_url`. Returns the SID assigned by the renderer.
    pub fn subscribe(&self, event_url: &str) -> Result<String> {
        let (sid, timeout) = http_subscribe(event_url, &self.callback_url, 1800)?;
        let st = SubscriptionState {
            sid: sid.clone(),
            timeout_secs: timeout,
            event_url: event_url.to_string(),
            last_renew: Instant::now(),
        };
        *self.state.lock().unwrap() = Some(st);
        info!("GENA subscribed: sid={} timeout={}s", sid, timeout);
        Ok(sid)
    }

    /// Background renewer loop. Renews well before the timeout expires.
    pub fn spawn_renewer(self: Arc<Self>) {
        thread::Builder::new()
            .name("stream-to-speaker-gena-renew".to_string())
            .spawn(move || loop {
                let to_sleep = {
                    let st = self.state.lock().unwrap();
                    if let Some(s) = st.as_ref() {
                        // Renew at 75% of timeout, min 60s.
                        let renew_in = (s.timeout_secs * 3 / 4).max(60);
                        Duration::from_secs(renew_in)
                    } else {
                        Duration::from_secs(60)
                    }
                };
                thread::sleep(to_sleep);

                let snapshot = self.state.lock().unwrap().clone();
                if let Some(s) = snapshot {
                    match http_renew(&s.event_url, &s.sid, 1800) {
                        Ok(timeout) => {
                            let mut st = self.state.lock().unwrap();
                            if let Some(s) = st.as_mut() {
                                s.timeout_secs = timeout;
                                s.last_renew = Instant::now();
                            }
                            debug!("GENA renewed sid={} timeout={}s", s.sid, timeout);
                        }
                        Err(e) => {
                            warn!("GENA renew failed: {}; re-subscribing", e);
                            let _ = self.subscribe(&s.event_url);
                        }
                    }
                }
            })
            .expect("spawning GENA renewer");
    }

    /// UNSUBSCRIBE on shutdown.  Best-effort.
    pub fn unsubscribe(&self) {
        let st = self.state.lock().unwrap().clone();
        if let Some(s) = st {
            let _ = http_unsubscribe(&s.event_url, &s.sid);
        }
    }
}

/// Parse a NOTIFY body for RenderingControl LastChange events. Returns
/// None if we can't extract anything useful (which is fine — we'll just
/// ignore the notify).
pub fn parse_rendering_notify(body: &str) -> Option<RenderingChange> {
    // The body is propertyset -> property -> LastChange (text node,
    // entity-encoded).  Extract LastChange.
    let lc = find_tag_text(body, "LastChange")?;
    // LastChange is XML-encoded inside the text node.
    let decoded = xml_unescape(&lc);

    // Decoded looks like:
    // <Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT_RCS">
    //   <InstanceID val="0">
    //     <Volume channel="Master" val="42"/>
    //     <Mute channel="Master" val="0"/>
    //   </InstanceID>
    // </Event>
    let mut volume: Option<u32> = None;
    let mut mute: Option<bool> = None;

    if let Ok(doc) = roxmltree::Document::parse(&decoded) {
        for node in doc.descendants() {
            let name = node.tag_name().name();
            if name == "Volume" {
                // Prefer Master channel; if no channel attr at all, accept.
                let channel = node.attribute("channel").unwrap_or("Master");
                if channel.eq_ignore_ascii_case("Master") {
                    if let Some(v) = node.attribute("val").and_then(|s| s.parse::<u32>().ok()) {
                        volume = Some(v);
                    }
                }
            } else if name == "Mute" {
                let channel = node.attribute("channel").unwrap_or("Master");
                if channel.eq_ignore_ascii_case("Master") {
                    if let Some(v) = node.attribute("val") {
                        mute = Some(v == "1" || v.eq_ignore_ascii_case("true"));
                    }
                }
            }
        }
    }

    if volume.is_none() && mute.is_none() {
        None
    } else {
        Some(RenderingChange { volume, mute })
    }
}

// -----------------------------------------------------------------------------
// HTTP-level helpers
// -----------------------------------------------------------------------------

fn http_subscribe(event_url: &str, callback_url: &str, timeout_secs: u64) -> Result<(String, u64)> {
    let resp = do_request(
        event_url,
        "SUBSCRIBE",
        &[
            ("CALLBACK", format!("<{}>", callback_url)),
            ("NT", "upnp:event".to_string()),
            ("TIMEOUT", format!("Second-{}", timeout_secs)),
        ],
        "",
    )?;
    let sid = extract_header(&resp, "SID").ok_or_else(|| anyhow!("SID missing in subscribe response"))?;
    let to = extract_header(&resp, "TIMEOUT")
        .and_then(|t| {
            // Format: "Second-1800" or "Second-infinite".
            t.strip_prefix("Second-")
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(timeout_secs);
    Ok((sid, to))
}

fn http_renew(event_url: &str, sid: &str, timeout_secs: u64) -> Result<u64> {
    let resp = do_request(
        event_url,
        "SUBSCRIBE",
        &[
            ("SID", sid.to_string()),
            ("TIMEOUT", format!("Second-{}", timeout_secs)),
        ],
        "",
    )?;
    let to = extract_header(&resp, "TIMEOUT")
        .and_then(|t| t.strip_prefix("Second-").and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(timeout_secs);
    Ok(to)
}

fn http_unsubscribe(event_url: &str, sid: &str) -> Result<()> {
    let _ = do_request(
        event_url,
        "UNSUBSCRIBE",
        &[("SID", sid.to_string())],
        "",
    )?;
    Ok(())
}

fn do_request(
    url_str: &str,
    method: &str,
    extra_headers: &[(&str, String)],
    body: &str,
) -> Result<String> {
    let url = url::Url::parse(url_str).with_context(|| format!("parse {}", url_str))?;
    let host = url.host_str().ok_or_else(|| anyhow!("no host in {}", url_str))?;
    let port = url.port().unwrap_or(80);
    let path = if url.path().is_empty() { "/" } else { url.path() };
    let path_q = if let Some(q) = url.query() {
        format!("{}?{}", path, q)
    } else {
        path.to_string()
    };
    let addr: SocketAddr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("no addrs"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let mut req = String::new();
    req.push_str(&format!("{} {} HTTP/1.1\r\n", method, path_q));
    req.push_str(&format!("Host: {}:{}\r\n", host, port));
    for (k, v) in extra_headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("Connection: close\r\n");
    req.push_str("User-Agent: stream-to-speaker/0.1\r\n");
    req.push_str("\r\n");
    req.push_str(body);

    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut buf = Vec::with_capacity(2048);
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();

    let status_line = text.lines().next().unwrap_or("");
    if !status_line.contains(" 200 ") {
        return Err(anyhow!("{} {} failed: {}", method, url_str, status_line));
    }
    Ok(text)
}

fn extract_header(response: &str, header: &str) -> Option<String> {
    let key = format!("{}:", header.to_ascii_uppercase());
    for line in response.lines() {
        let upper: String = line.chars().take(key.len()).map(|c| c.to_ascii_uppercase()).collect();
        if upper == key {
            return Some(line[key.len()..].trim().to_string());
        }
    }
    None
}

/// Pull the text between the first `<tag>...</tag>` pair.
fn find_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = xml.find(&open)?;
    let after = &xml[i + open.len()..];
    let j = after.find(&close)?;
    Some(after[..j].to_string())
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if s[i..].starts_with("&amp;") {
                out.push('&');
                i += 5;
                continue;
            } else if s[i..].starts_with("&lt;") {
                out.push('<');
                i += 4;
                continue;
            } else if s[i..].starts_with("&gt;") {
                out.push('>');
                i += 4;
                continue;
            } else if s[i..].starts_with("&quot;") {
                out.push('"');
                i += 6;
                continue;
            } else if s[i..].starts_with("&apos;") {
                out.push('\'');
                i += 6;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_volume_change() {
        let body = r#"<?xml version="1.0"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
<e:property>
<LastChange>&lt;Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT_RCS"&gt;&lt;InstanceID val="0"&gt;&lt;Volume channel="Master" val="33"/&gt;&lt;Mute channel="Master" val="0"/&gt;&lt;/InstanceID&gt;&lt;/Event&gt;</LastChange>
</e:property>
</e:propertyset>"#;
        let rc = parse_rendering_notify(body).expect("decode");
        assert_eq!(rc.volume, Some(33));
        assert_eq!(rc.mute, Some(false));
    }

    #[test]
    fn xml_unescape_basic() {
        assert_eq!(xml_unescape("a &amp; b &lt;c&gt; &quot;d&quot;"), "a & b <c> \"d\"");
    }
}
