//! AAC-LC encoding via the Windows-provided Media Foundation encoder.
//!
//! AirPlay 2 buffered audio (type 103) from real iOS senders is AAC-LC
//! 44100/2 — and field testing suggests current Sonos firmware only truly
//! *plays* that combination (it accepts SETUPs for ALAC/realtime/NTP the
//! way it accepts RAOP: vestigially, without sound). Windows ships an
//! AAC-LC encoder MFT (`CLSID_AACMFTEncoder`, Windows 7+), so no
//! third-party codec or license is needed.
//!
//! Usage contract: feed [`AacEncoder::encode`] exactly [`AAC_SPF`] frames
//! (1024) of interleaved stereo i16 per call; it returns zero or more raw
//! AAC-LC access units (the MFT buffers one or two frames before its
//! first output). Each returned frame is one RTP packet's payload and
//! advances rtptime by [`AAC_SPF`].
//!
//! COM threading: construct and use an encoder on the same thread
//! (the session probes availability with a throwaway instance, and the
//! sender thread builds its own).
//!
//! Media Foundation is found at run time ([`MfPlat`]), never linked: a
//! linked function puts `mfplat.dll` in the exe's import table, and Windows
//! refuses to start a program whose imports are missing. N editions have no
//! Media Foundation unless the Media Feature Pack is installed, so a linked
//! import would stop the whole app there, not just AAC.

use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFMediaType_Audio, MFSTARTUP_LITE, MFT_OUTPUT_DATA_BUFFER, MF_API_VERSION,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SDK_VERSION,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_MOD_NOT_FOUND};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use crate::WIRE_SAMPLE_RATE;

/// The four Media Foundation functions the encoder calls, from the system's
/// own `mfplat.dll`. Found once per process, the first time an encoder is
/// made; a missing library or function is an ordinary error, which the
/// session's probe turns into its ALAC fallback.
struct MfPlat {
    startup: unsafe extern "system" fn(version: u32, flags: u32) -> HRESULT,
    create_media_type: unsafe extern "system" fn(out: *mut *mut c_void) -> HRESULT,
    create_sample: unsafe extern "system" fn(out: *mut *mut c_void) -> HRESULT,
    create_memory_buffer: unsafe extern "system" fn(max_len: u32, out: *mut *mut c_void) -> HRESULT,
}

impl MfPlat {
    fn get() -> Result<&'static MfPlat> {
        static MFPLAT: OnceLock<std::result::Result<MfPlat, String>> = OnceLock::new();
        MFPLAT.get_or_init(MfPlat::load).as_ref().map_err(|e| anyhow!("{e}"))
    }

    fn load() -> std::result::Result<MfPlat, String> {
        let name: Vec<u16> = "mfplat.dll\0".encode_utf16().collect();
        // System32 only: never a copy in the app's folder or the current
        // directory. The library stays loaded for the life of the process.
        let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            let code = unsafe { GetLastError() };
            return Err(if code == ERROR_MOD_NOT_FOUND {
                "Media Foundation is not installed on this PC: there is no mfplat.dll in System32. \
                 Windows N editions get it with the Media Feature Pack"
                    .to_string()
            } else {
                format!("Media Foundation (mfplat.dll) could not be loaded: {}", os_error(code))
            });
        }
        let find = |symbol: &[u8]| {
            unsafe { GetProcAddress(module, symbol.as_ptr()) }.ok_or_else(|| {
                let code = unsafe { GetLastError() };
                let name = String::from_utf8_lossy(symbol.strip_suffix(b"\0").unwrap_or(symbol)).into_owned();
                format!("mfplat.dll has no {name}: {}", os_error(code))
            })
        };
        // SAFETY: each pointer is the named export of mfplat.dll, whose
        // signature is the one declared in `MfPlat` (mfapi.h).
        unsafe {
            Ok(MfPlat {
                startup: std::mem::transmute(find(b"MFStartup\0")?),
                create_media_type: std::mem::transmute(find(b"MFCreateMediaType\0")?),
                create_sample: std::mem::transmute(find(b"MFCreateSample\0")?),
                create_memory_buffer: std::mem::transmute(find(b"MFCreateMemoryBuffer\0")?),
            })
        }
    }

    unsafe fn startup(&self, version: u32, flags: u32) -> windows::core::Result<()> {
        (self.startup)(version, flags).ok()
    }

    unsafe fn create_media_type(&self) -> windows::core::Result<IMFMediaType> {
        let mut out = std::ptr::null_mut();
        (self.create_media_type)(&mut out).ok()?;
        interface_from(out)
    }

    unsafe fn create_sample(&self) -> windows::core::Result<IMFSample> {
        let mut out = std::ptr::null_mut();
        (self.create_sample)(&mut out).ok()?;
        interface_from(out)
    }

    unsafe fn create_memory_buffer(&self, max_len: u32) -> windows::core::Result<IMFMediaBuffer> {
        let mut out = std::ptr::null_mut();
        (self.create_memory_buffer)(max_len, &mut out).ok()?;
        interface_from(out)
    }
}

/// Take ownership of the reference a creation function returned, as the
/// `windows` crate's own wrappers do (a null pointer with success is an
/// error).
unsafe fn interface_from<T: Interface>(raw: *mut c_void) -> windows::core::Result<T> {
    if raw.is_null() {
        Err(windows::core::Error::empty())
    } else {
        Ok(T::from_raw(raw))
    }
}

