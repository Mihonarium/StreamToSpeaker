//! Which network adapter speaker discovery runs on.
//!
//! By default SSDP and mDNS search from every adapter. A PC with a VPN,
//! a virtual-machine switch or two physical networks can instead be told
//! to search on one adapter only. The choice is strict: while that
//! adapter is down (unplugged, Wi-Fi off, no IPv4 address) discovery is
//! *paused*, never widened back to every adapter, and resumes by itself
//! when the adapter returns. Speakers added by address and the audio
//! path itself don't depend on it.

use log::{info, warn};
use mdns_sd::{IfKind, ServiceDaemon};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::airplay::AirPlayDiscoveryState;

/// One network adapter as the selector shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetAdapter {
    /// Stable identity: the adapter GUID on Windows (survives renames),
    /// the interface name elsewhere.
    pub key: String,
    /// Display name ("Wi-Fi", "Ethernet 2"). Also what the mDNS stack
    /// matches interfaces by.
    pub name: String,
    /// Every IPv4 address on it, link-local included (shown, not used).
    pub ipv4: Vec<Ipv4Addr>,
}

impl NetAdapter {
    /// The address discovery would search from: the first IPv4 that is
    /// neither link-local nor unspecified. None = the adapter has no
    /// working IPv4 connection (unplugged, or still on APIPA).
    pub fn usable_ipv4(&self) -> Option<Ipv4Addr> {
        self.ipv4
            .iter()
            .copied()
            .find(|ip| !ip.is_link_local() && !ip.is_unspecified())
    }
}

/// The persisted choice (`None` in config = every adapter). The name is
/// kept so a missing adapter can still be shown by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAdapter {
    pub key: String,
    pub name: String,
}

/// The adapters on this machine, loopback excluded. Ones with a usable
/// IPv4 address first, then by name.
pub fn list_adapters() -> Vec<NetAdapter> {
    let mut out: Vec<NetAdapter> = Vec::new();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if iface.is_loopback() {
            continue;
        }
        #[cfg(windows)]
        let key = iface.adapter_name.clone();
        #[cfg(not(windows))]
        let key = iface.name.clone();
        let idx = match out.iter().position(|a| a.key == key) {
            Some(i) => i,
            None => {
                out.push(NetAdapter { key, name: iface.name.clone(), ipv4: Vec::new() });
                out.len() - 1
            }
        };
        if let IpAddr::V4(v4) = iface.ip() {
            if !out[idx].ipv4.contains(&v4) {
                out[idx].ipv4.push(v4);
            }
        }
    }
    sort_adapters(&mut out);
    out
}

fn sort_adapters(v: &mut [NetAdapter]) {
    v.sort_by(|a, b| {
        b.usable_ipv4()
            .is_some()
            .cmp(&a.usable_ipv4().is_some())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// What discovery should be doing right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// No adapter chosen: every adapter.
    All,
    /// The chosen adapter is up: search there only.
    Active { name: String, ip: Ipv4Addr },
    /// The chosen adapter is missing or has no IPv4 address.
    Paused { name: String },
}

/// Map the saved choice onto the adapters present now.
pub fn resolve(selected: Option<&SavedAdapter>, adapters: &[NetAdapter]) -> Resolved {
    let Some(sel) = selected else {
        return Resolved::All;
    };
    match adapters.iter().find(|a| a.key == sel.key) {
        Some(a) => match a.usable_ipv4() {
            Some(ip) => Resolved::Active { name: a.name.clone(), ip },
            None => Resolved::Paused { name: a.name.clone() },
        },
        None => Resolved::Paused { name: sel.name.clone() },
    }
}

/// Owns the mDNS daemon so it can be rebuilt on a different set of
/// interfaces. A rebuild forgets every receiver first, so nothing found
/// through the old interfaces lingers in the list.
pub struct MdnsController {
    state: Arc<AirPlayDiscoveryState>,
    daemon: Mutex<Option<ServiceDaemon>>,
}

impl MdnsController {
    pub fn new(state: Arc<AirPlayDiscoveryState>) -> Self {
        Self { state, daemon: Mutex::new(None) }
    }

    /// (Re)start browsing: on every interface (`None`) or only on the
    /// interfaces of the adapter named `only`.
    pub fn start(&self, only: Option<&str>) -> anyhow::Result<()> {
        self.stop();
        let daemon =
            ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon init: {}", e))?;
        if let Some(name) = only {
            // Selections apply in order: drop everything, then add back
            // the one adapter (by the name the mDNS stack sees, which is
            // the same `if_addrs` name the selector lists).
            daemon
                .disable_interface(IfKind::All)
                .and_then(|_| daemon.enable_interface(IfKind::Name(name.to_string())))
                .map_err(|e| anyhow::anyhow!("mdns interface selection: {}", e))?;
            info!("AirPlay discovery: restricted to adapter {:?}", name);
        }
        crate::airplay::discovery::spawn_airplay_discovery_on(self.state.clone(), daemon.clone())?;
        *self.daemon.lock().unwrap() = Some(daemon);
        Ok(())
    }

    /// Stop browsing and forget every receiver.
    pub fn stop(&self) {
        let old = self.daemon.lock().unwrap().take();
        if let Some(d) = old {
            match d.shutdown() {
                Ok(rx) => {
                    let _ = rx.recv_timeout(Duration::from_secs(1));
                }
                Err(e) => warn!("mdns daemon shutdown: {}", e),
            }
            // Let the consumer threads drain what was already queued
            // before the list is wiped.
            std::thread::sleep(Duration::from_millis(200));
        }
        self.state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(key: &str, name: &str, ips: &[&str]) -> NetAdapter {
        NetAdapter {
            key: key.into(),
            name: name.into(),
            ipv4: ips.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    fn saved(key: &str, name: &str) -> SavedAdapter {
        SavedAdapter { key: key.into(), name: name.into() }
    }

    #[test]
    fn no_choice_means_every_adapter() {
        assert_eq!(resolve(None, &[]), Resolved::All);
    }

    #[test]
    fn chosen_adapter_up_down_and_missing() {
        let up = [adapter("{A}", "Wi-Fi", &["169.254.3.4", "192.168.1.5"])];
        assert_eq!(
            resolve(Some(&saved("{A}", "old name")), &up),
            Resolved::Active { name: "Wi-Fi".into(), ip: "192.168.1.5".parse().unwrap() }
        );
        // Only an APIPA address: down, not a fallback to anything else.
        let apipa = [
            adapter("{A}", "Wi-Fi", &["169.254.3.4"]),
            adapter("{B}", "Ethernet", &["10.0.0.2"]),
        ];
        assert_eq!(
            resolve(Some(&saved("{A}", "Wi-Fi")), &apipa),
            Resolved::Paused { name: "Wi-Fi".into() }
        );
        // Gone entirely: paused under the saved name.
        assert_eq!(
            resolve(Some(&saved("{C}", "USB LAN")), &apipa),
            Resolved::Paused { name: "USB LAN".into() }
        );
    }

    #[test]
    fn available_adapters_sort_first() {
        let mut v = vec![
            adapter("1", "Bluetooth", &[]),
            adapter("2", "zeta", &["10.0.0.2"]),
            adapter("3", "Alpha", &["192.168.1.5"]),
        ];
        sort_adapters(&mut v);
        let names: Vec<_> = v.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "zeta", "Bluetooth"]);
    }
}
