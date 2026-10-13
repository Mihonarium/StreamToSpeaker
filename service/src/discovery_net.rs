//! Which network adapter speaker discovery runs on.
//!
//! By default SSDP and mDNS (AirPlay and Sendspin speakers) search from
//! every adapter. A PC with a VPN, a virtual-machine switch or two
//! physical networks can instead be told to search on one adapter only.
//! The choice is strict: while that adapter is down (unplugged, Wi-Fi
//! off, no IPv4 address) discovery is *paused*, never widened back to
//! every adapter, and resumes by itself when the adapter returns.
//! Speakers added by address, the audio path itself and this PC's own
//! Music Assistant input advertisement don't depend on it.

use log::{info, warn};
use mdns_sd::{IfKind, ServiceDaemon};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::Duration;

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

/// The part of a [`Resolved`] scope an mDNS browse depends on: which
/// interfaces it runs on. An address change on the chosen adapter is not
/// a change here — the mDNS stack follows it by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdnsScope {
    All,
    Only(String),
    Paused,
}

impl From<&Resolved> for MdnsScope {
    fn from(r: &Resolved) -> Self {
        match r {
            Resolved::All => MdnsScope::All,
            Resolved::Active { name, .. } => MdnsScope::Only(name.clone()),
            Resolved::Paused { .. } => MdnsScope::Paused,
        }
    }
}

/// Starts a browse on a daemon; its threads end when the daemon shuts down.
type BrowseFn = dyn Fn(ServiceDaemon) -> anyhow::Result<()> + Send + Sync;

/// How a controller browses: on a real mDNS daemon, or (tests) through a
/// stand-in that opens no sockets.
enum Browser {
    Mdns(Box<BrowseFn>),
    #[cfg(test)]
    Stub(Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>),
}

/// A running browse's daemon.
enum Daemon {
    Mdns(ServiceDaemon),
    #[cfg(test)]
    Stub,
}

/// Owns one discovery's mDNS daemon (AirPlay's, Sendspin's) so it can be
/// rebuilt on a different set of interfaces. A rebuild forgets everything
/// found first, so nothing found through the old interfaces lingers in
/// the list.
pub struct MdnsController {
    /// Which discovery this is, for the log.
    what: &'static str,
    browse: Browser,
    /// Empties the discovery's list.
    forget: Box<dyn Fn() + Send + Sync>,
    daemon: Mutex<Option<Daemon>>,
    /// The scope last applied successfully; `None` before the first, after
    /// [`stop`](Self::stop) and after a failed start.
    applied: Mutex<Option<MdnsScope>>,
}

impl MdnsController {
    pub fn new(
        what: &'static str,
        browse: impl Fn(ServiceDaemon) -> anyhow::Result<()> + Send + Sync + 'static,
        forget: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::with_browser(what, Browser::Mdns(Box::new(browse)), Box::new(forget))
    }

    /// A controller whose browse runs no mDNS daemon: `start` stands in
    /// for creating the daemon and starting the browse.
    #[cfg(test)]
    pub(crate) fn stub(
        what: &'static str,
        start: impl Fn() -> anyhow::Result<()> + Send + Sync + 'static,
        forget: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::with_browser(what, Browser::Stub(Box::new(start)), Box::new(forget))
    }

