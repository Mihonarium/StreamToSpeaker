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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::latest::Latest;

use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::BOOL;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW, REG_SZ,
};

const S_OK: HRESULT = 0;
const S_FALSE: HRESULT = 1;
const RPC_E_CHANGED_MODE: HRESULT = -2147417850; // 0x80010106

// IPolicyConfig — undocumented but stable Vista+. Documented usage
// in github.com/frgnca/AudioDeviceCmdlets and others. We need
// SetPropertyValue (slot 12), SetDefaultEndpoint (13) and SetEndpointVisibility (14).
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

// Vtable — only the slots called are typed. Padding fills the others;
// the final three entries set the name, default output and visibility.
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
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
    set_endpoint_visibility: unsafe extern "system" fn(*mut c_void, *const u16, BOOL) -> HRESULT,
}

#[repr(C)]
struct IPolicyConfig {
    vtbl: *const IPolicyConfigVtbl,
}

const ARROW: &str = " \u{2192} "; // " → "

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Set once the platform seam names the device ("Kitchen (Stream To
/// Speaker)"): from then on the older label path does nothing.
static PLATFORM_NAMES_IT: AtomicBool = AtomicBool::new(false);

/// The platform seam names the device from now on.
pub(crate) fn platform_names_the_device() {
    PLATFORM_NAMES_IT.store(true, Ordering::Release);
}

/// One worker renames the device, always to the latest name asked for:
/// renames asked for in quick succession (a bind, then Disable) can't
/// finish out of order, and a name counts as done only once it was set.
fn renames() -> &'static Latest<String> {
    static RENAMES: OnceLock<Latest<String>> = OnceLock::new();
    RENAMES.get_or_init(|| {
        Latest::new("stream-to-speaker-rename-endpoint", |new_name: &String| {
            match do_update(new_name) {
                Ok(()) => true,
                Err(e) => {
                    // Warn, not debug. This is cosmetic enough to ignore at
                    // runtime but not so cosmetic that it should be invisible:
                    // it went unnoticed for a long time precisely because the
                    // one failure that mattered never showed up in the log.
                    log::warn!("could not rename the Windows endpoint (cosmetic): {:#}", e);
                    false
                }
            }
        })
    })
}

/// Update the friendly name of the Stream To Speaker endpoint to
/// reflect the active speaker. Pass `Some(name)` while streaming,
/// `None` to revert to the default. Logs at warn-level on failure;
/// caller does not need to handle the error. Returns at once: the whole
/// thing (registry scan, COM init, IPC) can take 100+ ms.
pub fn update_endpoint_name(active_speaker: Option<&str>) {
    if PLATFORM_NAMES_IT.load(Ordering::Acquire) {
        return;
    }
    renames().want(label(active_speaker));
}

/// [`update_endpoint_name`], waiting up to `max` for the rename: at quit,
/// where the process would otherwise end it half-way.
pub fn update_endpoint_name_within(active_speaker: Option<&str>, max: Duration) {
    if PLATFORM_NAMES_IT.load(Ordering::Acquire) {
        return;
    }
    let ticket = renames().want(label(active_speaker));
    renames().wait(ticket, max);
}

fn label(active_speaker: Option<&str>) -> String {
    match active_speaker {
        Some(s) => format!("Stream To Speaker{}{}", ARROW, s),
        None => "Stream To Speaker".to_string(),
    }
}

/// Name the device `name`, as it is; returns at once. The ticket waits
/// for it (see [`wait_for_rename`]).
pub(crate) fn rename_endpoint(name: &str) -> u64 {
    renames().want(name.to_string())
}

/// Wait up to `max` for rename `ticket` to be done.
pub(crate) fn wait_for_rename(ticket: u64, max: Duration) {
    renames().wait(ticket, max);
}

/// Name the device `name` now, on this thread, without the worker: for a
/// crash and for Windows ending the session, where nothing else will run.
pub(crate) fn rename_endpoint_now(name: &str) -> Result<()> {
    let Some(endpoint_id) = find_our_endpoint_id()? else { bail!("no Stream To Speaker endpoint") };
    set_endpoint_friendly_name(&endpoint_id, name)
}

fn do_update(new_name: &str) -> Result<()> {
    let Some(endpoint_id) = find_our_endpoint_id()? else {
        bail!(
            "no Stream To Speaker endpoint found in MMDevices\\Audio\\Render \
             (is the driver installed and the endpoint enrolled?)"
        );
    };
    set_endpoint_friendly_name(&endpoint_id, new_name)?;

    // Read the name back. IPolicyConfig returning S_OK is not proof the
    // name stuck: it is an RPC into audiosrv, which can accept the call
    // and then have AudioEndpointBuilder recompose the name from the pin
    // category afterwards. Without this check a silent recompose looks
    // exactly like success, which is how the original bug hid for so
    // long — the failing step reported nothing at all.
    match endpoint_friendly_name(&endpoint_id) {
        Some(actual) if actual == new_name => {
            log::info!("Windows endpoint renamed to {:?}", actual);
        }
        Some(actual) => {
            log::warn!(
                "endpoint rename did not stick: asked for {:?}, Windows kept {:?}",
                new_name,
                actual
            );
        }
        None => log::warn!("endpoint renamed but the name could not be read back"),
    }
    Ok(())
}

