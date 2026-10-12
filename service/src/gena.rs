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
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::stream_gate::canonical_ip;

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

// -----------------------------------------------------------------------------
// Live-subscription registry
// -----------------------------------------------------------------------------

/// How an incoming NOTIFY relates to the subscriptions we hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyAdmission {
    /// SID and source address match a live subscription.
    Live,
    /// Unknown SID, but a SUBSCRIBE to this address is in flight — the
    /// publisher's initial event can race our reading of the SID from
    /// the SUBSCRIBE response. Worth waiting briefly for.
    Pending,
    /// Neither: not one of ours.
    Unknown,
}

#[derive(Default)]
struct RegistryInner {
    /// (SID, publisher addresses) of every live subscription.
    live: Vec<(String, Vec<IpAddr>)>,
    /// (token, publisher addresses) of SUBSCRIBEs still in flight.
    pending: Vec<(u64, Vec<IpAddr>)>,
    next_token: u64,
}

/// Every GENA subscription this process holds, keyed by SID together
/// with the publisher's address. The HTTP server accepts a NOTIFY only
/// when both match, so a LAN host can't inject volume/mute events by
/// guessing the callback path.
#[derive(Default)]
pub struct SubscriptionRegistry {
    inner: Mutex<RegistryInner>,
    changed: Condvar,
}

impl SubscriptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note a SUBSCRIBE about to be sent to `publisher`. Pass the token
    /// to [`Self::finish_subscribe`] once the response is in.
    pub(crate) fn begin_subscribe(&self, publisher: Vec<IpAddr>) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.next_token += 1;
        let token = g.next_token;
        let publisher = publisher.into_iter().map(canonical_ip).collect();
        g.pending.push((token, publisher));
        token
    }

    /// Resolve an in-flight SUBSCRIBE: registers `sid` (if it succeeded)
    /// for the addresses given to [`Self::begin_subscribe`].
    pub(crate) fn finish_subscribe(&self, token: u64, sid: Option<&str>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(pos) = g.pending.iter().position(|(t, _)| *t == token) {
            let (_, ips) = g.pending.remove(pos);
            if let Some(sid) = sid {
                g.live.retain(|(s, _)| s != sid);
                g.live.push((sid.to_string(), ips));
            }
        }
        drop(g);
        self.changed.notify_all();
    }

    pub(crate) fn remove(&self, sid: &str) {
        self.inner.lock().unwrap().live.retain(|(s, _)| s != sid);
        self.changed.notify_all();
    }

    pub fn check(&self, sid: &str, peer: IpAddr) -> NotifyAdmission {
        let g = self.inner.lock().unwrap();
        Self::check_locked(&g, sid, canonical_ip(peer))
    }

    fn check_locked(g: &RegistryInner, sid: &str, peer: IpAddr) -> NotifyAdmission {
        if g.live.iter().any(|(s, ips)| s == sid && ips.contains(&peer)) {
            NotifyAdmission::Live
        } else if g.pending.iter().any(|(_, ips)| ips.contains(&peer)) {
            NotifyAdmission::Pending
        } else {
            NotifyAdmission::Unknown
        }
    }

    /// Like [`Self::check`], but while the answer is `Pending` wait (up to
    /// `timeout`) for the in-flight SUBSCRIBE to resolve. True only for a
    /// live match.
    pub fn wait_live(&self, sid: &str, peer: IpAddr, timeout: Duration) -> bool {
        let peer = canonical_ip(peer);
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            match Self::check_locked(&g, sid, peer) {
                NotifyAdmission::Live => return true,
                NotifyAdmission::Unknown => return false,
                NotifyAdmission::Pending => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self.changed.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
}

/// The process-wide registry used by [`GenaManager`] and the HTTP
/// server's NOTIFY route.
pub fn registry() -> &'static SubscriptionRegistry {
    static REGISTRY: OnceLock<SubscriptionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SubscriptionRegistry::new)
}

