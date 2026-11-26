//! Network List Manager supplies profile identities; IP Helper describes the adapters.

use crate::platform::ConnKind;

fn adapter_kind(interface_type: u32, description: &str) -> Option<ConnKind> {
    // IANA ifType values used by MIB_IF_ROW2.
    const SOFTWARE_LOOPBACK: u32 = 24;
    const IEEE80211: u32 = 71;
    const ETHERNET: &[u32] = &[6, 26, 62, 69, 117];
    let description = description.to_ascii_lowercase();
    if interface_type == SOFTWARE_LOOPBACK || description.contains("loopback") {
        return None;
    }
    let virtual_adapter = [
        "wireguard",
        "wintun",
        "tap0901",
        "tailscale",
        "zerotier",
        "vpn",
        "tunnel",
        "virtual",
        "hyper-v",
        "anyconnect",
        "globalprotect",
        "wan miniport",
    ]
    .iter()
    .any(|word| description.contains(word));
    let tap = description.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| word == "tap");
    Some(if virtual_adapter || tap {
        ConnKind::Other
    } else if interface_type == IEEE80211 {
        ConnKind::WiFi
    } else if ETHERNET.contains(&interface_type) {
        ConnKind::Wired
    } else {
        ConnKind::Other
    })
}

#[cfg(windows)]
pub(super) use native::start;

#[cfg(windows)]
mod native {
    use super::adapter_kind;
    use crate::platform::win::{notices, Apartment};
    use crate::platform::{ConnKind, NetConnection, Network, SystemEvent};
    use crossbeam_channel::{bounded, Sender};
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;
    use windows::core::{Result, GUID};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::IpHelper::{
        CancelMibChangeNotify2, ConvertInterfaceGuidToLuid, GetIfEntry2, NotifyIpInterfaceChange, MIB_IF_ROW2,
        MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
    };
    use windows::Win32::Networking::NetworkListManager::{INetworkConnection, INetworkListManager, NetworkListManager};
    use windows::Win32::Networking::WinSock::AF_UNSPEC;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    #[derive(Default)]
    struct WindowsNetwork(Mutex<Vec<NetConnection>>);

    impl Network for WindowsNetwork {
        fn connections(&self) -> Vec<NetConnection> {
            self.0.lock().unwrap().clone()
        }
    }

    // There is one network monitor in this process. The notification needs
    // no borrowed context. A burst of interface changes needs one fresh read.
    static CHANGES: OnceLock<Sender<()>> = OnceLock::new();

    // SAFETY: this IP Helper callback uses neither pointer supplied by Windows.
    unsafe extern "system" fn changed(_: *const c_void, _: *const MIB_IPINTERFACE_ROW, _: MIB_NOTIFICATION_TYPE) {
        if let Some(wake) = CHANGES.get() {
            let _ = wake.try_send(());
        }
    }

    struct Notification(HANDLE);

    impl Notification {
        fn register() -> Option<Self> {
            let mut handle = HANDLE::default();
            // SAFETY: the callback uses no caller-owned pointers. Windows
            // writes the subscription handle, which Drop cancels off-callback.
            unsafe { NotifyIpInterfaceChange(AF_UNSPEC, Some(changed), None, false, &mut handle) }
                .is_ok()
                .then(|| Self(handle))
        }
    }

    impl Drop for Notification {
        fn drop(&mut self) {
            // SAFETY: this is the handle returned by the successful registration.
            let _ = unsafe { CancelMibChangeNotify2(self.0) };
        }
    }

    pub(in crate::platform::win) fn start() -> Arc<dyn Network> {
        let network = Arc::new(WindowsNetwork::default());
        let owner = Arc::downgrade(&network);
        let result = thread::Builder::new().name("stream-to-speaker-network".into()).spawn(move || {
            let Ok(_apartment) = Apartment::enter() else { return };
            let (wake, changes) = bounded(1);
            let _ = CHANGES.set(wake);
            let notification = Notification::register();
            // With Windows' notice of interface changes, a slow look is only
            // a safeguard. After a notice the monitor looks again a few
            // times: Windows names a network a moment after its adapter
            // connects.
            let idle = Duration::from_secs(if notification.is_some() { 60 } else { 5 });
            let mut settling = 0u8;
            let mut last = Vec::new();
            let mut notified = false;
            loop {
                let Some(network) = owner.upgrade() else { return };
                if let Ok(now) = read() {
                    let changed = now != last;
                    if changed {
                        *network.0.lock().unwrap() = now.iter().map(|(_, conn)| conn.clone()).collect();
                        last = now;
                    }
                    if changed || notified {
                        notices().tell(SystemEvent::NetworkChanged);
                    }
                }
                drop(network);
                let wait = if settling > 0 { Duration::from_secs(2) } else { idle };
                notified = changes.recv_timeout(wait).is_ok();
                settling = if notified { 3 } else { settling.saturating_sub(1) };
            }
        });
        if let Err(e) = result {
            log::debug!("Could not start the network monitor: {e}");
        }
        network
    }

