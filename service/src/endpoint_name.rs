//! Set the Windows-visible friendly name of our virtual audio
//! endpoint at runtime to reflect the speaker currently being
//! streamed to.
//!
//! Plumbs `IPolicyConfig::SetPropertyValue(PKEY_Device_FriendlyName)`
//! from Rust — the same RPC the Sound Settings "Rename" button uses,
//! routed through `audiosrv`. The post-install `Rename-Endpoint.ps1`
//! script does the same dance from PowerShell; this module mirrors
//! it so we can update on every `App::select_speaker` instead of only
//! at install time.
//!
//! Best-effort throughout — failures (older Windows, audiosrv hung,
//! endpoint not yet enrolled) are logged at debug level and ignored.
//! The endpoint name is a purely cosmetic Windows-side label;
//! audio still flows.

#![cfg(windows)]

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr;

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::BOOL;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    RegQueryValueExW, REG_SZ,
};

const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;
const RPC_E_CHANGED_MODE: HRESULT = -2147417850; // 0x80010106

// IPolicyConfig — undocumented but stable Vista+. Documented usage
// in github.com/frgnca/AudioDeviceCmdlets and others. We only need
// SetPropertyValue (vtable slot 12).
const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID {
    data1: 0x870af99c,
    data2: 0x171d,
    data3: 0x4f9e,
    data4: [0xaf, 0x0d, 0xe6, 0x3d, 0xf4, 0x0c, 0x2b, 0xc9],
};

const IID_I_POLICY_CONFIG: GUID = GUID {
    data1: 0xf8679f50,
    data2: 0x850a,
    data3: 0x41cf,
    data4: [0x9c, 0x72, 0x43, 0x0f, 0x29, 0x02, 0x90, 0xc8],
};

const PKEY_DEVICE_FMTID: GUID = GUID {
    data1: 0xa45c254e,
    data2: 0xdf1c,
    data3: 0x4efd,
    data4: [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
};
const PKEY_DEVICE_FRIENDLY_NAME_PID: u32 = 14;

const VT_LPWSTR: u16 = 31;

#[repr(C)]
struct PropertyKey {
    fmtid: GUID,
    pid: u32,
}

#[repr(C)]
struct PropVariant {
    vt: u16,
    _r1: u16,
    _r2: u16,
    _r3: u16,
    data: *mut u16,
    _data2: i32,
}

// Vtable — only SetPropertyValue is typed. Padding fills the 13
// IPolicyConfig slots; SetPropertyValue is at index 12.
#[repr(C)]
struct IPolicyConfigVtbl {
    query_interface: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _slot3: *const c_void,
    _slot4: *const c_void,
    _slot5: *const c_void,
    _slot6: *const c_void,
    _slot7: *const c_void,
    _slot8: *const c_void,
    _slot9: *const c_void,
    _slot10: *const c_void,
    _slot11: *const c_void,
    set_property_value: unsafe extern "system" fn(
        *mut c_void,
        *const u16,
        BOOL,
        *const PropertyKey,
        *const PropVariant,
    ) -> HRESULT,
    _slot13: *const c_void,
    _slot14: *const c_void,
}

#[repr(C)]
struct IPolicyConfig {
    vtbl: *const IPolicyConfigVtbl,
}

const ARROW: &str = " \u{2192} "; // " → "

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Update the friendly name of the Stream To Speaker endpoint to
/// reflect the active speaker. Pass `Some(name)` while streaming,
/// `None` to revert to the default. Logs at warn-level on failure;
/// caller does not need to handle the error.
pub fn update_endpoint_name(active_speaker: Option<&str>) {
    let new_name = match active_speaker {
        Some(s) => format!("Stream To Speaker{}{}", ARROW, s),
        None => "Stream To Speaker".to_string(),
    };
    // The whole thing — registry scan, COM init, IPC — can take
    // 100+ ms; do it on a detached thread so callers (GUI / select_
    // speaker) don't block.
    std::thread::Builder::new()
        .name("stream-to-speaker-rename-endpoint".to_string())
        .spawn(move || {
            if let Err(e) = do_update(&new_name) {
                log::debug!("update_endpoint_name failed (cosmetic): {:#}", e);
            }
        })
        .ok();
}

fn do_update(new_name: &str) -> Result<()> {
    let Some(endpoint_id) = find_our_endpoint_id()? else {
        bail!("no Stream To Speaker endpoint found in MMDevices");
    };
    set_endpoint_friendly_name(&endpoint_id, new_name)
}

/// Scan `HKLM\...\MMDevices\Audio\Render\*` and return the device id
/// (`{0.0.0.00000000}.{<guid>}`) of the first endpoint whose
/// DeviceDesc contains "Stream To Speaker". Returns None if no such
/// endpoint exists.
///
/// `pub(crate)` so `endpoint_volume` can reuse it to locate the same
/// endpoint for `IAudioEndpointVolume`.
pub(crate) fn find_our_endpoint_id() -> Result<Option<String>> {
    let base = to_wide_null(r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render");
    let mut render_key: HKEY = ptr::null_mut();
    let r = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            base.as_ptr(),
            0,
            KEY_READ,
            &mut render_key,
        )
    };
    if r != 0 {
        bail!("RegOpenKeyExW(MMDevices\\Render): {}", r);
    }
    let _guard = CloseKeyOnDrop(render_key);

    let mut idx = 0u32;
    loop {
        let mut name_buf = [0u16; 128];
        let mut name_len = name_buf.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                render_key,
                idx,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        // ERROR_NO_MORE_ITEMS = 259
        if r == 259 {
            return Ok(None);
        }
        if r != 0 {
            bail!("RegEnumKeyExW: {}", r);
        }
        idx += 1;
        let guid_str = String::from_utf16_lossy(&name_buf[..name_len as usize]);

        let props_path = to_wide_null(&format!(r"{}\Properties", &guid_str));
        let mut props_key: HKEY = ptr::null_mut();
        let r = unsafe {
            RegOpenKeyExW(
                render_key,
                props_path.as_ptr(),
                0,
                KEY_READ,
                &mut props_key,
            )
        };
        if r != 0 {
            continue;
        }
        let _guard2 = CloseKeyOnDrop(props_key);

        let desc_pkey =
            to_wide_null("{a45c254e-df1c-4efd-8020-67d146a850e0},2");
        let mut data_type: u32 = 0;
        let mut buf = [0u8; 1024];
        let mut len = buf.len() as u32;
        let r = unsafe {
            RegQueryValueExW(
                props_key,
                desc_pkey.as_ptr(),
                ptr::null_mut(),
                &mut data_type,
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        if r != 0 || data_type != REG_SZ {
            continue;
        }
        let u16_slice = unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, (len / 2) as usize)
        };
        // Trim trailing nulls.
        let stop = u16_slice.iter().position(|&c| c == 0).unwrap_or(u16_slice.len());
        let desc = String::from_utf16_lossy(&u16_slice[..stop]);

        if desc.contains("Stream To Speaker") {
            return Ok(Some(format!(r"{{0.0.0.00000000}}.{}", &guid_str)));
        }
    }
}