/// Addresses the publisher behind `event_url` can send NOTIFYs from:
/// the event URL's host (an IP literal for every renderer we know of;
/// a name is resolved).
fn publisher_addresses(event_url: &str) -> Vec<IpAddr> {
    let Ok(url) = url::Url::parse(event_url) else {
        return Vec::new();
    };
    match url.host() {
        Some(url::Host::Ipv4(v4)) => vec![IpAddr::V4(v4)],
        Some(url::Host::Ipv6(v6)) => vec![IpAddr::V6(v6)],
        Some(url::Host::Domain(name)) => (name, url.port().unwrap_or(80))
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip()).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Manager that keeps a subscription alive in the background and parses
/// incoming notifies.
pub struct GenaManager {
    state: Mutex<Option<SubscriptionState>>,
    /// Public callback URL the renderer should call back on.
    callback_url: String,
    /// Set by [`GenaManager::unsubscribe`]; stops the renewer and keeps
    /// it from re-subscribing a speaker we've moved away from.
    closed: AtomicBool,
}

impl GenaManager {
    pub fn new(callback_url: String) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(None),
            callback_url,
            closed: AtomicBool::new(false),
        })
    }

    /// SUBSCRIBE to `event_url`. Returns the SID assigned by the renderer.
    pub fn subscribe(&self, event_url: &str) -> Result<String> {
        if self.closed.load(Ordering::Acquire) {
            return Err(anyhow!("subscription closed"));
        }
        let reg = registry();
        let token = reg.begin_subscribe(publisher_addresses(event_url));
        let result = http_subscribe(event_url, &self.callback_url, 1800);
        reg.finish_subscribe(token, result.as_ref().ok().map(|(sid, _)| sid.as_str()));
        let (sid, timeout) = result?;
        let st = SubscriptionState {
            sid: sid.clone(),
            timeout_secs: timeout,
            event_url: event_url.to_string(),
            last_renew: Instant::now(),
        };
        let old = self.state.lock().unwrap().replace(st);
        if let Some(old) = old.filter(|o| o.sid != sid) {
            reg.remove(&old.sid);
        }
        // Lost a race with unsubscribe(): withdraw what we just added.
        if self.closed.load(Ordering::Acquire) {
            if let Some(s) = self.state.lock().unwrap().take() {
                reg.remove(&s.sid);
                let _ = http_unsubscribe(&s.event_url, &s.sid);
            }
            return Err(anyhow!("subscription closed"));
        }
        info!("GENA subscribed: sid={} timeout={}s", sid, timeout);
        Ok(sid)
    }

    /// Background renewer loop. Renews well before the timeout expires.
    /// Exits once [`GenaManager::unsubscribe`] has run.
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
                if self.closed.load(Ordering::Acquire) {
                    return;
                }

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
                            if self.closed.load(Ordering::Acquire) {
                                return;
                            }
                            warn!("GENA renew failed: {}; re-subscribing", e);
                            let _ = self.subscribe(&s.event_url);
                        }
                    }
                }
            })
            .expect("spawning GENA renewer");
    }

    /// UNSUBSCRIBE on shutdown / speaker switch.  Best-effort. NOTIFYs
    /// carrying the old SID are refused from here on.
    pub fn unsubscribe(&self) {
        self.closed.store(true, Ordering::Release);
        let st = self.state.lock().unwrap().take();
        if let Some(s) = st {
            registry().remove(&s.sid);
            let _ = http_unsubscribe(&s.event_url, &s.sid);
        }
    }
}

/// Parse a NOTIFY body for volume/mute changes. Handles both shapes we
/// subscribe to:
///
///  * RenderingControl — a `LastChange` property whose text is an
///    entity-encoded XML document with `<Volume>` / `<Mute>` elements;
///  * GroupRenderingControl (Sonos group volume) — flat `<GroupVolume>`
///    / `<GroupMute>` properties, no LastChange wrapper.
///
/// Returns None if we can't extract anything useful (which is fine —
/// we'll just ignore the notify).
pub fn parse_rendering_notify(body: &str) -> Option<RenderingChange> {
    let Some(lc) = find_tag_text(body, "LastChange") else {
        // GroupRenderingControl notifies carry flat properties.
        let volume = find_tag_text(body, "GroupVolume").and_then(|v| v.trim().parse::<u32>().ok());
        let mute = find_tag_text(body, "GroupMute")
            .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"));
        if volume.is_none() && mute.is_none() {
            return None;
        }
        return Some(RenderingChange { volume, mute });
    };
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

    // SUBSCRIBE/UNSUBSCRIBE replies are headers only.
    let buf = crate::ssdp::read_to_end_bounded(
        &mut stream,
        64 * 1024,
        std::time::Instant::now() + crate::ssdp::RESPONSE_DEADLINE,
    )
    .with_context(|| format!("reading {} response", method))?;
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

/// Pull the text between the first `<tag>...</tag>` pair. Also used by
/// upnp.rs / sonos.rs for the ZoneGroupState payload.
pub(crate) fn find_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = xml.find(&open)?;
    let after = &xml[i + open.len()..];
    let j = after.find(&close)?;
    Some(after[..j].to_string())
}

