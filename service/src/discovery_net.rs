//! Which network adapter speaker discovery runs on, and which addresses
//! it may send requests to.
//!
//! By default SSDP and mDNS (AirPlay and Sendspin speakers) search from
//! every adapter. A PC with a VPN, a virtual-machine switch or two
//! physical networks can instead be told to search on one adapter only.
//! The choice is strict: while that adapter is down (unplugged, Wi-Fi
//! off, no IPv4 address) discovery is *paused*, never widened back to
//! every adapter, and resumes by itself when the adapter returns.
//! Speakers added by address, the audio path itself and this PC's own
//! Music Assistant input advertisement don't depend on it.
//!
//! [`LanTargets`] limits the requests UPnP discovery makes on other
//! devices' say-so to this PC's own networks, and [`Reported`] puts each
//! target it refuses in the log once.

use log::{debug, info, warn};
use mdns_sd::{IfKind, ServiceDaemon};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::Duration;

/// The IPv4 networks this PC is attached to: the only places UPnP
/// discovery sends requests to.
///
/// Discovery follows URLs that other devices hand it: an SSDP reply's
/// LOCATION, the ZoneGroupTopology URL in a device description, the
/// member Locations in a Sonos topology. It fetches or posts to them by
/// itself, without anyone choosing them, so without this check any
/// device on the network could point this PC at any address: services
/// listening on this machine, hosts on other networks and beyond the LAN.
/// A target passes only as a unicast host address inside the subnet of
/// one of this PC's interfaces (the interface address and its prefix
/// length): never loopback, unspecified, multicast or broadcast, never
/// one of this PC's own addresses (a service listening on every interface
/// answers there just as on loopback), and a link-local address only when
/// one of our interfaces is in that link-local subnet. The search is IPv4
/// multicast, so only IPv4 networks count. A URL that names a host
/// instead of an address is refused: the address the name resolves to
/// when we connect could differ from the one checked, and UPnP devices
/// advertise addresses.
///
/// Every other host on those subnets passes, a router's admin page there
/// included: a device that answers discovery can still aim these requests
/// at its neighbours on the LAN, but no further.
///
/// Speakers added by address don't go through this: requests to them are
/// the user's choice. That is also how a renderer running on this PC is
/// used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanTargets {
    nets: Vec<Ipv4Net>,
    /// Every IPv4 address of this PC's interfaces, including those whose
    /// network isn't in `nets`.
    own: Vec<Ipv4Addr>,
}

/// Why discovery won't send a request to a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// It isn't a URL with a host.
    Unreadable,
    /// It names a host, not an address.
    HostName(String),
    /// It names this PC: a loopback address or one of its own.
    ThisPc(IpAddr),
    /// It names no device: an unspecified, multicast or broadcast address,
    /// or a subnet's own address.
    NotADevice(IpAddr),
    /// It names an address outside this PC's networks.
    Elsewhere(IpAddr),
    /// It names an IPv6 address. Discovery follows only IPv4 addresses:
    /// the HTTP clients connect to the URL's host as written, and for an
    /// IPv6 literal that isn't guaranteed to be the address checked here.
    Ipv6(IpAddr),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Unreadable => "it isn't a URL with a host",
            Refusal::HostName(_) => "it names a host, and discovery follows only addresses",
            Refusal::ThisPc(ip) if ip.is_loopback() => "it is this PC's loopback address",
            Refusal::ThisPc(_) => "it is this PC's own address",
            Refusal::NotADevice(_) => "it isn't a device's address",
            Refusal::Elsewhere(_) => "it isn't on this PC's networks",
            Refusal::Ipv6(_) => "it is an IPv6 address, and discovery follows only IPv4 addresses",
        })
    }
}

/// Where the user adds a speaker by address, for the log lines that send
/// them there.
pub const ADD_BY_ADDRESS: &str = "add it by address (+ Add, under Speakers in the window)";

/// The most characters [`shown`] keeps of a string that came from the
/// network.
const SHOWN_CHARS: usize = 200;

/// `s` (from the network) for a log line: quoted, escaped and cut short.
pub fn shown(s: &str) -> String {
    let mut cut: String = s.chars().take(SHOWN_CHARS).collect();
    if cut.len() < s.len() {
        cut.push('…');
    }
    format!("{:?}", cut)
}

impl Refusal {
    /// What was refused, as the log names it: the address, the host name,
    /// or the URL itself when it has no host.
    pub fn target(&self, url: &str) -> String {
        match self {
            Refusal::Unreadable => shown(url),
            Refusal::HostName(name) => shown(name),
            Refusal::ThisPc(ip) | Refusal::NotADevice(ip) | Refusal::Elsewhere(ip) | Refusal::Ipv6(ip) => ip.to_string(),
        }
    }