    fn kind(guid: GUID) -> Option<ConnKind> {
        let mut row = MIB_IF_ROW2::default();
        // SAFETY: both calls use writable, sized fields in a live row.
        // GetIfEntry2 identifies the adapter by the LUID set by the first call.
        unsafe {
            if ConvertInterfaceGuidToLuid(&guid, &mut row.InterfaceLuid).is_err() || GetIfEntry2(&mut row).is_err() {
                return Some(ConnKind::Other);
            }
        }
        let end = row.Description.iter().position(|&c| c == 0).unwrap_or(row.Description.len());
        let kind = adapter_kind(row.Type, &String::from_utf16_lossy(&row.Description[..end]))?;
        // HardwareInterface is bit zero in MIB_IF_ROW2's status flags.
        Some(if row.InterfaceAndOperStatusFlags._bitfield & 1 == 0 { ConnKind::Other } else { kind })
    }

    fn connection_info(connection: INetworkConnection) -> Result<Option<(String, NetConnection)>> {
        // SAFETY: this live connection and its network are read on the enumerating MTA.
        unsafe {
            if !connection.IsConnected()?.as_bool() {
                return Ok(None);
            }
            let Some(kind) = kind(connection.GetAdapterId()?) else { return Ok(None) };
            let profile = connection.GetNetwork()?;
            let id = format!("{:?}", profile.GetNetworkId()?);
            let name = profile.GetName().ok().map(|s| s.to_string()).filter(|s| !s.is_empty());
            Ok(Some((format!("{:?}", connection.GetConnectionId()?), NetConnection { id, name, kind })))
        }
    }

    fn read() -> Result<Vec<(String, NetConnection)>> {
        let mut out = Vec::new();
        // SAFETY: this thread owns the COM apartment and all interfaces.
        // Next has one writable, initially empty slot and a live fetched count.
        unsafe {
            let manager: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)?;
            let connections = manager.GetNetworkConnections()?;
            loop {
                let mut slot = [None];
                let mut fetched = 0;
                connections.Next(&mut slot, Some(&mut fetched))?;
                let Some(connection) = slot[0].take() else { break };
                // An adapter can disappear between Next and GetNetwork.
                if let Ok(Some(info)) = connection_info(connection) {
                    out.push(info);
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn connected_profiles_have_ids() {
            // Also exercise enumeration directly on a COM thread; the cache
            // can legitimately be empty before its first refresh.
            let platform = crate::platform::win::platform();
            assert!(platform.network.connections().iter().all(|c| !c.id.is_empty()));
            let _apartment = Apartment::enter().unwrap();
            match read() {
                Ok(list) => assert!(list.iter().all(|(_, c)| !c.id.is_empty())),
                Err(e) => eprintln!("Windows network profiles unavailable: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_virtual_and_loopback_adapters() {
        assert_eq!(adapter_kind(71, "Intel Wi-Fi 6"), Some(ConnKind::WiFi));
        assert_eq!(adapter_kind(6, "Realtek PCIe GbE"), Some(ConnKind::Wired));
        for name in [
            "WireGuard",
            "Wintun",
            "TAP-Windows Adapter V9",
            "TAP",
            "OpenVPN",
            "Tailscale",
            "ZeroTier One",
            "Hyper-V Virtual Ethernet",
        ] {
            assert_eq!(adapter_kind(6, name), Some(ConnKind::Other), "{name}");
        }
        for ty in [1, 23, 131] {
            assert_eq!(adapter_kind(ty, ""), Some(ConnKind::Other));
        }
        assert_eq!(adapter_kind(24, ""), None);
        assert_eq!(adapter_kind(6, "Microsoft KM-TEST Loopback Adapter"), None);
    }
}