pub(crate) fn xml_unescape(s: &str) -> String {
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
    fn registry_matches_sid_and_publisher() {
        let reg = SubscriptionRegistry::new();
        let speaker: IpAddr = "192.168.1.50".parse().unwrap();
        let other: IpAddr = "192.168.1.99".parse().unwrap();
        assert_eq!(reg.check("uuid:a", speaker), NotifyAdmission::Unknown);

        let t = reg.begin_subscribe(vec![speaker]);
        assert_eq!(reg.check("uuid:a", speaker), NotifyAdmission::Pending);
        assert_eq!(reg.check("uuid:a", other), NotifyAdmission::Unknown);
        reg.finish_subscribe(t, Some("uuid:a"));
        assert_eq!(reg.check("uuid:a", speaker), NotifyAdmission::Live);
        // IPv4-mapped form of the same address.
        assert_eq!(reg.check("uuid:a", "::ffff:192.168.1.50".parse().unwrap()), NotifyAdmission::Live);
        assert_eq!(reg.check("uuid:a", other), NotifyAdmission::Unknown);
        assert_eq!(reg.check("uuid:b", speaker), NotifyAdmission::Unknown);

        reg.remove("uuid:a");
        assert_eq!(reg.check("uuid:a", speaker), NotifyAdmission::Unknown);

        // A failed SUBSCRIBE leaves nothing behind.
        let t = reg.begin_subscribe(vec![speaker]);
        reg.finish_subscribe(t, None);
        assert_eq!(reg.check("uuid:a", speaker), NotifyAdmission::Unknown);
    }

    #[test]
    fn wait_live_sees_a_subscribe_that_completes_meanwhile() {
        let reg = Arc::new(SubscriptionRegistry::new());
        let speaker: IpAddr = "192.168.1.50".parse().unwrap();
        let t = reg.begin_subscribe(vec![speaker]);
        let r2 = reg.clone();
        let h = thread::spawn(move || r2.wait_live("uuid:late", speaker, Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(50));
        reg.finish_subscribe(t, Some("uuid:late"));
        assert!(h.join().unwrap());
        // Unknown publisher: no wait at all.
        let start = Instant::now();
        assert!(!reg.wait_live("uuid:late", "10.0.0.1".parse().unwrap(), Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn publisher_addresses_from_event_url() {
        assert_eq!(
            publisher_addresses("http://192.168.1.50:1400/MediaRenderer/RenderingControl/Event"),
            vec!["192.168.1.50".parse::<IpAddr>().unwrap()]
        );
        assert!(publisher_addresses("not a url").is_empty());
    }

    #[test]
    fn unsubscribed_manager_does_not_resubscribe() {
        let m = GenaManager::new("http://127.0.0.1:1/gena".into());
        m.unsubscribe();
        assert!(m.subscribe("http://127.0.0.1:1/Event").is_err());
    }

    #[test]
    fn xml_unescape_basic() {
        assert_eq!(xml_unescape("a &amp; b &lt;c&gt; &quot;d&quot;"), "a & b <c> \"d\"");
    }

    #[test]
    fn parses_group_volume_change() {
        // GroupRenderingControl NOTIFY — flat properties, no LastChange.
        let body = r#"<?xml version="1.0"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
<e:property><GroupVolume>32</GroupVolume></e:property>
<e:property><GroupMute>1</GroupMute></e:property>
<e:property><GroupVolumeChangeable>1</GroupVolumeChangeable></e:property>
</e:propertyset>"#;
        let rc = parse_rendering_notify(body).expect("decode");
        assert_eq!(rc.volume, Some(32));
        assert_eq!(rc.mute, Some(true));
    }

    #[test]
    fn ignores_empty_group_notify() {
        let body = r#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
<e:property><GroupVolumeChangeable>1</GroupVolumeChangeable></e:property>
</e:propertyset>"#;
        assert!(parse_rendering_notify(body).is_none());
    }
}