/// Read the render endpoint's friendly name back after naming it.
pub(crate) fn endpoint_friendly_name(endpoint_id: &str) -> Option<String> {
    let (_, guid) = endpoint_id.rsplit_once('.')?;
    let path = format!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{}\Properties",
        guid
    );
    let path_w = to_wide_null(&path);
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: the terminated path stays live and key receives the opened handle.
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path_w.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return None;
    }
    let _guard = CloseKeyOnDrop(key);
    read_string_value(key, "{a45c254e-df1c-4efd-8020-67d146a850e0},14")
}

/// Read one REG_SZ property value out of an endpoint's Properties key.
fn read_string_value(key: HKEY, pkey: &str) -> Option<String> {
    let name = to_wide_null(pkey);
    let mut data_type: u32 = 0;
    let mut buf = [0u16; 512];
    let mut len = size_of_val(&buf) as u32;
    // SAFETY: key stays open and len is the byte capacity of the aligned buffer.
    let r = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            ptr::null_mut(),
            &mut data_type,
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if r != 0 || data_type != REG_SZ {
        return None;
    }
    let u16_slice = &buf[..len as usize / 2];
    let stop = u16_slice.iter().position(|&c| c == 0).unwrap_or(u16_slice.len());
    Some(String::from_utf16_lossy(&u16_slice[..stop]))
}

/// Find the active render endpoint whose parent carries the driver's hardware ID.
pub(crate) fn find_our_endpoint_id() -> Result<Option<String>> {
    crate::platform::win::active_endpoint(|_| true)
}

struct CloseKeyOnDrop(HKEY);
impl Drop for CloseKeyOnDrop {
    fn drop(&mut self) {
        // SAFETY: this guard owns the open registry key.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Make `endpoint_id` Windows' default output for `role` (0 console, 1
/// multimedia, 2 communications), through the same interface Sound
/// settings uses.
pub(crate) fn set_default_endpoint(endpoint_id: &str, role: i32) -> Result<()> {
    with_policy_config(|pc| {
        let id = to_wide_null(endpoint_id);
        // SAFETY: `pc` is a live IPolicyConfig from CoCreateInstance (see
        // `with_policy_config`), and `id` is a NUL-terminated UTF-16 string
        // that outlives the call.
        let hr = unsafe { ((*(*pc).vtbl).set_default_endpoint)(pc as *mut c_void, id.as_ptr(), role) };
        if hr < 0 {
            bail!("IPolicyConfig::SetDefaultEndpoint: 0x{:08x}", hr);
        }
        Ok(())
    })
}

/// Turn on the render endpoint the audio worker found in Windows' disabled list.
pub(crate) fn turn_on_endpoint(endpoint_id: &str) -> Result<()> {
    with_policy_config(|pc| {
        let id = to_wide_null(endpoint_id);
        // SAFETY: the closure holds a live policy client and id is NUL-terminated for the call.
        let hr = unsafe { ((*(*pc).vtbl).set_endpoint_visibility)(pc.cast(), id.as_ptr(), 1) };
        if hr < 0 { bail!("IPolicyConfig::SetEndpointVisibility: 0x{:08x}", hr); }
        Ok(())
    })
}

/// Run `f` with an IPolicyConfig, COM initialised on this thread for the
/// call (an apartment already set up is used as it is).
fn with_policy_config(f: impl FnOnce(*mut IPolicyConfig) -> Result<()>) -> Result<()> {
    // SAFETY: CoInitializeEx and CoUninitialize are balanced below;
    // CoCreateInstance writes one interface pointer, released before the
    // closure returns and never used after.
    unsafe {
        let hr = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let initialized_here = hr == S_OK || hr == S_FALSE;
        if hr != S_OK && hr != S_FALSE && hr != RPC_E_CHANGED_MODE {
            bail!("CoInitializeEx: 0x{:08x}", hr);
        }
        let mut client: *mut c_void = ptr::null_mut();
        let hr = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, ptr::null_mut(), CLSCTX_ALL, &IID_I_POLICY_CONFIG, &mut client);
        let result = if hr < 0 {
            Err(anyhow::anyhow!("CoCreateInstance(PolicyConfig): 0x{:08x}", hr))
        } else {
            let pc = client as *mut IPolicyConfig;
            let r = f(pc);
            ((*(*pc).vtbl).release)(pc as *mut c_void);
            r
        };
        if initialized_here {
            CoUninitialize();
        }
        result
    }
}

pub(crate) fn set_endpoint_friendly_name(endpoint_id: &str, new_name: &str) -> Result<()> {
    with_policy_config(|pc| {
        let key = PropertyKey { fmtid: PKEY_DEVICE_FMTID, pid: PKEY_DEVICE_FRIENDLY_NAME_PID };
        let name = to_wide_null(new_name);
        let pv = PropVariant { vt: VT_LPWSTR, _r1: 0, _r2: 0, _r3: 0, data: name.as_ptr() as *mut u16, _data2: 0 };
        let id = to_wide_null(endpoint_id);
        // SAFETY: pc is live and the endpoint id, property key and string value outlive the call.
        let hr = unsafe { ((*(*pc).vtbl).set_property_value)(pc.cast(), id.as_ptr(), 0, &key, &pv) };
        if hr < 0 { bail!("IPolicyConfig::SetPropertyValue: 0x{:08x}", hr); }
        Ok(())
    })
}