    fn with_browser(what: &'static str, browse: Browser, forget: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            what,
            browse,
            forget,
            daemon: Mutex::new(None),
            applied: Mutex::new(None),
        }
    }

    /// Browse on `scope`: restart on a new set of interfaces, stop and
    /// forget everything while paused, and leave a browse that already
    /// runs on these interfaces alone.
    pub fn apply(&self, scope: &Resolved) -> anyhow::Result<()> {
        let want = MdnsScope::from(scope);
        let mut applied = self.applied.lock().unwrap();
        if applied.as_ref() == Some(&want) {
            return Ok(());
        }
        let result = match &want {
            MdnsScope::All => self.start(None),
            MdnsScope::Only(name) => self.start(Some(name)),
            MdnsScope::Paused => {
                self.shutdown();
                Ok(())
            }
        };
        *applied = result.is_ok().then_some(want);
        result
    }

    /// The scope the browse runs on (see [`apply`](Self::apply)).
    pub fn applied(&self) -> Option<MdnsScope> {
        self.applied.lock().unwrap().clone()
    }

    /// A daemon is running (not paused, stopped or failed).
    pub fn is_running(&self) -> bool {
        self.daemon.lock().unwrap().is_some()
    }

    /// Stop browsing and forget everything found.
    pub fn stop(&self) {
        let mut applied = self.applied.lock().unwrap();
        self.shutdown();
        *applied = None;
    }

    /// (Re)start browsing: on every interface (`None`) or only on the
    /// interfaces of the adapter named `only`.
    fn start(&self, only: Option<&str>) -> anyhow::Result<()> {
        self.shutdown();
        let browse = match &self.browse {
            Browser::Mdns(browse) => browse,
            #[cfg(test)]
            Browser::Stub(start) => {
                start()?;
                *self.daemon.lock().unwrap() = Some(Daemon::Stub);
                return Ok(());
            }
        };
        let daemon =
            ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns daemon init: {}", e))?;
        let started = (|| {
            if let Some(name) = only {
                // Selections apply in order: drop everything, then add
                // back the one adapter (by the name the mDNS stack sees,
                // which is the same `if_addrs` name the selector lists).
                daemon
                    .disable_interface(IfKind::All)
                    .and_then(|_| daemon.enable_interface(IfKind::Name(name.to_string())))
                    .map_err(|e| anyhow::anyhow!("mdns interface selection: {}", e))?;
                info!("{} discovery: restricted to adapter {:?}", self.what, name);
            }
            browse(daemon.clone())
        })();
        if let Err(e) = started {
            let _ = daemon.shutdown();
            return Err(e);
        }
        *self.daemon.lock().unwrap() = Some(Daemon::Mdns(daemon));
        Ok(())
    }

    /// Shut the daemon down and forget everything found.
    fn shutdown(&self) {
        let old = self.daemon.lock().unwrap().take();
        if let Some(Daemon::Mdns(d)) = old {
            match d.shutdown() {
                Ok(rx) => {
                    let _ = rx.recv_timeout(Duration::from_secs(1));
                }
                Err(e) => warn!("{} mdns daemon shutdown: {}", self.what, e),
            }
            // Let the consumer threads drain what was already queued
            // before the list is wiped.
            std::thread::sleep(Duration::from_millis(200));
        }
        (self.forget)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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
    fn mdns_scope_ignores_address_changes() {
        let at = |ip: &str| Resolved::Active { name: "Wi-Fi".into(), ip: ip.parse().unwrap() };
        assert_eq!(MdnsScope::from(&at("192.168.1.5")), MdnsScope::from(&at("192.168.1.6")));
        assert_eq!(MdnsScope::from(&at("192.168.1.5")), MdnsScope::Only("Wi-Fi".into()));
        assert_eq!(MdnsScope::from(&Resolved::All), MdnsScope::All);
        assert_eq!(
            MdnsScope::from(&Resolved::Paused { name: "USB LAN".into() }),
            MdnsScope::Paused
        );
    }

    #[test]
    fn paused_controller_browses_nothing_and_forgets() {
        let browsed = Arc::new(AtomicUsize::new(0));
        let forgot = Arc::new(AtomicUsize::new(0));
        let (b, f) = (browsed.clone(), forgot.clone());
        let ctl = MdnsController::new(
            "test",
            move |_| {
                b.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            move || {
                f.fetch_add(1, Ordering::SeqCst);
            },
        );
        let paused = Resolved::Paused { name: "Wi-Fi".into() };
        ctl.apply(&paused).unwrap();
        assert!(!ctl.is_running());
        assert_eq!(ctl.applied(), Some(MdnsScope::Paused));
        assert_eq!(forgot.load(Ordering::SeqCst), 1);
        // Still paused (under another name): nothing to do.
        ctl.apply(&Resolved::Paused { name: "Ethernet".into() }).unwrap();
        assert_eq!(forgot.load(Ordering::SeqCst), 1);
        // A stopped controller applies the next scope afresh.
        ctl.stop();
        assert_eq!(ctl.applied(), None);
        ctl.apply(&paused).unwrap();
        assert_eq!(forgot.load(Ordering::SeqCst), 3);
        assert_eq!(browsed.load(Ordering::SeqCst), 0);
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
