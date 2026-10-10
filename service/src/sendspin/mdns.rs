//! One shared mDNS daemon for Sendspin: advertising our source client
//! (`_sendspin._tcp`) and browsing for players (`_sendspin._tcp` too —
//! players and sources are both Sendspin *clients*, discovered by the
//! server the same way).

use anyhow::{anyhow, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

pub const CLIENT_SERVICE: &str = "_sendspin._tcp.local.";
/// TXT marker on this app's source advertisement.
pub const SOURCE_MARKER_KEY: &str = "sts";
pub const SOURCE_MARKER_VALUE: &str = "source";
pub const DEFAULT_CLIENT_PORT: u16 = 8928;
pub const DEFAULT_PATH: &str = "/sendspin";

static DAEMON: OnceLock<Mutex<Option<ServiceDaemon>>> = OnceLock::new();
static OWN_INSTANCES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// The process-wide Sendspin mDNS daemon (created on first use).
pub fn daemon() -> Result<ServiceDaemon> {
    let slot = DAEMON.get_or_init(|| Mutex::new(None));
    let mut g = slot.lock().map_err(|_| anyhow!("mdns lock poisoned"))?;
    if let Some(d) = g.as_ref() {
        return Ok(d.clone());
    }
    let d = ServiceDaemon::new().map_err(|e| anyhow!("mDNS daemon: {}", e))?;
    *g = Some(d.clone());
    Ok(d)
}

fn own() -> &'static Mutex<HashSet<String>> {
    OWN_INSTANCES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// True if `fullname` is a service this process advertises (so the
/// player browser can skip our own source advertisement).
pub fn is_own_instance(fullname: &str) -> bool {
    own().lock().map(|s| s.contains(&fullname.to_ascii_lowercase())).unwrap_or(false)
}

/// mDNS-safe instance label: printable, no dots, ≤ 63 bytes.
pub fn instance_label(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c == '.' || c.is_control() { ' ' } else { c })
        .collect();
    s = s.trim().to_string();
    if s.is_empty() {
        s = "Stream To Speaker".into();
    }
    while s.len() > 63 {
        s.pop();
    }
    s
}

/// DNS host label derived from the machine name.
fn host_label(name: &str) -> String {
    let mut h: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    h = h.trim_matches('-').to_string();
    if h.is_empty() {
        h = "stream-to-speaker".into();
    }
    h.truncate(40);
    h
}

/// True for this app's own Music Assistant source advertisement (another
/// copy of the app), which is not a speaker.
pub fn is_source_advert(info: &ServiceInfo) -> bool {
    info.get_property_val_str(SOURCE_MARKER_KEY) == Some(SOURCE_MARKER_VALUE)
}

/// Advertise a Sendspin client endpoint. Returns the registered fullname
/// (pass it to [`unregister`]).
pub fn register_client(instance: &str, friendly_name: &str, port: u16) -> Result<String> {
    let d = daemon()?;
    let instance = instance_label(instance);
    let host = format!("{}-sendspin.local.", host_label(&crate::sendspin::machine_name()));
    // `sts=source` marks this app's own source advertisement, so other
    // copies of the app don't list it as a speaker (roles are otherwise
    // only known after connecting).
    let props = [("path", DEFAULT_PATH), ("name", friendly_name), (SOURCE_MARKER_KEY, SOURCE_MARKER_VALUE)];
    let info = ServiceInfo::new(CLIENT_SERVICE, &instance, &host, "", port, &props[..])
        .map_err(|e| anyhow!("mDNS service info: {}", e))?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    d.register(info).map_err(|e| anyhow!("mDNS register: {}", e))?;
    if let Ok(mut s) = own().lock() {
        s.insert(fullname.to_ascii_lowercase());
    }
    Ok(fullname)
}

pub fn unregister(fullname: &str) {
    if let Ok(d) = daemon() {
        let _ = d.unregister(fullname);
    }
    if let Ok(mut s) = own().lock() {
        s.remove(&fullname.to_ascii_lowercase());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_marker_is_recognised() {
        let src = ServiceInfo::new(CLIENT_SERVICE, "pc", "pc.local.", "192.0.2.1", 8928,
            &[("path", DEFAULT_PATH), (SOURCE_MARKER_KEY, SOURCE_MARKER_VALUE)][..]).unwrap();
        let spk = ServiceInfo::new(CLIENT_SERVICE, "kitchen", "k.local.", "192.0.2.2", 8928,
            &[("path", DEFAULT_PATH), ("name", "Kitchen")][..]).unwrap();
        assert!(is_source_advert(&src));
        assert!(!is_source_advert(&spk));
    }

    use super::*;

    #[test]
    fn labels_are_sanitised() {
        assert_eq!(instance_label("My.PC"), "My PC");
        assert_eq!(instance_label("  "), "Stream To Speaker");
        assert_eq!(instance_label(&"x".repeat(80)).len(), 63);
        assert_eq!(host_label("DESKTOP_42 Ü"), "desktop-42");
    }
}
