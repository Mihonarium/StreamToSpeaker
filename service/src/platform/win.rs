//! Windows' side of the seam: Core Audio for Windows' outputs, their
//! names, levels and meters and which apps play on them; `IPolicyConfig`
//! to switch the default output (as Sound settings does); a hidden window
//! that hears sleep, wake, sign-out and shutdown; and the hand-back that
//! runs when the app ends without quitting.
//!
//! One worker thread owns every Core Audio object. It looks at Windows
//! every quarter second, keeps what it saw for the engine to read without
//! waiting, and tells the engine when the default output or the set of
//! outputs changed. Switching and setting a level go to the worker and wait for it
//! (at most two seconds), so a hung audio service never holds the engine
//! for long.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use log::{debug, info, warn};
use windows::core::{Interface, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_DeviceInterface_FriendlyName, PKEY_Device_DeviceDesc, PKEY_Device_FriendlyName,
};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE, PKEY_AudioEndpoint_FormFactor,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};

use super::{
    AppAudio, AudioSessions, EventSink, Handback, OurDevice, OutputInfo, OutputKind, Outputs, PcOutputId, Platform,
    Role, SystemEvent, SystemEvents,
};

/// How often the worker looks at Windows.
const LOOK_EVERY: Duration = Duration::from_millis(250);
/// The longest a switch or a level change waits for the worker.
const WAIT_FOR_WORKER: Duration = Duration::from_secs(2);
/// The words every name of the app's own output carries.
const OUR_NAME: &str = "Stream To Speaker";

/// The real PC's platform; passive when Core Audio can't be reached.
pub(crate) fn platform() -> Platform {
    match Shared::start() {
        Some(shared) => {
            crate::endpoint_name::platform_names_the_device();
            install_crash_handback();
            start_session_window(shared.clone());
            let w = Arc::new(Windows(shared));
            Platform { outputs: w.clone(), sessions: w.clone(), device: w.clone(), handback: w.clone(), events: w }
        }
        None => {
            warn!("Windows' sound outputs can't be read here; the person chooses the output in Windows");
            Platform::passive()
        }
    }
}

/// What the worker last saw.
#[derive(Clone, Default)]
struct Seen {
    default: Option<PcOutputId>,
    ours: Option<PcOutputId>,
    outputs: Vec<OutputInfo>,
    levels: HashMap<PcOutputId, (u8, bool)>,
    peaks: HashMap<PcOutputId, f32>,
    sessions: Vec<AppAudio>,
}

enum Job {
    SetDefault(PcOutputId, Vec<Role>, Sender<Result<(), String>>),
    SetVolume(PcOutputId, u8, bool, Sender<Result<(), String>>),
}

struct Shared {
    seen: Mutex<Seen>,
    jobs: Sender<Job>,
    sinks: Mutex<Vec<EventSink>>,
}

