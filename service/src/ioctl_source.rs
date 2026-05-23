//! AudioSource backed by the StreamToSpeaker virtual driver via DeviceIoControl.
//!
//! Implements the contract in `include/stream_to_speaker_ioctl.h`. Layout of the
//! kernel structs is reproduced here in `#[repr(C)]` form. Any change to
//! the C header must be mirrored here.

#![cfg(windows)]

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::{debug, error, info, warn};
use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStringExt;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_NO_MORE_ITEMS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

use crate::audio_source::{AudioPacket, AudioSource, PACKET_FLAG_HINT_SILENT, PACKET_FLAG_STREAM_RESTART};

// -----------------------------------------------------------------------------
// IOCTL contract (mirror of include/stream_to_speaker_ioctl.h)
// -----------------------------------------------------------------------------

const STREAM_TO_SPEAKER_DEVICE_PATH: &str = r"\\.\StreamToSpeaker";
const STREAM_TO_SPEAKER_PROTOCOL_VERSION: u32 = 1;
const STREAM_TO_SPEAKER_MAX_PACKET_BYTES: usize = 8192;

/// {7B3F1F2C-A1A2-4567-89AB-CDEF01234567}
const GUID_DEVINTERFACE_STREAM_TO_SPEAKER: GUID = GUID {
    data1: 0x7B3F1F2C,
    data2: 0xA1A2,
    data3: 0x4567,
    data4: [0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67],
};