struct CloseKeyOnDrop(HKEY);
impl Drop for CloseKeyOnDrop {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn set_endpoint_friendly_name(endpoint_id: &str, new_name: &str) -> Result<()> {
    unsafe {
        let hr = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let initialized_here = hr == S_OK;
        if hr != S_OK && hr != S_FALSE && hr != RPC_E_CHANGED_MODE {
            bail!("CoInitializeEx: 0x{:08x}", hr);
        }
        let result: Result<()> = (|| {
            let mut client: *mut c_void = ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_POLICY_CONFIG_CLIENT,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_I_POLICY_CONFIG,
                &mut client,
            );
            if hr < 0 {
                bail!("CoCreateInstance(PolicyConfig): 0x{:08x}", hr);
            }
            let pc = client as *mut IPolicyConfig;
            let key = PropertyKey {
                fmtid: PKEY_DEVICE_FMTID,
                pid: PKEY_DEVICE_FRIENDLY_NAME_PID,
            };
            let name_wide = to_wide_null(new_name);
            let pv = PropVariant {
                vt: VT_LPWSTR,
                _r1: 0,
                _r2: 0,
                _r3: 0,
                data: name_wide.as_ptr() as *mut u16,
                _data2: 0,
            };
            let endpoint_id_wide = to_wide_null(endpoint_id);
            let hr = ((*(*pc).vtbl).set_property_value)(
                pc as *mut c_void,
                endpoint_id_wide.as_ptr(),
                0, // bFxStore = FALSE → endpoint store, not FX store
                &key,
                &pv,
            );
            ((*(*pc).vtbl).release)(pc as *mut c_void);
            if hr < 0 {
                bail!("IPolicyConfig::SetPropertyValue: 0x{:08x}", hr);
            }
            Ok(())
        })();
        if initialized_here {
            CoUninitialize();
        }
        result
    }
}