fn os_error(code: u32) -> std::io::Error {
    std::io::Error::from_raw_os_error(code as i32)
}

/// Samples per AAC-LC frame — fixed by the codec.
pub const AAC_SPF: usize = 1024;

/// AAC-LC bitrate in bytes/second. The Windows MFT accepts 12000, 16000,
/// 20000 or 24000 for stereo 44.1 kHz; 24000 = 192 kbps, its best.
const AAC_BYTES_PER_SECOND: u32 = 24000;

/// CLSID of the Windows AAC encoder MFT (documented constant; the
/// `windows` crate doesn't export it by name).
const CLSID_AAC_MFT_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);

const MF_VERSION: u32 = ((MF_SDK_VERSION as u32) << 16) | MF_API_VERSION as u32;

pub struct AacEncoder {
    mf: &'static MfPlat,
    mft: IMFTransform,
    /// Presentation clock for input samples, in 100 ns units.
    pts_100ns: i64,
    /// Scratch for the MFT's preferred output buffer size.
    out_buf_size: u32,
}

impl AacEncoder {
    /// Create and configure an encoder: PCM 44.1 kHz stereo 16-bit in,
    /// raw AAC-LC access units out (payload type 0 — no ADTS framing).
    pub fn new() -> Result<Self> {
        let mf = MfPlat::get()?;
        unsafe {
            // Per-thread COM init; tolerate "already initialized (any mode)".
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            mf.startup(MF_VERSION, MFSTARTUP_LITE).context("MFStartup")?;

            let mft: IMFTransform =
                CoCreateInstance(&CLSID_AAC_MFT_ENCODER, None, CLSCTX_INPROC_SERVER)
                    .context("creating the Windows AAC encoder MFT")?;

            // Encoders want the output type first.
            let out_ty: IMFMediaType = mf.create_media_type().context("MFCreateMediaType(out)")?;
            out_ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out_ty.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out_ty.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out_ty.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, WIRE_SAMPLE_RATE)?;
            out_ty.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            out_ty.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
            out_ty.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw access units
            mft.SetOutputType(0, &out_ty, 0).context("SetOutputType(AAC)")?;

            let in_ty: IMFMediaType = mf.create_media_type().context("MFCreateMediaType(in)")?;
            in_ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            in_ty.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            in_ty.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            in_ty.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, WIRE_SAMPLE_RATE)?;
            in_ty.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            in_ty.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            in_ty.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, WIRE_SAMPLE_RATE * 4)?;
            mft.SetInputType(0, &in_ty, 0).context("SetInputType(PCM)")?;

            let out_info = mft.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            let out_buf_size = out_info.cbSize.max(8192);

            Ok(Self { mf, mft, pts_100ns: 0, out_buf_size })
        }
    }

    /// Encode exactly [`AAC_SPF`] stereo frames (`2 * AAC_SPF` interleaved
    /// i16 samples). Returns every AAC access unit the MFT has ready —
    /// possibly none for the first call or two while it primes.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>> {
        if pcm.len() != AAC_SPF * 2 {
            return Err(anyhow!("AAC encode needs {} samples, got {}", AAC_SPF * 2, pcm.len()));
        }
        unsafe {
            let byte_len = (pcm.len() * 2) as u32;
            let in_buf: IMFMediaBuffer =
                self.mf.create_memory_buffer(byte_len).context("MFCreateMemoryBuffer(in)")?;
            {
                let mut ptr = std::ptr::null_mut();
                in_buf.Lock(&mut ptr, None, None).context("in Lock")?;
                std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, byte_len as usize);
                in_buf.Unlock().context("in Unlock")?;
            }
            in_buf.SetCurrentLength(byte_len)?;
            let sample: IMFSample = self.mf.create_sample().context("MFCreateSample(in)")?;
            sample.AddBuffer(&in_buf)?;
            // 1024 samples at 44.1 kHz in 100 ns units.
            let duration = (AAC_SPF as i64 * 10_000_000) / WIRE_SAMPLE_RATE as i64;
            sample.SetSampleTime(self.pts_100ns)?;
            sample.SetSampleDuration(duration)?;
            self.pts_100ns += duration;

            self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;
            self.drain_output()
        }
    }

    unsafe fn drain_output(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        loop {
            let out_buf: IMFMediaBuffer =
                self.mf.create_memory_buffer(self.out_buf_size).context("MFCreateMemoryBuffer(out)")?;
            let out_sample: IMFSample = self.mf.create_sample().context("MFCreateSample(out)")?;
            out_sample.AddBuffer(&out_buf)?;

            let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(out_sample.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let hr = self.mft.ProcessOutput(0, &mut outputs, &mut status);
            // Release the refs the struct holds regardless of outcome.
            ManuallyDrop::drop(&mut outputs[0].pSample);
            ManuallyDrop::drop(&mut outputs[0].pEvents);

            match hr {
                Ok(()) => {
                    let len = out_buf.GetCurrentLength().context("GetCurrentLength")? as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut ptr = std::ptr::null_mut();
                    out_buf.Lock(&mut ptr, None, None).context("out Lock")?;
                    let frame = std::slice::from_raw_parts(ptr as *const u8, len).to_vec();
                    out_buf.Unlock().context("out Unlock")?;
                    frames.push(frame);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) => return Err(anyhow!("ProcessOutput failed: {e}")),
            }
        }
        Ok(frames)
    }
}