// CTL_CODE macro: ((DeviceType << 16) | (Access << 14) | (Function << 2) | Method)
const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
const METHOD_BUFFERED: u32 = 0;
const METHOD_OUT_DIRECT: u32 = 2;
const FILE_READ_ACCESS: u32 = 0x0001;
const FILE_WRITE_ACCESS: u32 = 0x0002;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const STREAM_TO_SPEAKER_IOCTL_BASE: u32 = 0x800;
const IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 1, METHOD_OUT_DIRECT, FILE_READ_ACCESS);
const IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 2, METHOD_BUFFERED, FILE_READ_ACCESS);
const IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 3, METHOD_BUFFERED, FILE_WRITE_ACCESS);
const IOCTL_STREAM_TO_SPEAKER_GET_VERSION: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, STREAM_TO_SPEAKER_IOCTL_BASE + 4, METHOD_BUFFERED, FILE_READ_ACCESS);

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamToSpeakerVersionInfo {
    protocol_version: u32,
    driver_build: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamToSpeakerAudioPacketHeader {
    sample_rate: u32,
    bits_per_sample: u16,
    channels: u16,
    sample_frame_count: u32,
    data_bytes: u32,
    flags: u32,
    timestamp_qpc: u64,
    stream_position: u64,
}

const SIZEOF_PACKET_HEADER: usize = size_of::<StreamToSpeakerAudioPacketHeader>();
const IOCTL_BUFFER_BYTES: usize = SIZEOF_PACKET_HEADER + STREAM_TO_SPEAKER_MAX_PACKET_BYTES;

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamToSpeakerControlEvent {
    event_type: u32,
    _reserved: u32,
    payload: [u8; 16],
}

const STREAM_TO_SPEAKER_EVENT_VOLUME_CHANGED: u32 = 1;
const STREAM_TO_SPEAKER_EVENT_MUTE_CHANGED: u32 = 2;
const STREAM_TO_SPEAKER_EVENT_STREAM_START: u32 = 3;
const STREAM_TO_SPEAKER_EVENT_STREAM_STOP: u32 = 4;
const STREAM_TO_SPEAKER_EVENT_FORMAT_CHANGE: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamToSpeakerPushVolumeInput {
    level_millibels: i32,
    muted: u32,
}

// -----------------------------------------------------------------------------
// Public types: events delivered to the main loop
// -----------------------------------------------------------------------------

/// Decoded control event from the driver.
#[derive(Debug, Clone, Copy)]
pub enum DriverEvent {
    VolumeChanged { level_millibels: i32 },
    MuteChanged { muted: bool },
    StreamStart,
    StreamStop,
    FormatChange { sample_rate: u32, bits_per_sample: u16, channels: u16 },
}

// -----------------------------------------------------------------------------
// Handle wrapper
// -----------------------------------------------------------------------------

/// RAII wrapper around a Windows HANDLE. Shareable across threads as it's
/// just an integer; we synchronize at the IOCTL boundary on the driver side.
#[derive(Clone)]
struct SharedHandle(Arc<HandleInner>);

struct HandleInner(HANDLE);

unsafe impl Send for HandleInner {}
unsafe impl Sync for HandleInner {}

/// Newtype for the MMCSS handle.  HANDLE is *mut c_void which is !Send,
/// but logically we own this handle from the audio thread only and never
/// migrate it; the wrapper lets the enclosing struct still be Send.
struct MmcssHandle(HANDLE);
unsafe impl Send for MmcssHandle {}
unsafe impl Sync for MmcssHandle {}

impl Drop for HandleInner {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: handle was opened by CreateFileW; we own it.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Public source struct
// -----------------------------------------------------------------------------

/// IOCTL-backed audio source. Spawns a control-event reader thread on
/// construction; that thread feeds the `events()` receiver.
pub struct IoctlAudioSource {
    handle: SharedHandle,
    /// Scratch buffer for the IOCTL audio path (one packet).
    scratch_in: Vec<u8>,
    /// Reusable output Vec<i16>.
    scratch_out: Vec<i16>,
    /// Control event channel.
    event_rx: Receiver<DriverEvent>,
    /// Tells the control thread to exit.
    shutdown: Arc<AtomicBool>,
    /// MMCSS handle for the *current* thread (audio thread).  Lazily set on
    /// first `recv_packet` call.
    mmcss_initialised: bool,
    mmcss_handle: MmcssHandle,
    mmcss_task_index: u32,
}

impl IoctlAudioSource {
    /// Open the device via the interface GUID, falling back to the symbolic
    /// link, then run `IOCTL_STREAM_TO_SPEAKER_GET_VERSION` and refuse if the
    /// protocol doesn't match.
    pub fn open() -> Result<Self> {
        let handle = open_device_handle().context("opening StreamToSpeaker device")?;

        // Version handshake.
        let version = ioctl_get_version(handle.0 .0)?;
        if version.protocol_version != STREAM_TO_SPEAKER_PROTOCOL_VERSION {
            bail!(
                "driver protocol version mismatch: driver={} expected={}",
                version.protocol_version,
                STREAM_TO_SPEAKER_PROTOCOL_VERSION
            );
        }
        info!(
            "StreamToSpeaker driver opened (proto={} build={})",
            version.protocol_version, version.driver_build
        );

        // Control event channel + reader thread.
        let (tx, rx) = bounded::<DriverEvent>(64);
        let shutdown = Arc::new(AtomicBool::new(false));
        spawn_control_event_thread(handle.clone(), tx, shutdown.clone());

        Ok(Self {
            handle,
            scratch_in: vec![0u8; IOCTL_BUFFER_BYTES],
            scratch_out: Vec::with_capacity(STREAM_TO_SPEAKER_MAX_PACKET_BYTES / 2),
            event_rx: rx,
            shutdown,
            mmcss_initialised: false,
            mmcss_handle: MmcssHandle(std::ptr::null_mut()),
            mmcss_task_index: 0,
        })
    }

    /// Receiver for control events. The audio-data path is via
    /// `recv_packet`; this is for volume / stream-start / stream-stop.
    pub fn events(&self) -> Receiver<DriverEvent> {
        self.event_rx.clone()
    }

    /// Push a volume / mute update to the driver (so Windows UI reflects an
    /// external Sonos change).
    pub fn push_volume(&self, level_millibels: i32, muted: bool) -> Result<()> {
        let mut input = StreamToSpeakerPushVolumeInput {
            level_millibels,
            muted: if muted { 1 } else { 0 },
        };
        let mut returned: u32 = 0;
        // SAFETY: stack input, no overlapped.
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0 .0,
                IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME,
                (&mut input as *mut _) as *mut _,
                size_of::<StreamToSpeakerPushVolumeInput>() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            bail!("IOCTL_STREAM_TO_SPEAKER_PUSH_VOLUME failed: WinError {}", e);
        }
        Ok(())
    }

    fn ensure_mmcss(&mut self) {
        if self.mmcss_initialised {
            return;
        }
        self.mmcss_initialised = true;

        // "Pro Audio" as a UTF-16 wide string.
        let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut task_index: u32 = 0;
        // SAFETY: Avrt entry point; task_index is a valid u32 ptr.
        let h = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index) };
        if h.is_null() {
            let e = unsafe { GetLastError() };
            warn!("AvSetMmThreadCharacteristics(Pro Audio) failed: WinError {}", e);
        } else {
            debug!("MMCSS Pro Audio acquired on audio thread (task_index={})", task_index);
            self.mmcss_handle = MmcssHandle(h);
            self.mmcss_task_index = task_index;
        }
    }
}