    /// What the user can do instead, if anything: discovery never follows
    /// these, but a speaker added by address is contacted wherever it is.
    pub fn hint(&self) -> Option<String> {
        match self {
            Refusal::ThisPc(_) => Some(format!("To use a speaker running on this PC, {}.", ADD_BY_ADDRESS)),
            Refusal::Elsewhere(_) | Refusal::HostName(_) => Some(format!("To use a speaker there, {}.", ADD_BY_ADDRESS)),
            // Adding by address takes only IPv4 addresses and host names.
            Refusal::Ipv6(_) => {
                Some("To use a speaker there, add it by its IPv4 address (+ Add, under Speakers in the window).".to_string())
            }
            Refusal::Unreadable | Refusal::NotADevice(_) => None,
        }
    }

    /// The log line for refusing `url`, which came `from` somewhere ("from
    /// an SSDP reply by 192.168.1.1").
    pub fn line(&self, url: &str, from: &str) -> String {
        let mut line = format!("Not contacting {} ({}): {}.", self.target(url), from, self);
        if let Some(hint) = self.hint() {
            line.push(' ');
            line.push_str(&hint);
        }
        line
    }
}

/// Targets discovery won't contact, and devices whose ignored service URLs
/// it has logged. Each is reported once at info level rather than on every
/// scan: SSDP relayed from another subnet or VLAN doesn't reach the list,
/// and the user should learn why once, not every few minutes. Forgotten
/// with the discovery list.
#[derive(Default)]
pub struct Reported {
    seen: Mutex<HashSet<String>>,
}

/// Most targets remembered as reported: beyond any real network, a bound
/// for a flood of made-up replies, whose refusals go to the debug log.
const MAX_REPORTED: usize = 256;

impl Reported {
    /// Log `line` at info level the first time `key` comes up, at debug
    /// level after that. True the first time.
    pub fn once(&self, key: &str, line: impl FnOnce() -> String) -> bool {
        let first = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            !seen.contains(key) && seen.len() < MAX_REPORTED && seen.insert(key.to_string())
        };
        if first {
            info!("{}", line());
        } else {
            debug!("{}", line());
        }
        first
    }

    /// Forget what was reported (discovery moved to another network).
    pub fn clear(&self) {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// How many targets have been reported.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// A discovery scan's limits: the networks it may send requests to, and
/// the memory of the refusals it has reported.
#[derive(Clone, Copy)]
pub struct Reach<'a> {
    pub lan: &'a LanTargets,
    pub reported: &'a Reported,
}

impl Reach<'_> {
    /// May discovery follow `url`? `from` says where the URL came from,
    /// for the log line a refusal gets the first time its target comes up.
    pub fn allows(&self, url: &str, from: impl FnOnce() -> String) -> bool {
        self.address(url, from).is_some()
    }

    /// The address `url` names, if discovery may follow it (as
    /// [`LanTargets::check_url`] gives it); None, logged as by
    /// [`allows`](Self::allows), if not.
    pub fn address(&self, url: &str, from: impl FnOnce() -> String) -> Option<IpAddr> {
        match self.lan.check_url(url) {
            Ok(ip) => Some(ip),
            Err(why) => {
                self.reported.once(&why.target(url), || why.line(url, &from()));
                None
            }
        }
    }
}

/// An interface's IPv4 network: its address and on-link prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    fn mask(self) -> u32 {
        u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0)
    }

    fn contains(self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) ^ u32::from(self.addr)) & self.mask() == 0
    }

    /// Not the network's own address or its broadcast address. A /31 or
    /// /32 has neither (RFC 3021).
    fn is_host(self, ip: Ipv4Addr) -> bool {
        let host_bits = u32::from(ip) & !self.mask();
        self.prefix >= 31 || (host_bits != 0 && host_bits != !self.mask())
    }
}

/// The widest prefix taken as one network. An adapter reporting a wider
/// one (/0, say) is not describing a LAN.
const WIDEST_LAN_PREFIX: u8 = 8;

