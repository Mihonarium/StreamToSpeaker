//! Read and write the *real* Windows volume-slider position of our
//! virtual endpoint via Core Audio's `IAudioEndpointVolume`.
//!
//! ## Why this module exists
//!
//! The position of the Windows volume slider (Sound Settings, the tray
//! flyout, the volume mixer) is the `IAudioEndpointVolume` **scalar** —
//! a value in `0.0..=1.0`. When the user drags that slider, the audio
//! engine converts the scalar to a dB value through a proprietary
//! "audio-tapered" curve and writes *that* to our driver's hardware
//! volume node (`KSPROPERTY_AUDIO_VOLUMELEVEL`). We then receive the dB
//! over the IOCTL event channel.
//!
//! That curve is deliberately undocumented and "might change in future
//! versions of Windows":
//! <https://learn.microsoft.com/windows/win32/coreaudio/audio-tapered-volume-controls>.
//! So trying to recover the slider percentage by inverting the dB with a
//! fixed formula is guaranteed to be wrong — 50 % on the slider is about
//! −10 dB, not the −6 dB a naive amplitude formula predicts, and not the
//! −48 dB a linear-in-dB mapping predicts. That mismatch is exactly the
//! non-linear slider→speaker correspondence we were seeing.
//!
//! The fix is to ask Windows for the scalar directly: `scalar * 100` *is*
//! the slider percentage, by definition, whatever the curve happens to
//! be. We use it for both directions — read it to mirror the slider onto
//! the speaker, and set it to mirror an external speaker change back onto
//! the slider.
//!
//! Best-effort: every entry point degrades to `None` / `Err` so callers
//! can fall back to the (imperfect) dB formula or the IOCTL push if the
//! endpoint can't be reached.

#[cfg(windows)]
pub use imp::{master_scalar_percent, set_master_scalar_percent};

/// Current Windows slider position of our endpoint as a `0..=100`
/// percent, or `None` on non-Windows / when the endpoint can't be
/// queried. On non-Windows this is always `None`.
#[cfg(not(windows))]
pub fn master_scalar_percent() -> Option<u32> {
    None
}

/// Move the Windows slider of our endpoint to `percent` (`0..=100`).
/// On non-Windows this always errors so callers fall back.
#[cfg(not(windows))]
pub fn set_master_scalar_percent(_percent: u32) -> anyhow::Result<()> {
    anyhow::bail!("endpoint volume control is only available on Windows")
}