impl Drop for IoctlAudioSource {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if !self.mmcss_handle.0.is_null() {
            // SAFETY: handle obtained from AvSetMmThreadCharacteristicsW.
            unsafe {
                AvRevertMmThreadCharacteristics(self.mmcss_handle.0);
            }
        }
    }
}

impl AudioSource for IoctlAudioSource {
    fn name(&self) -> &str {
        "stream-to-speaker-ioctl"
    }

    fn recv_packet(&mut self) -> Result<AudioPacket> {
        self.ensure_mmcss();

        let mut returned: u32 = 0;
        // SAFETY: scratch_in is sized to IOCTL_BUFFER_BYTES; the driver will
        // write at most that many bytes. METHOD_OUT_DIRECT works fine with
        // a heap buffer because the I/O manager will lock our pages.
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0 .0,
                IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET,
                null_mut(),
                0,
                self.scratch_in.as_mut_ptr() as *mut _,
                self.scratch_in.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            bail!("IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET failed: WinError {}", e);
        }
        if (returned as usize) < SIZEOF_PACKET_HEADER {
            // Driver completes the IRP with 0 bytes on StreamStop (see
            // driver/ioctl.cpp::IoctlOnStreamStop). That happens any
            // time the audio source pauses — e.g. YouTube pause, app
            // switching output device. Treat it as "stream paused" and
            // emit a short silence packet so the loop keeps the HTTP
            // stream alive instead of killing the whole service (which
            // would kill the HTTP server, force Sonos to drop the TCP
            // connection, and require a manual restart).
            //
            // The silence detector downstream injects a low noise floor
            // so Sonos doesn't drop on a long zero run.
            //
            // The next recv_packet will block on a fresh IRP until the
            // next StreamStart fires, so this only emits one silence
            // packet per stop event — enough to keep the connection
            // alive across brief pauses. Continuous-silence-during-
            // pause needs a separate "silence mode" driven by the
            // event channel; defer until single-packet bridging is
            // proven insufficient.
            const SILENCE_FRAMES: usize = 441; // 10 ms at 44.1 kHz
            return Ok(AudioPacket {
                samples: vec![0i16; SILENCE_FRAMES * 2],
                sample_rate: 44_100,
                channels: 2,
                timestamp_qpc: 0,
                stream_position: 0,
                flags: 0,
            });
        }

        // SAFETY: scratch_in[0..SIZEOF_PACKET_HEADER] is initialised by the
        // driver write; we read it as a POD struct.
        let header: StreamToSpeakerAudioPacketHeader = unsafe {
            let ptr = self.scratch_in.as_ptr() as *const StreamToSpeakerAudioPacketHeader;
            ptr.read_unaligned()
        };

        let data_bytes = header.data_bytes as usize;
        if (returned as usize) < SIZEOF_PACKET_HEADER + data_bytes {
            bail!(
                "truncated audio packet: header says {} bytes, IOCTL returned {} bytes",
                data_bytes,
                returned
            );
        }
        if data_bytes > STREAM_TO_SPEAKER_MAX_PACKET_BYTES {
            bail!(
                "driver reported {} data bytes > max {}",
                data_bytes, STREAM_TO_SPEAKER_MAX_PACKET_BYTES
            );
        }
        if header.bits_per_sample != 16 || header.channels != 2 {
            bail!(
                "unsupported wire format from driver: bps={} ch={}",
                header.bits_per_sample,
                header.channels
            );
        }

        // Reinterpret the PCM region as i16s. We have to copy because we
        // hand a Vec<i16> downstream; the buffer is reused for the next
        // IOCTL so we can't ship a borrow.
        let n_samples = data_bytes / 2;
        self.scratch_out.clear();
        self.scratch_out.reserve(n_samples);
        // SAFETY: scratch_in is at least SIZEOF_PACKET_HEADER + data_bytes
        // bytes, and i16 alignment <= u8 alignment.
        unsafe {
            let pcm_ptr = self.scratch_in.as_ptr().add(SIZEOF_PACKET_HEADER) as *const i16;
            for i in 0..n_samples {
                self.scratch_out.push(pcm_ptr.add(i).read_unaligned());
            }
        }

        let mut flags = 0u32;
        if header.flags & 0x0000_0001 != 0 {
            flags |= PACKET_FLAG_STREAM_RESTART;
        }
        if header.flags & 0x0000_0002 != 0 {
            flags |= PACKET_FLAG_HINT_SILENT;
        }