impl LanTargets {
    /// This PC's networks right now. Empty, so nothing passes, when the
    /// adapters can't be listed.
    pub fn current() -> Self {
        let ifaces = match if_addrs::get_if_addrs() {
            Ok(list) => list,
            Err(e) => {
                warn!("can't list this PC's network adapters ({}); discovery contacts no device", e);
                Vec::new()
            }
        };
        Self::from_networks(ifaces.into_iter().filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some((v4.ip, v4.prefixlen)),
            if_addrs::IfAddr::V6(_) => None,
        }))
    }

    /// From (interface address, prefix length) pairs. Every address is
    /// this PC's own; the networks of loopback and unspecified addresses,
    /// prefixes wider than /8 and invalid ones are left out.
    pub fn from_networks(nets: impl IntoIterator<Item = (Ipv4Addr, u8)>) -> Self {
        let mut lan = Vec::new();
        let mut own = Vec::new();
        for (addr, prefix) in nets {
            own.push(addr);
            if !addr.is_loopback() && !addr.is_unspecified() && (WIDEST_LAN_PREFIX..=32).contains(&prefix) {
                lan.push(Ipv4Net { addr, prefix });
            }
        }
        lan.sort();
        lan.dedup();
        own.sort();
        own.dedup();
        Self { nets: lan, own }
    }

    /// May discovery send a request to `ip`?
    pub fn allows(&self, ip: IpAddr) -> bool {
        self.refusal(ip).is_none()
    }

    /// Why discovery may not send a request to `ip`; None if it may. An
    /// IPv4-mapped IPv6 address is judged as the IPv4 address it carries.
    pub fn refusal(&self, ip: IpAddr) -> Option<Refusal> {
        let ip = crate::stream_gate::canonical_ip(ip);
        let v4 = match ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) if v6.is_loopback() => return Some(Refusal::ThisPc(ip)),
            IpAddr::V6(v6) if v6.is_unspecified() || v6.is_multicast() => return Some(Refusal::NotADevice(ip)),
            IpAddr::V6(_) => return Some(Refusal::Elsewhere(ip)),
        };
        if v4.is_loopback() || self.own.contains(&v4) {
            return Some(Refusal::ThisPc(ip));
        }
        if v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() {
            return Some(Refusal::NotADevice(ip));
        }
        let mut on_our_network = false;
        for net in self.nets.iter().filter(|n| n.contains(v4)) {
            if net.is_host(v4) {
                return None;
            }
            on_our_network = true;
        }
        Some(if on_our_network { Refusal::NotADevice(ip) } else { Refusal::Elsewhere(ip) })
    }

    /// Check a URL discovery is about to follow: the IPv4 address it names,
    /// or why it may not be followed.
    pub fn check_url(&self, url: &str) -> Result<IpAddr, Refusal> {
        let parsed = url::Url::parse(url).map_err(|_| Refusal::Unreadable)?;
        let ip = match parsed.host() {
            Some(url::Host::Ipv4(v4)) => IpAddr::V4(v4),
            Some(url::Host::Ipv6(v6)) => return Err(Refusal::Ipv6(IpAddr::V6(v6))),
            Some(url::Host::Domain(name)) => return Err(Refusal::HostName(name.to_string())),
            None => return Err(Refusal::Unreadable),
        };
        match self.refusal(ip) {
            None => Ok(crate::stream_gate::canonical_ip(ip)),
            Some(why) => Err(why),
        }
    }
}

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

    fn lan(nets: &[(&str, u8)]) -> LanTargets {
        LanTargets::from_networks(nets.iter().map(|(a, p)| (a.parse().unwrap(), *p)))
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn lan_targets_admit_hosts_on_our_subnets_only() {
        let t = lan(&[("192.168.1.5", 24), ("10.20.0.7", 16)]);
        for ok in ["192.168.1.10", "192.168.1.254", "10.20.200.3"] {
            assert!(t.allows(ip(ok)), "{ok}");
        }
        for refused in [
            "192.168.1.5",     // this PC
            "10.20.0.7",       // this PC, on its other network
            "127.0.0.1",       // loopback
            "127.5.5.5",       // loopback alias
            "0.0.0.0",         // unspecified
            "224.0.0.1",       // multicast
            "239.255.255.250", // SSDP multicast
            "255.255.255.255", // limited broadcast
            "192.168.1.255",   // our subnet's broadcast
            "192.168.1.0",     // our subnet's own address
            "192.168.2.10",    // another subnet
            "8.8.8.8",         // beyond the LAN
            "169.254.10.20",   // link-local, and no link-local interface
            "::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!t.allows(ip(refused)), "{refused}");
        }
        // An IPv4-mapped IPv6 address is judged as its IPv4 address.
        assert!(t.allows(ip("::ffff:192.168.1.10")));
        assert!(!t.allows(ip("::ffff:127.0.0.1")));
        assert!(!t.allows(ip("::ffff:192.168.1.5")));
    }

    #[test]
    fn lan_targets_refuse_every_address_of_this_pc() {
        // A second address whose prefix is left out, and an adapter with
        // an implausible network: still this PC.
        let t = lan(&[("192.168.1.5", 24), ("192.168.1.77", 33), ("100.64.0.9", 4)]);
        assert!(t.allows(ip("192.168.1.10")));
        for this_pc in ["192.168.1.5", "192.168.1.77", "::ffff:192.168.1.77", "100.64.0.9", "127.0.0.1", "::1"] {
            assert!(matches!(t.refusal(ip(this_pc)), Some(Refusal::ThisPc(_))), "{this_pc}");
        }
    }

    #[test]
    fn lan_targets_link_local_only_with_a_link_local_interface() {
        let t = lan(&[("169.254.3.4", 16)]);
        assert!(t.allows(ip("169.254.10.20")));
        assert!(!t.allows(ip("169.254.255.255")));
        assert!(!t.allows(ip("192.168.1.10")));
    }

    #[test]
    fn lan_targets_ignore_loopback_and_implausible_networks() {
        // The loopback interface is not a LAN; nor is an adapter claiming
        // a prefix wider than /8.
        let t = lan(&[("127.0.0.1", 8), ("100.64.0.9", 4), ("172.16.0.2", 33)]);
        assert!(t.nets.is_empty());
        assert!(!t.allows(ip("127.0.0.2")));
        assert!(!t.allows(ip("100.65.1.1")));
        assert!(!t.allows(ip("172.16.0.3")));
        // Nothing passes without networks.
        assert!(!LanTargets::default().allows(ip("192.168.1.10")));
    }

    #[test]
    fn lan_targets_point_to_point_networks() {
        // /31: both addresses are hosts (RFC 3021), and one of them is
        // ours; a /32 holds only our own address, so nothing passes.
        let t = lan(&[("10.0.0.0", 31), ("10.9.9.9", 32)]);
        assert!(!t.allows(ip("10.0.0.0")));
        assert!(t.allows(ip("10.0.0.1")));
        assert!(!t.allows(ip("10.9.9.9")));
        assert!(!t.allows(ip("10.9.9.8")));
    }

    #[test]
    fn lan_targets_say_why_a_url_is_refused() {
        let t = lan(&[("192.168.1.5", 24)]);
        let why = |url: &str| t.check_url(url).unwrap_err();
        assert_eq!(why("http://192.168.1.5:5901/stream.raw"), Refusal::ThisPc(ip("192.168.1.5")));
        assert_eq!(why("http://127.0.0.1:8080/admin"), Refusal::ThisPc(ip("127.0.0.1")));
        assert_eq!(why("http://[::ffff:127.0.0.1]/"), Refusal::Ipv6(ip("::ffff:127.0.0.1")));
        assert_eq!(why("http://[::ffff:192.168.1.10]/"), Refusal::Ipv6(ip("::ffff:192.168.1.10")));
        assert_eq!(why("http://10.0.5.20:1400/x"), Refusal::Elsewhere(ip("10.0.5.20")));
        assert_eq!(why("http://192.168.1.255/x"), Refusal::NotADevice(ip("192.168.1.255")));
        assert_eq!(why("http://239.255.255.250:1900/"), Refusal::NotADevice(ip("239.255.255.250")));
        assert_eq!(why("http://Speaker.local:1400/x"), Refusal::HostName("speaker.local".into()));
        assert_eq!(why("not a url"), Refusal::Unreadable);
        assert_eq!(Refusal::ThisPc(ip("127.0.0.1")).to_string(), "it is this PC's loopback address");
        assert_eq!(Refusal::ThisPc(ip("192.168.1.5")).to_string(), "it is this PC's own address");
    }

    #[test]
    fn lan_targets_check_urls() {
        let t = lan(&[("192.168.1.5", 24)]);
        assert_eq!(
            t.check_url("http://192.168.1.10:1400/xml/device_description.xml"),
            Ok(ip("192.168.1.10"))
        );
        for refused in [
            "http://127.0.0.1:8080/admin",
            "http://192.168.1.5:5901/stream.raw",
            "http://localhost:1400/xml/device_description.xml",
            "http://speaker.local:1400/xml/device_description.xml",
            // Other spellings of loopback, as the URL parser reads them.
            "http://0x7f000001/x",
            "http://2130706433/x",
            "http://[::1]:1400/x",
            "http://[::ffff:127.0.0.1]/x",
            // An IPv4 address on this network, spelled as IPv6.
            "http://[::ffff:192.168.1.10]:1400/x",
            "http://192.168.2.10/x",
            "not a url",
            "file:///etc/passwd",
        ] {
            assert!(t.check_url(refused).is_err(), "{refused}");
        }
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