impl Shared {
    /// Start the worker; `None` when Core Audio can't be reached.
    fn start() -> Option<Arc<Shared>> {
        let (jobs, rx) = unbounded();
        let shared = Arc::new(Shared { seen: Mutex::new(Seen::default()), jobs, sinks: Mutex::new(Vec::new()) });
        let (ready_tx, ready) = bounded(1);
        let for_worker = shared.clone();
        let spawned = std::thread::Builder::new().name("stream-to-speaker-windows-audio".into()).spawn(move || {
            // SAFETY: COM is initialised once for this thread, which owns
            // every Core Audio object it creates and runs as long as the app.
            if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
                let _ = ready_tx.send(false);
                return;
            }
            // SAFETY: plain object creation on a COM thread.
            let enumerator: IMMDeviceEnumerator = match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
                Ok(e) => e,
                Err(e) => {
                    warn!("no audio device enumerator: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            let mut worker = Worker { enumerator, endpoints: HashMap::new(), apps: HashMap::new() };
            worker.look(&for_worker);
            let _ = ready_tx.send(true);
            worker.run(&rx, &for_worker);
        });
        if spawned.is_err() {
            return None;
        }
        ready.recv_timeout(WAIT_FOR_WORKER).ok().filter(|ok| *ok).map(|_| shared)
    }

    fn ask(&self, job: impl FnOnce(Sender<Result<(), String>>) -> Job) -> Result<(), String> {
        let (tx, rx) = bounded(1);
        self.jobs.send(job(tx)).map_err(|_| "the Windows audio worker has stopped".to_string())?;
        rx.recv_timeout(WAIT_FOR_WORKER).unwrap_or_else(|_| Err("Windows didn't answer in time".into()))
    }

    fn tell(&self, event: SystemEvent) {
        let sinks: Vec<EventSink> = self.sinks.lock().unwrap().clone();
        for s in sinks {
            s(event.clone());
        }
    }
}

/// The Core Audio objects of one active output, kept while it stays.
struct Endpoint {
    info: OutputInfo,
    volume: Option<IAudioEndpointVolume>,
    meter: Option<IAudioMeterInformation>,
    sessions: Option<IAudioSessionManager2>,
}

struct Worker {
    enumerator: IMMDeviceEnumerator,
    endpoints: HashMap<PcOutputId, Endpoint>,
    /// Process id → the app's name.
    apps: HashMap<u32, String>,
}

impl Worker {
    fn run(&mut self, rx: &Receiver<Job>, shared: &Shared) {
        loop {
            match rx.recv_timeout(LOOK_EVERY) {
                Ok(Job::SetDefault(id, roles, reply)) => {
                    let r = set_default(&id, &roles);
                    self.look(shared);
                    let _ = reply.send(r);
                }
                Ok(Job::SetVolume(id, level, muted, reply)) => {
                    let r = self.set_volume(&id, level, muted);
                    self.look(shared);
                    let _ = reply.send(r);
                }
                Err(RecvTimeoutError::Timeout) => self.look(shared),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Look at Windows and keep what was seen.
    fn look(&mut self, shared: &Shared) {
        let mut seen = Seen::default();
        // SAFETY: calls on Core Audio objects this thread created.
        unsafe {
            seen.default = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()
                .and_then(|d| device_id(&d));
            let active = self.active_ids();
            self.endpoints.retain(|id, _| active.iter().any(|(a, _)| a == id));
            for (id, device) in active {
                if !self.endpoints.contains_key(&id) {
                    if let Some(e) = open_endpoint(&id, &device) {
                        self.endpoints.insert(id.clone(), e);
                    }
                }
            }
            for (id, e) in &self.endpoints {
                seen.outputs.push(e.info.clone());
                if is_ours(&e.info) {
                    seen.ours = Some(id.clone());
                }
                if let Some(v) = &e.volume {
                    let muted = v.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                    let level = if is_ours(&e.info) {
                        v.GetMasterVolumeLevel()
                            .ok()
                            .map(|db| crate::volume_sync::millibels_to_sonos((db * 100.0).round() as i32) as u8)
                    } else {
                        v.GetMasterVolumeLevelScalar().ok().map(|s| (s * 100.0).round().clamp(0.0, 100.0) as u8)
                    };
                    if let Some(level) = level {
                        seen.levels.insert(id.clone(), (level, muted));
                    }
                }
                if let Some(m) = &e.meter {
                    seen.peaks.insert(id.clone(), m.GetPeakValue().unwrap_or(0.0));
                }
            }
            let managers: Vec<(PcOutputId, IAudioSessionManager2)> =
                self.endpoints.iter().filter_map(|(id, e)| e.sessions.clone().map(|m| (id.clone(), m))).collect();
            let mut sessions = Vec::new();
            for (id, manager) in &managers {
                self.sessions_on(id, manager, &mut sessions);
            }
            seen.sessions = sessions;
        }
        seen.outputs.sort_by(|a, b| a.friendly.to_lowercase().cmp(&b.friendly.to_lowercase()));
        let (default_changed, outputs_changed) = {
            let mut was = shared.seen.lock().unwrap();
            let ids = |s: &Seen| s.outputs.iter().map(|o| o.id.clone()).collect::<Vec<_>>();
            let changes = (was.default != seen.default, ids(&was) != ids(&seen));
            let first = was.outputs.is_empty() && was.default.is_none();
            *was = seen;
            if first {
                (false, false)
            } else {
                changes
            }
        };
        if outputs_changed {
            shared.tell(SystemEvent::OutputsChanged);
        }
        if default_changed {
            shared.tell(SystemEvent::DefaultOutputChanged);
        }
    }

    /// Every active render endpoint, by id.
    ///
    /// # Safety
    /// On this worker's thread only.
    unsafe fn active_ids(&self) -> Vec<(PcOutputId, IMMDevice)> {
        let Ok(all) = self.enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let count = all.GetCount().unwrap_or(0);
        (0..count)
            .filter_map(|i| all.Item(i).ok())
            .filter_map(|d| device_id(&d).map(|id| (id, d)))
            .collect()
    }

    /// The apps that have an active session on `id`, with their peaks.
    ///
    /// # Safety
    /// On this worker's thread only.
    unsafe fn sessions_on(&mut self, id: &PcOutputId, manager: &IAudioSessionManager2, out: &mut Vec<AppAudio>) {
        let Ok(list) = manager.GetSessionEnumerator() else { return };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(session) = list.GetSession(i) else { continue };
            if session.GetState().map(|s| s != AudioSessionStateActive).unwrap_or(true) {
                continue;
            }
            let Ok(control) = session.cast::<IAudioSessionControl2>() else { continue };
            let system_sounds = control.IsSystemSoundsSession().0 == 0;
            let pid = control.GetProcessId().unwrap_or(0);
            let peak = session.cast::<IAudioMeterInformation>().ok().and_then(|m| m.GetPeakValue().ok()).unwrap_or(0.0);
            let app = if system_sounds {
                "System".to_string()
            } else {
                self.apps.entry(pid).or_insert_with(|| app_name(pid)).clone()
            };
            out.push(AppAudio { app, output: id.clone(), peak, system_sounds });
        }
    }

    fn set_volume(&self, id: &PcOutputId, level: u8, muted: bool) -> Result<(), String> {
        let e = self.endpoints.get(id).ok_or("no such output")?;
        let v = e.volume.as_ref().ok_or("its level can't be set")?;
        let level = level.min(100);
        // SAFETY: calls on an endpoint volume this thread activated.
        unsafe {
            if is_ours(&e.info) {
                // The app's own output: in dB, on the linear mapping its
                // level node uses (Windows draws that node's slider
                // linearly, so the scalar call's taper would misplace it).
                let db = crate::volume_sync::sonos_to_millibels(level as u32) as f32 / 100.0;
                v.SetMasterVolumeLevel(db, std::ptr::null()).map_err(|e| e.to_string())?;
            } else {
                v.SetMasterVolumeLevelScalar(level as f32 / 100.0, std::ptr::null()).map_err(|e| e.to_string())?;
            }
            v.SetMute(windows::Win32::Foundation::BOOL::from(muted), std::ptr::null()).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// An output's id, as Windows gives it.
///
/// # Safety
/// `device` must be a live IMMDevice.
unsafe fn device_id(device: &IMMDevice) -> Option<PcOutputId> {
    let id: PWSTR = device.GetId().ok()?;
    let text = id.to_string().ok();
    CoTaskMemFree(Some(id.0 as *const _));
    text.map(|t| PcOutputId::new(&t))
}

/// A string property, if set.
///
/// # Safety
/// `store` must be a live property store.
unsafe fn text_of(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    let value = store.GetValue(key).ok()?;
    let text = value.to_string();
    (!text.is_empty()).then_some(text)
}

/// Open an active output's names and the objects the worker reads.
///
/// # Safety
/// `device` must be a live IMMDevice on this thread.
unsafe fn open_endpoint(id: &PcOutputId, device: &IMMDevice) -> Option<Endpoint> {
    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let friendly = text_of(&store, &PKEY_Device_FriendlyName).unwrap_or_default();
    let name = text_of(&store, &PKEY_Device_DeviceDesc).unwrap_or_else(|| friendly.clone());
    let device_name = text_of(&store, &PKEY_DeviceInterface_FriendlyName).unwrap_or_default();
    let form = store.GetValue(&PKEY_AudioEndpoint_FormFactor).ok().and_then(|v| u32::try_from(&v).ok());
    let kind = match form {
        // Speakers.
        Some(1) => OutputKind::Speakers,
        // Headphones, a headset.
        Some(3) | Some(5) => OutputKind::Headphones,
        // A display over HDMI or DisplayPort.
        Some(9) => OutputKind::Display,
        _ => OutputKind::Other,
    };
    let info = OutputInfo { id: id.clone(), friendly, name, device: device_name, kind };
    Some(Endpoint {
        volume: device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok(),
        meter: device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None).ok(),
        sessions: device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok(),
        info,
    })
}

/// The app's own output: its device is "Stream To Speaker", whatever the
/// output itself is called now.
fn is_ours(info: &OutputInfo) -> bool {
    info.device.contains(OUR_NAME) || info.friendly.contains(OUR_NAME)
}

fn role_number(role: Role) -> i32 {
    match role {
        Role::Console => 0,
        Role::Multimedia => 1,
        Role::Communications => 2,
    }
}

/// Make `id` Windows' default for `roles`.
fn set_default(id: &PcOutputId, roles: &[Role]) -> Result<(), String> {
    for role in roles {
        crate::endpoint_name::set_default_endpoint(&id.0, role_number(*role)).map_err(|e| format!("{e:#}"))?;
    }
    info!("Windows' output is now {} for {:?}", id.0, roles);
    Ok(())
}

/// An app's name for the Volume mixer: its file description ("Spotify",
/// "Google Chrome"), else its file name without ".exe".
fn app_name(pid: u32) -> String {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: the handle is checked and closed; the buffer and its length
    // are this function's own and stay alive for the call.
    let ok = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return format!("Process {pid}");
        }
        let ok = QueryFullProcessImageNameW(h, 0, path.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        ok
    };
    if !ok {
        return format!("Process {pid}");
    }
    let path = String::from_utf16_lossy(&path[..len as usize]);
    file_description(&path).unwrap_or_else(|| {
        let file = path.rsplit('\\').next().unwrap_or(&path);
        file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    })
}

/// The FileDescription in an executable's version resource.
fn file_description(path: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; `data` is sized as Windows asked;
    // VerQueryValueW points into `data`, which outlives every read, and the
    // lengths it returns bound them.
    unsafe {
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let query = |sub: &str| -> Option<(*const u8, u32)> {
            let sub: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
            let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            (VerQueryValueW(data.as_ptr().cast(), sub.as_ptr(), &mut ptr, &mut len) != 0 && !ptr.is_null())
                .then_some((ptr as *const u8, len))
        };
        let (t, tlen) = query("\\VarFileInfo\\Translation")?;
        if tlen < 4 {
            return None;
        }
        let lang = u16::from_le_bytes([*t, *t.add(1)]);
        let codepage = u16::from_le_bytes([*t.add(2), *t.add(3)]);
        let (p, n) = query(&format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription"))?;
        let chars = std::slice::from_raw_parts(p as *const u16, n as usize);
        let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        let text = String::from_utf16_lossy(&chars[..end]).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// The platform's face to the engine.
struct Windows(Arc<Shared>);

impl Outputs for Windows {
    fn default_output(&self) -> Option<PcOutputId> {
        self.0.seen.lock().unwrap().default.clone()
    }
    fn our_output(&self) -> Option<PcOutputId> {
        self.0.seen.lock().unwrap().ours.clone()
    }
    fn describe(&self, id: &PcOutputId) -> Option<OutputInfo> {
        self.0.seen.lock().unwrap().outputs.iter().find(|o| &o.id == id).cloned()
    }
    fn list(&self) -> Vec<OutputInfo> {
        self.0.seen.lock().unwrap().outputs.clone()
    }
    fn set_default(&self, id: &PcOutputId, roles: &[Role]) -> Result<(), String> {
        let roles = roles.to_vec();
        self.0.ask(|reply| Job::SetDefault(id.clone(), roles, reply))
    }
    fn volume(&self, id: &PcOutputId) -> Option<(u8, bool)> {
        self.0.seen.lock().unwrap().levels.get(id).copied()
    }
    fn set_volume(&self, id: &PcOutputId, level: u8, muted: bool) -> Result<(), String> {
        self.0.ask(|reply| Job::SetVolume(id.clone(), level, muted, reply))
    }
}

impl AudioSessions for Windows {
    fn sessions(&self) -> Vec<AppAudio> {
        self.0.seen.lock().unwrap().sessions.clone()
    }
    fn peak(&self, output: &PcOutputId) -> f32 {
        self.0.seen.lock().unwrap().peaks.get(output).copied().unwrap_or(0.0)
    }
}

impl OurDevice for Windows {
    fn rename(&self, name: &str) -> Result<(), String> {
        let ticket = crate::endpoint_name::rename_endpoint(name);
        *LAST_RENAME.lock().unwrap() = ticket;
        Ok(())
    }
    fn settle(&self, max: Duration) {
        let ticket = *LAST_RENAME.lock().unwrap();
        crate::endpoint_name::wait_for_rename(ticket, max);
    }
}

static LAST_RENAME: Mutex<u64> = Mutex::new(0);

/// Where the sound goes back to, and the device's name, if the app ends
/// without quitting.
static HANDBACK: Mutex<Option<(Option<String>, String)>> = Mutex::new(None);

impl Handback for Windows {
    fn prepare(&self, output: Option<&PcOutputId>, name: &str) {
        *HANDBACK.lock().unwrap() = Some((output.map(|o| o.0.to_string()), name.to_string()));
    }
}

/// Give Windows' output back and name the device, now, on this thread:
/// when the app crashes and when Windows ends the session. Waits for no
/// lock another thread may hold.
fn hand_back_now() {
    let Ok(guard) = HANDBACK.try_lock() else { return };
    let Some((output, name)) = guard.clone() else { return };
    drop(guard);
    if let Some(output) = output {
        let ours = crate::endpoint_name::find_our_endpoint_id().ok().flatten();
        // SAFETY: COM for this thread, balanced when it was this call that
        // set it up; the enumerator and device are released on return.
        let default = unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let d = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .ok()
                .and_then(|e| e.GetDefaultAudioEndpoint(eRender, eConsole).ok())
                .and_then(|d| device_id(&d));
            if hr.is_ok() {
                windows::Win32::System::Com::CoUninitialize();
            }
            d
        };
        if ours.is_some() && default.as_ref().map(|d| d.0.to_string()) == ours {
            let _ = set_default(&PcOutputId::new(&output), super::ALL_ROLES);
        }
    }
    let _ = crate::endpoint_name::rename_endpoint_now(&name);
}

/// Hand the sound back before the app's own panic hook runs (which logs
/// and may show a dialog).
fn install_crash_handback() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let earlier = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            hand_back_now();
            earlier(info);
        }));
    });
}