        Ok(AudioPacket {
            samples: self.scratch_out.clone(),
            sample_rate: header.sample_rate,
            channels: header.channels,
            timestamp_qpc: header.timestamp_qpc,
            stream_position: header.stream_position,
            flags,
        })
    }
}

// -----------------------------------------------------------------------------
// Device opening
// -----------------------------------------------------------------------------

fn open_device_handle() -> Result<SharedHandle> {
    // Try the device interface enumeration path first.
    match open_via_interface_guid() {
        Ok(h) => Ok(h),
        Err(e) => {
            warn!("device interface enumeration failed: {} — falling back to symbolic link", e);
            open_via_symbolic_link()
        }
    }
}

fn open_via_symbolic_link() -> Result<SharedHandle> {
    let wide: Vec<u16> = STREAM_TO_SPEAKER_DEVICE_PATH
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: wide is null-terminated; we own no security descriptor.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        let e = unsafe { GetLastError() };
        if e == ERROR_FILE_NOT_FOUND {
            bail!("StreamToSpeaker driver not installed (ERROR_FILE_NOT_FOUND opening \\\\.\\StreamToSpeaker)");
        }
        bail!("CreateFileW(\\\\.\\StreamToSpeaker) failed: WinError {}", e);
    }
    Ok(SharedHandle(Arc::new(HandleInner(h))))
}

fn open_via_interface_guid() -> Result<SharedHandle> {
    // SAFETY: SetupDiGetClassDevsW takes a borrowed GUID pointer.
    let dev_info = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_STREAM_TO_SPEAKER as *const _,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
        )
    };
    if dev_info == -1 {  // INVALID_HANDLE_VALUE for HDEVINFO (isize)
        let e = unsafe { GetLastError() };
        bail!("SetupDiGetClassDevsW failed: WinError {}", e);
    }

    let mut iface_data: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
    iface_data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

    // First interface.
    let ok = unsafe {
        SetupDiEnumDeviceInterfaces(
            dev_info,
            std::ptr::null_mut(),
            &GUID_DEVINTERFACE_STREAM_TO_SPEAKER as *const _,
            0,
            &mut iface_data,
        )
    };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        unsafe { SetupDiDestroyDeviceInfoList(dev_info) };
        if e == ERROR_NO_MORE_ITEMS {
            bail!("no StreamToSpeaker device interfaces present");
        }
        bail!("SetupDiEnumDeviceInterfaces failed: WinError {}", e);
    }

    // Discover required buffer size.
    let mut required: u32 = 0;
    let _ = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &mut iface_data,
            null_mut(),
            0,
            &mut required,
            null_mut(),
        )
    };
    let last = unsafe { GetLastError() };
    if last != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        unsafe { SetupDiDestroyDeviceInfoList(dev_info) };
        bail!("SetupDiGetDeviceInterfaceDetailW(size) failed: WinError {}", last);
    }

    // The structure has a flexible array (DevicePath[1]); we allocate
    // `required` bytes of u8, then write cbSize at offset 0.
    let mut buf: Vec<u8> = vec![0u8; required as usize];
    let detail_ptr = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    // SAFETY: buf is large enough to back the struct header.
    unsafe {
        // On x64, cbSize per the WDK headers is 8.
        (*detail_ptr).cbSize = 8;
    }

    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &mut iface_data,
            detail_ptr,
            required,
            null_mut(),
            null_mut(),
        )
    };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        unsafe { SetupDiDestroyDeviceInfoList(dev_info) };
        bail!("SetupDiGetDeviceInterfaceDetailW failed: WinError {}", e);
    }

    // DevicePath starts immediately after cbSize (4 bytes), but on x64 the
    // struct is 8-byte aligned; per WDK headers DevicePath offset is 4.
    // We read a null-terminated u16 sequence starting at offset 4.
    let path_offset: usize = 4;
    let bytes = &buf[path_offset..];
    let wide_slice: &[u16] = unsafe {
        std::slice::from_raw_parts(bytes.as_ptr() as *const u16, bytes.len() / 2)
    };
    let null_idx = wide_slice
        .iter()
        .position(|&c| c == 0)
        .ok_or_else(|| anyhow!("device interface path not null-terminated"))?;
    let os_path = OsString::from_wide(&wide_slice[..null_idx]);
    debug!("found device interface: {:?}", os_path);

    let mut wide: Vec<u16> = wide_slice[..null_idx].to_vec();
    wide.push(0);

    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };

    // CRITICAL: capture GetLastError BEFORE any other Win32 call. The
    // SetupDi* destroy below resets LastError; reading it afterwards
    // gives us "WinError 0" even when CreateFileW failed.
    let create_err = if h == INVALID_HANDLE_VALUE || h.is_null() {
        Some(unsafe { GetLastError() })
    } else {
        None
    };

    unsafe { SetupDiDestroyDeviceInfoList(dev_info) };

    if let Some(e) = create_err {
        bail!("CreateFileW(device interface) failed: WinError {}", e);
    }

    Ok(SharedHandle(Arc::new(HandleInner(h))))
}