#[cfg(windows)]
mod imp {
    use anyhow::{bail, Result};
    use std::ffi::c_void;
    use std::ptr;

    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };

    const S_OK: HRESULT = 0;
    const S_FALSE: HRESULT = 1;
    const RPC_E_CHANGED_MODE: HRESULT = -2147417850; // 0x80010106

    // CLSID_MMDeviceEnumerator {BCDE0395-E52F-467C-8E3D-C4579291692E}
    const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID {
        data1: 0xBCDE0395,
        data2: 0xE52F,
        data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    // IID_IMMDeviceEnumerator {A95664D2-9614-4F35-A746-DE8DB63617E6}
    const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID {
        data1: 0xA95664D2,
        data2: 0x9614,
        data3: 0x4F35,
        data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    // IID_IAudioEndpointVolume {5CDF2C82-841E-4546-9722-0CF74078229A}
    const IID_IAUDIO_ENDPOINT_VOLUME: GUID = GUID {
        data1: 0x5CDF2C82,
        data2: 0x841E,
        data3: 0x4546,
        data4: [0x97, 0x22, 0x0C, 0xF7, 0x40, 0x78, 0x22, 0x9A],
    };

    // --- COM vtable shapes. We spell out only the IUnknown trio (shared
    // by all three interfaces) plus the exact slots we call; every other
    // slot is an opaque pointer so the field offsets stay correct without
    // typing signatures we never invoke. Same approach as the
    // IPolicyConfig vtable in endpoint_name.rs. ---

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    // IMMDeviceEnumerator — GetDevice is vtable slot 5.
    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        base: IUnknownVtbl,
        enum_audio_endpoints: *const c_void,       // 3
        get_default_audio_endpoint: *const c_void, // 4
        get_device: unsafe extern "system" fn(     // 5
            *mut c_void,
            *const u16,
            *mut *mut c_void,
        ) -> HRESULT,
        register_endpoint_notification_callback: *const c_void,
        unregister_endpoint_notification_callback: *const c_void,
    }

    // IMMDevice — Activate is vtable slot 3.
    #[repr(C)]
    struct IMMDeviceVtbl {
        base: IUnknownVtbl,
        activate: unsafe extern "system" fn(       // 3
            *mut c_void,
            *const GUID,
            u32,
            *const c_void,
            *mut *mut c_void,
        ) -> HRESULT,
        open_property_store: *const c_void,
        get_id: *const c_void,
        get_state: *const c_void,
    }

    // IAudioEndpointVolume — SetMasterVolumeLevelScalar is slot 7,
    // GetMasterVolumeLevelScalar is slot 9. Truncated after slot 9: we
    // never touch the later methods, and we only ever cast a live
    // pointer onto this struct (never construct one), so a shorter
    // definition is sound as long as the offsets we read are right.
    #[repr(C)]
    struct IAudioEndpointVolumeVtbl {
        base: IUnknownVtbl,
        register_control_change_notify: *const c_void,   // 3
        unregister_control_change_notify: *const c_void, // 4
        get_channel_count: *const c_void,                // 5
        set_master_volume_level: *const c_void,          // 6
        set_master_volume_level_scalar:                  // 7
            unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> HRESULT,
        get_master_volume_level: *const c_void,          // 8
        get_master_volume_level_scalar:                  // 9
            unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
    }

    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Release a COM object through its IUnknown slot. All three
    /// interfaces share the IUnknown layout, so one helper covers them.
    unsafe fn com_release(obj: *mut c_void) {
        if !obj.is_null() {
            let vtbl = *(obj as *const *const IUnknownVtbl);
            ((*vtbl).release)(obj);
        }
    }

    /// RAII `Release` guard for a raw COM pointer.
    struct ComPtr(*mut c_void);
    impl Drop for ComPtr {
        fn drop(&mut self) {
            unsafe { com_release(self.0) }
        }
    }

    /// Locate our endpoint, activate `IAudioEndpointVolume`, and run `f`
    /// with the live interface pointer + its vtable. COM is initialised
    /// per call (apartment-threaded) so this is safe to call from any
    /// thread — the driver-event thread reads, the GENA thread writes —
    /// mirroring the per-call pattern in endpoint_name.rs.
    fn with_endpoint_volume<T>(
        f: impl FnOnce(*mut c_void, &IAudioEndpointVolumeVtbl) -> Result<T>,
    ) -> Result<T> {
        let endpoint_id = match crate::endpoint_name::find_our_endpoint_id()? {
            Some(id) => id,
            None => bail!("Stream To Speaker endpoint not found in MMDevices"),
        };
        unsafe {
            let hr = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let initialized_here = hr == S_OK;
            if hr != S_OK && hr != S_FALSE && hr != RPC_E_CHANGED_MODE {
                bail!("CoInitializeEx: 0x{:08x}", hr);
            }
            let result: Result<T> = (|| {
                // 1. Create the device enumerator.
                let mut enum_raw: *mut c_void = ptr::null_mut();
                let hr = CoCreateInstance(
                    &CLSID_MM_DEVICE_ENUMERATOR,
                    ptr::null_mut(),
                    CLSCTX_ALL,
                    &IID_IMM_DEVICE_ENUMERATOR,
                    &mut enum_raw,
                );
                if hr < 0 {
                    bail!("CoCreateInstance(MMDeviceEnumerator): 0x{:08x}", hr);
                }
                let _enum_guard = ComPtr(enum_raw);
                let enum_vtbl = &*(*(enum_raw as *const *const IMMDeviceEnumeratorVtbl));

                // 2. Resolve our endpoint id to an IMMDevice.
                let id_wide = to_wide_null(&endpoint_id);
                let mut dev_raw: *mut c_void = ptr::null_mut();
                let hr = (enum_vtbl.get_device)(enum_raw, id_wide.as_ptr(), &mut dev_raw);
                if hr < 0 {
                    bail!("IMMDeviceEnumerator::GetDevice: 0x{:08x}", hr);
                }
                let _dev_guard = ComPtr(dev_raw);
                let dev_vtbl = &*(*(dev_raw as *const *const IMMDeviceVtbl));

                // 3. Activate the endpoint-volume interface.
                let mut vol_raw: *mut c_void = ptr::null_mut();
                let hr = (dev_vtbl.activate)(
                    dev_raw,
                    &IID_IAUDIO_ENDPOINT_VOLUME,
                    CLSCTX_ALL,
                    ptr::null(),
                    &mut vol_raw,
                );
                if hr < 0 {
                    bail!("IMMDevice::Activate(IAudioEndpointVolume): 0x{:08x}", hr);
                }
                let _vol_guard = ComPtr(vol_raw);
                let vol_vtbl = &*(*(vol_raw as *const *const IAudioEndpointVolumeVtbl));

                f(vol_raw, vol_vtbl)
            })();
            if initialized_here {
                CoUninitialize();
            }
            result
        }
    }

    /// Current Windows slider position of our endpoint as a `0..=100`
    /// percent, or `None` if the endpoint can't be queried.
    pub fn master_scalar_percent() -> Option<u32> {
        let scalar = with_endpoint_volume(|vol, vtbl| unsafe {
            let mut scalar: f32 = 0.0;
            let hr = (vtbl.get_master_volume_level_scalar)(vol, &mut scalar);
            if hr < 0 {
                bail!("GetMasterVolumeLevelScalar: 0x{:08x}", hr);
            }
            Ok(scalar)
        });
        match scalar {
            Ok(s) => Some((s.clamp(0.0, 1.0) * 100.0).round() as u32),
            Err(e) => {
                log::debug!("read endpoint volume scalar failed: {:#}", e);
                None
            }
        }
    }

    /// Move the Windows slider of our endpoint to `percent` (`0..=100`).
    /// Windows then derives the matching dB for our driver node, so the
    /// slider and the node stay consistent.
    pub fn set_master_scalar_percent(percent: u32) -> Result<()> {
        let scalar = (percent.min(100) as f32) / 100.0;
        with_endpoint_volume(|vol, vtbl| unsafe {
            // Null event-context GUID: we don't need change-origin
            // tracking here — echo suppression lives in VolumeSync.
            let hr = (vtbl.set_master_volume_level_scalar)(vol, scalar, ptr::null());
            if hr < 0 {
                bail!("SetMasterVolumeLevelScalar: 0x{:08x}", hr);
            }
            Ok(())
        })
    }
}