impl SystemEvents for Windows {
    fn subscribe(&self, sink: EventSink) {
        self.0.sinks.lock().unwrap().push(sink);
    }
}

/// The platform the hidden window tells.
static SESSION: OnceLock<Arc<Shared>> = OnceLock::new();

/// A hidden top-level window on its own thread: Windows sends it the
/// power and session messages every top-level window gets.
fn start_session_window(shared: Arc<Shared>) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG, WNDCLASSW,
        WS_OVERLAPPED,
    };
    if SESSION.set(shared).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new().name("stream-to-speaker-session".into()).spawn(|| {
        let class: Vec<u16> = "StreamToSpeakerSession\0".encode_utf16().collect();
        // SAFETY: the class name outlives the class and the window (it lives
        // for this thread, which runs the message loop until the app ends);
        // the window procedure is a plain `extern "system"` function; MSG is
        // written by GetMessageW before it is read.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(session_window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            if RegisterClassW(&wc) == 0 {
                warn!("no window for sleep and sign-out notices");
                return;
            }
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                warn!("no window for sleep and sign-out notices");
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
    if let Err(e) = spawned {
        warn!("no thread for sleep and sign-out notices: {e}");
    }
}

unsafe extern "system" fn session_window_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION,
    };
    let tell = |e: SystemEvent| {
        if let Some(s) = SESSION.get() {
            s.tell(e);
        }
    };
    match msg {
        WM_POWERBROADCAST if wparam == PBT_APMSUSPEND as usize => {
            debug!("Windows is going to sleep");
            tell(SystemEvent::Suspending);
            1
        }
        WM_POWERBROADCAST if wparam == PBT_APMRESUMEAUTOMATIC as usize => {
            debug!("Windows woke");
            tell(SystemEvent::Resumed);
            1
        }
        // Signing out or shutting down is fine by this app.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            // The process may end as soon as this returns: hand the sound
            // back here, then let the engine do the rest if it still can.
            info!("Windows is ending the session: giving the sound back");
            hand_back_now();
            tell(SystemEvent::SessionEnding);
            0
        }
        // SAFETY: the arguments are the ones Windows passed in.
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