fn ioctl_get_version(handle: HANDLE) -> Result<StreamToSpeakerVersionInfo> {
    let mut info = StreamToSpeakerVersionInfo {
        protocol_version: 0,
        driver_build: 0,
    };
    let mut returned: u32 = 0;
    // SAFETY: METHOD_BUFFERED IOCTL with a stack output.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STREAM_TO_SPEAKER_GET_VERSION,
            null_mut(),
            0,
            (&mut info as *mut _) as *mut _,
            size_of::<StreamToSpeakerVersionInfo>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        bail!("IOCTL_STREAM_TO_SPEAKER_GET_VERSION failed: WinError {}", e);
    }
    if (returned as usize) < size_of::<StreamToSpeakerVersionInfo>() {
        bail!("IOCTL_STREAM_TO_SPEAKER_GET_VERSION returned {} bytes", returned);
    }
    Ok(info)
}

// -----------------------------------------------------------------------------
// Control-event reader thread
// -----------------------------------------------------------------------------

fn spawn_control_event_thread(
    handle: SharedHandle,
    tx: Sender<DriverEvent>,
    shutdown: Arc<AtomicBool>,
) {
    thread::Builder::new()
        .name("stream-to-speaker-control".to_string())
        .spawn(move || control_event_loop(handle, tx, shutdown))
        .expect("spawning control-event thread");
}

fn control_event_loop(handle: SharedHandle, tx: Sender<DriverEvent>, shutdown: Arc<AtomicBool>) {
    let mut buf: StreamToSpeakerControlEvent = unsafe { zeroed() };

    while !shutdown.load(Ordering::SeqCst) {
        let mut returned: u32 = 0;
        // SAFETY: METHOD_BUFFERED with stack output.
        let ok = unsafe {
            DeviceIoControl(
                handle.0 .0,
                IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT,
                null_mut(),
                0,
                (&mut buf as *mut _) as *mut _,
                size_of::<StreamToSpeakerControlEvent>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            error!("IOCTL_STREAM_TO_SPEAKER_GET_CONTROL_EVENT failed: WinError {} — control thread exiting", e);
            return;
        }
        if (returned as usize) < size_of::<StreamToSpeakerControlEvent>() {
            warn!("short control event: {} bytes", returned);
            continue;
        }

        let decoded = decode_control_event(&buf);
        if let Some(ev) = decoded {
            debug!("driver event: {:?}", ev);
            if tx.send(ev).is_err() {
                // Receiver dropped — main loop is shutting down.
                return;
            }
        } else {
            warn!("unknown control event type: {}", buf.event_type);
        }
    }
}

fn decode_control_event(ev: &StreamToSpeakerControlEvent) -> Option<DriverEvent> {
    match ev.event_type {
        STREAM_TO_SPEAKER_EVENT_VOLUME_CHANGED => {
            // First i32 of payload.
            let level = i32::from_ne_bytes(ev.payload[..4].try_into().ok()?);
            Some(DriverEvent::VolumeChanged { level_millibels: level })
        }
        STREAM_TO_SPEAKER_EVENT_MUTE_CHANGED => {
            let muted = u32::from_ne_bytes(ev.payload[..4].try_into().ok()?);
            Some(DriverEvent::MuteChanged { muted: muted != 0 })
        }
        STREAM_TO_SPEAKER_EVENT_STREAM_START => Some(DriverEvent::StreamStart),
        STREAM_TO_SPEAKER_EVENT_STREAM_STOP => Some(DriverEvent::StreamStop),
        STREAM_TO_SPEAKER_EVENT_FORMAT_CHANGE => {
            let sample_rate = u32::from_ne_bytes(ev.payload[..4].try_into().ok()?);
            let bits_per_sample = u16::from_ne_bytes(ev.payload[4..6].try_into().ok()?);
            let channels = u16::from_ne_bytes(ev.payload[6..8].try_into().ok()?);
            Some(DriverEvent::FormatChange {
                sample_rate,
                bits_per_sample,
                channels,
            })
        }
        _ => None,
    }
}
