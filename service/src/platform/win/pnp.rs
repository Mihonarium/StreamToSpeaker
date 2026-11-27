//! Find present root devices and the render endpoints attached to them.

use anyhow::{bail, ensure, Result};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_Parent, DEVPROP_TYPE_STRING};
use windows_sys::Win32::System::Registry::REG_MULTI_SZ;

pub(super) const HARDWARE_ID: &str = r"Root\StreamToSpeaker";

fn check(operation: &str, code: CONFIGRET) -> Result<()> {
    ensure!(code == CR_SUCCESS, "{operation} returned CONFIGRET {code}");
    Ok(())
}

fn has_hardware_id(node: u32) -> Result<bool> {
    let mut ids = vec![0u16; 256];
    loop {
        let mut bytes = (ids.len() * 2) as u32;
        let mut kind = 0;
        // SAFETY: ids is aligned for REG_MULTI_SZ and bytes is its writable capacity.
        let code = unsafe {
            CM_Get_DevNode_Registry_PropertyW(
                node,
                CM_DRP_HARDWAREID,
                &mut kind,
                ids.as_mut_ptr().cast(),
                &mut bytes,
                0,
            )
        };
        match code {
            CR_BUFFER_SMALL => ids.resize((bytes as usize).div_ceil(2), 0),
            CR_NO_SUCH_VALUE => return Ok(false),
            _ => {
                check("Read device hardware IDs", code)?;
                ensure!(
                    kind == REG_MULTI_SZ && bytes % 2 == 0,
                    "Invalid hardware ID list"
                );
                return Ok(ids[..bytes as usize / 2]
                    .split(|c| *c == 0)
                    .any(|id| String::from_utf16_lossy(id).eq_ignore_ascii_case(HARDWARE_ID)));
            }
        }
    }
}

/// The software device for a Core Audio endpoint is a child of its audio adapter.
pub(super) fn endpoint_parent(endpoint: &str) -> Result<Option<u32>> {
    let instance: Vec<_> = format!(r"SWD\MMDEVAPI\{endpoint}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut node = 0;
    let mut parent = 0;
    let mut parent_id = [0u16; MAX_DEVICE_ID_LEN as usize];
    let mut bytes = size_of_val(&parent_id) as u32;
    let mut kind = 0;
    // SAFETY: instance is terminated. CfgMgr32 bounds its property write by bytes.
    // The returned parent ID must be a terminated string before we pass it back.
    unsafe {
        check("Locate endpoint", CM_Locate_DevNodeW(&mut node, instance.as_ptr(), CM_LOCATE_DEVNODE_PHANTOM))?;
        check("Read endpoint parent", CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_Parent,
            &mut kind,
            parent_id.as_mut_ptr().cast(),
            &mut bytes,
            0,
        ))?;
        let units = bytes as usize / 2;
        ensure!(kind == DEVPROP_TYPE_STRING && bytes % 2 == 0 && units > 1 && parent_id[0] != 0
            && parent_id.get(units - 1) == Some(&0), "Invalid parent ID for endpoint {endpoint}");
        check("Locate endpoint parent", CM_Locate_DevNodeW(&mut parent, parent_id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL))?;
    }
    Ok(has_hardware_id(parent)?.then_some(parent))
}

fn status(node: u32) -> Result<(u32, u32)> {
    let mut flags = 0;
    let mut problem = 0;
    // SAFETY: node is a CfgMgr32 devnode and both output words are writable.
    check("Read device status", unsafe {
        CM_Get_DevNode_Status(&mut flags, &mut problem, node, 0)
    })?;
    Ok((flags, problem))
}

pub(super) fn working(node: u32) -> bool {
    status(node).is_ok_and(|(flags, problem)| {
        problem == 0 && flags & (DN_STARTED | DN_HAS_PROBLEM) == DN_STARTED
    })
}

pub(super) struct Device {
    pub instance: String,
    pub node: u32,
    pub problem: u32,
    flags: u32,
}

impl Device {
    pub fn enabled_but_not_working(&self) -> bool {
        !self.disabled()
            && (self.problem != 0 || self.flags & (DN_STARTED | DN_HAS_PROBLEM) != DN_STARTED)
    }

    pub fn disabled(&self) -> bool {
        self.problem == CM_PROB_DISABLED
    }

    pub fn enable(&self) -> Result<()> {
        // SAFETY: node identifies the present device selected by hardware ID.
        check("Enable device", unsafe { CM_Enable_DevNode(self.node, 0) })
    }
}

pub(super) fn devices() -> Result<Vec<Device>> {
    let flags = CM_GETIDLIST_FILTER_PRESENT | CM_GETIDLIST_FILTER_ENUMERATOR;
    let root = windows_sys::core::w!("ROOT");
    let ids = loop {
        let mut size = 0;
        // SAFETY: root is NUL-terminated and size receives the required UTF-16 count.
        check("Size root device list", unsafe {
            CM_Get_Device_ID_List_SizeW(&mut size, root, flags)
        })?;
        let mut ids = vec![0u16; size as usize];
        // SAFETY: ids holds size UTF-16 units. Retry if the set grew between calls.
        let code = unsafe { CM_Get_Device_ID_ListW(root, ids.as_mut_ptr(), size, flags) };
        if code != CR_BUFFER_SMALL {
            check("List present root devices", code)?;
            break ids;
        }
    };
    let mut found = Vec::new();
    for id in ids
        .split_inclusive(|c| *c == 0)
        .take_while(|id| id.len() > 1)
    {
        if id.last() != Some(&0) {
            bail!("Unterminated device instance ID");
        }
        let mut node = 0;
        // SAFETY: id includes its terminator and node receives a present devnode.
        check("Locate root device", unsafe {
            CM_Locate_DevNodeW(&mut node, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL)
        })?;
        if has_hardware_id(node)? {
            let (flags, problem) = status(node)?;
            found.push(Device {
                node,
                problem,
                flags,
                instance: String::from_utf16(&id[..id.len() - 1])?,
            });
        }
    }
    Ok(found)
}
