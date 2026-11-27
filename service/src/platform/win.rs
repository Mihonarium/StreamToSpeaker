//! Windows' side of the seam: Core Audio for Windows' outputs, their
//! names, levels and meters and which apps play on them; `IPolicyConfig`
//! to switch the default output (as Sound settings does); a hidden window
//! that hears sleep, wake, sign-out and shutdown; and the hand-back that
//! runs when the app ends without quitting.
//!
//! One worker thread watches Core Audio outputs and sessions. It looks at Windows
//! every quarter second, keeps what it saw for the engine to read without
//! waiting, and tells the engine when the default output or the set of
//! outputs changed. Switching and setting a level go to the worker and wait for it
//! (at most two seconds), so a hung audio service never holds the engine
//! for long.

#![cfg(windows)]

mod desk;
mod hotkeys;
pub(crate) use hotkeys::windows_key_down;
mod install;
mod network;
mod pnp;
mod pass;
mod players;
mod repair;
pub use repair::repair_output;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use log::{debug, info, warn};
use windows::core::{Interface, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_DeviceInterface_FriendlyName, PKEY_Device_DeviceDesc, PKEY_Device_FriendlyName,
};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, PKEY_AudioEndpoint_FormFactor,
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

/// The workers and subscriptions belong to this Windows process.
pub(crate) fn platform() -> Platform {
    static PLATFORM: OnceLock<Platform> = OnceLock::new();
    PLATFORM
        .get_or_init(|| {
            let mut p = Platform::passive();
            let audio = Shared::start();
            p.device = Arc::new(WindowsDevice(audio.clone()));
            if let Some(shared) = audio {
                crate::endpoint_name::platform_names_the_device();
                let w = Arc::new(Windows(shared));
                install_crash_handback(&*w);
                start_session_window();
                p.outputs = w.clone();
                p.sessions = w.clone();
                p.handback = w;
            } else {
                warn!("Windows' sound outputs can't be read here; the person chooses the output in Windows");
            }
            p.events = notices().clone();
            p.hotkeys = Arc::new(hotkeys::WindowsKeys);
            p.players = players::players().clone();
            p.network = network::start();
            p.pass = Arc::new(pass::WindowsPass::new(p.outputs.clone()));
            p.desk = Arc::new(desk::WindowsDesk::default());
            p
        })
        .clone()
}

#[derive(Default)]
struct Notices(Mutex<Vec<EventSink>>);

fn notices() -> &'static Arc<Notices> {
    static NOTICES: OnceLock<Arc<Notices>> = OnceLock::new();
    NOTICES.get_or_init(Arc::default)
}

impl Notices {
    fn tell(&self, event: SystemEvent) {
        let sinks = self.0.lock().unwrap().clone();
        for sink in sinks {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink(event.clone()))).is_err() {
                debug!("A Windows event subscriber stopped while handling a notice");
            }
        }
    }
}

impl SystemEvents for Notices {
    fn subscribe(&self, sink: EventSink) {
        self.0.lock().unwrap().push(sink);
    }
}

/// Initialises WinRT and COM on a worker. The guard stays on that thread
/// and is dropped after its last interface.
struct Apartment(std::marker::PhantomData<std::rc::Rc<()>>);

impl Apartment {
    fn enter() -> windows::core::Result<Self> {
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
        // SAFETY: a new worker enters its MTA and balances success in Drop.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self(std::marker::PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: this guard owns one successful RoInitialize on this thread.
        unsafe { windows::Win32::System::WinRT::RoUninitialize() };
    }
}

/// Walk a process snapshot until the visitor finds its process.
fn find_process(mut visit: impl FnMut(u32, &str) -> bool) -> bool {
    use windows::core::Owned;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    // SAFETY: the snapshot's sole handle belongs to Owned. Each successful
    // call fills the sized entry, whose inline name is read within its bounds.
    unsafe {
        let Ok(handle) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let snapshot = Owned::new(handle);
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut next = Process32FirstW(*snapshot, &mut entry);
        while next.is_ok() {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            if visit(entry.th32ProcessID, &String::from_utf16_lossy(&entry.szExeFile[..end])) {
                return true;
            }
            next = Process32NextW(*snapshot, &mut entry);
        }
        false
    }
}

/// What the worker last saw.
#[derive(Clone, Default)]
struct Seen {
    default: Option<PcOutputId>,
    multimedia: Option<PcOutputId>,
    calls: Option<PcOutputId>,
    ours: Option<PcOutputId>,
    disabled_ours: Option<PcOutputId>,
    outputs: Vec<OutputInfo>,
    levels: HashMap<PcOutputId, (u8, bool)>,
    peaks: HashMap<PcOutputId, f32>,
    sessions: Vec<AppAudio>,
}

enum Job {
    TurnOn(Sender<Result<(), String>>),
    CaptureName(PcOutputId, Sender<Option<String>>),
    SetDefault(PcOutputId, Vec<Role>, Sender<Result<(), String>>),
    SetVolume(PcOutputId, u8, bool, Sender<Result<(), String>>),
}

struct Shared {
    seen: Mutex<Seen>,
    jobs: OnceLock<Sender<Job>>,
}

impl Shared {
    /// Keep looking for Core Audio if it was unavailable at startup.
    fn start() -> Option<Arc<Shared>> {
        let shared = Arc::new(Shared { seen: Mutex::new(Seen::default()), jobs: OnceLock::new() });
        let (ready_tx, ready) = bounded(1);
        let for_worker = shared.clone();
        std::thread::Builder::new().name("stream-to-speaker-windows-audio".into()).spawn(move || {
            let open = || -> windows::core::Result<_> {
                let apartment = Apartment::enter()?;
                // SAFETY: the worker owns the apartment and will drop its enumerator before leaving it.
                let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
                Ok((apartment, enumerator))
            };
            let mut first = Some(ready_tx);
            let (_apartment, enumerator) = loop {
                match open() {
                    Ok(audio) => break audio,
                    Err(e) => {
                        if let Some(first) = first.take() {
                            warn!("Core Audio is unavailable, retrying: {e}");
                            let _ = first.send(());
                        }
                        std::thread::sleep(WAIT_FOR_WORKER);
                    }
                }
            };
            let mut worker = Worker { enumerator, endpoints: HashMap::new(), apps: HashMap::new() };
            worker.look(&for_worker);
            let (jobs, rx) = unbounded();
            let _ = for_worker.jobs.set(jobs);
            if let Some(first) = first { let _ = first.send(()); }
            worker.run(&rx, &for_worker);
        }).ok()?;
        let _ = ready.recv_timeout(WAIT_FOR_WORKER);
        Some(shared)
    }

    fn ask(&self, job: impl FnOnce(Sender<Result<(), String>>) -> Job) -> Result<(), String> {
        let (tx, rx) = bounded(1);
        self.jobs.get().ok_or("Windows audio is still starting")?.send(job(tx)).map_err(|_| "the Windows audio worker has stopped".to_string())?;
        rx.recv_timeout(WAIT_FOR_WORKER).unwrap_or_else(|_| Err("Windows didn't answer in time".into()))
    }
}

/// The Core Audio objects of one active output, kept while it stays.
struct Endpoint {
    ours: bool,
    info: OutputInfo,
    volume: Option<IAudioEndpointVolume>,
    meter: Option<IAudioMeterInformation>,
    sessions: Option<IAudioSessionManager2>,
}

struct Worker {
    enumerator: IMMDeviceEnumerator,
    endpoints: HashMap<PcOutputId, Endpoint>,
    /// Process id → the app's name.
    /// Each process's name and key, read once.
    apps: HashMap<u32, (String, String)>,
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
                Ok(Job::TurnOn(reply)) => {
                    let disabled = self.disabled_output();
                    let result = disabled.ok_or_else(|| "No disabled Stream To Speaker endpoint".to_string())
                        .and_then(|id| crate::endpoint_name::turn_on_endpoint(id.as_str()).map_err(|e| e.to_string()));
                    self.look(shared);
                    let _ = reply.send(result);
                }
                Ok(Job::CaptureName(id, reply)) => {
                    // SAFETY: this worker owns the enumerator and reads the live endpoint's property store.
                    let name = unsafe {
                        self.enumerator.GetDevice(&windows::core::HSTRING::from(id.as_str())).ok()
                            .and_then(|device| device.OpenPropertyStore(STGM_READ).ok())
                            .and_then(|store| text_of(&store, &PKEY_Device_FriendlyName))
                    };
                    let _ = reply.send(name);
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
            seen.multimedia = self.enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok().and_then(|d| device_id(&d));
            seen.calls = self.enumerator.GetDefaultAudioEndpoint(eRender, eCommunications).ok().and_then(|d| device_id(&d));
            let active = self.render_ids(DEVICE_STATE_ACTIVE);
            self.endpoints.retain(|id, _| active.iter().any(|(a, _)| a == id));
            let looking_for_ours = !self.endpoints.values().any(|e| e.ours);
            for (id, device) in active {
                if let Some(endpoint) = self.endpoints.get_mut(&id) {
                    // The PnP parent can appear after Core Audio first lists the endpoint.
                    if looking_for_ours {
                        if let Some(info) = endpoint_info(&id, &device) { endpoint.info = info; }
                        endpoint.ours = is_ours(&endpoint.info);
                    }
                    if endpoint.ours && endpoint.volume.is_none() {
                        endpoint.volume = device.Activate(CLSCTX_ALL, None).ok();
                    }
                } else if let Some(endpoint) = open_endpoint(&id, &device) {
                    self.endpoints.insert(id, endpoint);
                }
            }
            for (id, e) in &self.endpoints {
                seen.outputs.push(e.info.clone());
                if e.ours {
                    seen.ours = Some(id.clone());
                }
                if let Some(v) = &e.volume {
                    let muted = v.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                    let level = if e.ours {
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
            if seen.ours.is_none() {
                seen.disabled_ours = self.disabled_output();
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
            let changes = (was.default != seen.default || was.multimedia != seen.multimedia, was.ours != seen.ours || was.disabled_ours != seen.disabled_ours || ids(&was) != ids(&seen));
            let first = was.outputs.is_empty() && was.default.is_none();
            *was = seen;
            if first {
                (false, false)
            } else {
                changes
            }
        };
        if outputs_changed {
            notices().tell(SystemEvent::OutputsChanged);
        }
        if default_changed {
            notices().tell(SystemEvent::DefaultOutputChanged);
        }
    }

    fn disabled_output(&self) -> Option<PcOutputId> {
        // SAFETY: only the audio worker uses its enumerator.
        unsafe { self.render_ids(DEVICE_STATE_DISABLED) }.into_iter()
            .map(|(id, _)| id).find(|id| pnp::endpoint_parent(id.as_str()).is_ok_and(|p| p.is_some()))
    }

    /// Render endpoints in the requested Windows device state.
    ///
    /// # Safety
    /// On this worker's thread only.
    unsafe fn render_ids(&self, state: DEVICE_STATE) -> Vec<(PcOutputId, IMMDevice)> {
        let Ok(all) = self.enumerator.EnumAudioEndpoints(eRender, state) else { return Vec::new() };
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
            let (app, key) = if system_sounds {
                ("System".to_string(), String::new())
            } else {
                self.apps.entry(pid).or_insert_with(|| app_name_and_key(pid)).clone()
            };
            out.push(AppAudio { app, key, output: id.clone(), peak, system_sounds });
        }
    }

    fn set_volume(&self, id: &PcOutputId, level: u8, muted: bool) -> Result<(), String> {
        let e = self.endpoints.get(id).ok_or("no such output")?;
        let v = e.volume.as_ref().ok_or("its level can't be set")?;
        let level = level.min(100);
        // SAFETY: calls on an endpoint volume this thread activated.
        unsafe {
            if e.ours {
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

fn read_device_id(device: &IMMDevice) -> anyhow::Result<String> {
    // SAFETY: GetId returns a terminated string allocated by COM. Copy it before freeing it.
    unsafe {
        let id: PWSTR = device.GetId().context("Read render endpoint ID")?;
        anyhow::ensure!(!id.is_null(), "Windows returned a null render endpoint ID");
        let text = id.to_string();
        CoTaskMemFree(Some(id.0 as *const _));
        text.context("Decode render endpoint ID")
    }
}

fn device_id(device: &IMMDevice) -> Option<PcOutputId> {
    read_device_id(device).ok().map(|id| PcOutputId::new(&id))
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

/// Read an output's names and form factor from its property store.
///
/// # Safety
/// `device` must be a live IMMDevice on this thread.
unsafe fn endpoint_info(id: &PcOutputId, device: &IMMDevice) -> Option<OutputInfo> {
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
    Some(OutputInfo { id: id.clone(), friendly, name, device: device_name, kind })
}

/// Activate the objects used to monitor this output.
///
/// # Safety
/// `device` must be a live IMMDevice on this thread.
unsafe fn open_endpoint(id: &PcOutputId, device: &IMMDevice) -> Option<Endpoint> {
    let info = endpoint_info(id, device)?;
    Some(Endpoint {
        ours: is_ours(&info),
        volume: device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok(),
        meter: device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None).ok(),
        sessions: device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok(),
        info,
    })
}

fn is_ours(info: &OutputInfo) -> bool {
    pnp::endpoint_parent(info.id.as_str()).is_ok_and(|p| p.is_some())
}

pub(crate) fn active_endpoint(accept: impl Fn(u32) -> bool) -> anyhow::Result<Option<String>> {
    // SAFETY: all interfaces stay on this thread and drop before COM is uninitialized.
    // A caller already in an STA can use its existing apartment.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE { hr.ok()?; }
        let result = (|| {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let all = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let ids = (0..all.GetCount()?).map(|i| read_device_id(&all.Item(i)?));
            super::repair_policy::find_active_endpoint(ids, pnp::endpoint_parent, accept)
        })();
        if hr.is_ok() { windows::Win32::System::Com::CoUninitialize(); }
        result
    }
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

/// An app's name for the Volume mixer and its key: the name is its file
/// description ("Spotify", "Google Chrome"), else its file name without
/// ".exe"; the key is made from its file name ([`super::app_key`]) and is
/// empty where the program can't be read.
fn app_name_and_key(pid: u32) -> (String, String) {
    let Some(path) = program_path(pid) else {
        return (format!("Process {pid}"), String::new());
    };
    let name = file_description(&path).unwrap_or_else(|| {
        let file = path.rsplit('\\').next().unwrap_or(&path);
        file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string()
    });
    (name, super::app_key(&path))
}

/// The full path of a process's program, where Windows lets this process
/// read it.
fn program_path(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: the handle is checked and closed; the buffer and its length
    // are this function's own and stay alive for the call.
    let ok = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let ok = QueryFullProcessImageNameW(h, 0, path.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        ok
    };
    ok.then(|| String::from_utf16_lossy(&path[..len as usize]))
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
    fn default_for(&self, role: Role) -> Option<PcOutputId> {
        let seen = self.0.seen.lock().unwrap();
        match role {
            Role::Console => seen.default.clone(),
            Role::Multimedia => seen.multimedia.clone(),
            Role::Communications => seen.calls.clone(),
        }
    }
    fn our_output(&self) -> Option<PcOutputId> {
        self.0.seen.lock().unwrap().ours.clone()
    }
    fn current_friendly(&self, id: &PcOutputId) -> Option<String> {
        let (tx, rx) = bounded(1);
        self.0.jobs.get()?.send(Job::CaptureName(id.clone(), tx)).ok()?;
        rx.recv_timeout(WAIT_FOR_WORKER).ok().flatten()
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

/// Windows can launch a repair even when its audio worker could not start.
struct WindowsDevice(Option<Arc<Shared>>);

impl OurDevice for WindowsDevice {
    fn can_repair(&self) -> bool { install::protected_app().is_ok() }
    fn turned_off(&self) -> bool {
        self.0.as_ref().is_some_and(|audio| audio.seen.lock().unwrap().disabled_ours.is_some())
    }
    fn turn_on(&self) -> Result<(), String> {
        self.0.as_ref().ok_or("No Windows audio worker")?.ask(Job::TurnOn)
    }
    fn repair(&self, approved: &mut dyn FnMut()) -> super::RepairExit {
        repair::run(approved)
    }

    fn rename(&self, name: &str) -> Result<(), String> {
        if self.0.is_none() { return Ok(()); }
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
static HANDBACK: Mutex<Option<super::HandbackPlan>> = Mutex::new(None);

impl Handback for Windows {
    fn hand_back_now(&self) {
        let plan = HANDBACK.lock().unwrap().clone();
        if let Some(plan) = plan { return_roles(&plan); }
    }
    fn prepare(&self, plan: &super::HandbackPlan) {
        *HANDBACK.lock().unwrap() = Some(plan.clone());
    }
}

/// Give each role back only if it still belongs to this app.
fn hand_back_now() {
    let Ok(guard) = HANDBACK.try_lock() else { return };
    let Some(plan) = guard.clone() else { return };
    drop(guard);
    return_roles(&plan);
    let _ = crate::endpoint_name::rename_endpoint_now(&plan.name);
}

fn return_roles(plan: &super::HandbackPlan) {
    let lookup = || crate::endpoint_name::find_our_endpoint_id().ok().flatten().map(|id| PcOutputId::new(&id));
    // SAFETY: the enumerator and devices stay on this thread and are dropped
    // before a successful COM initialization is balanced.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Ok(e) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            plan.return_roles(lookup, |role| {
                let role_id = windows::Win32::Media::Audio::ERole(role_number(role));
                e.GetDefaultAudioEndpoint(eRender, role_id).ok().and_then(|d| device_id(&d))
            }, |target, role| set_default(target, &[role]));
        }
        if hr.is_ok() { windows::Win32::System::Com::CoUninitialize(); }
    }
}

/// Hand the sound back before the app's own panic hook runs (which logs
/// and may show a dialog).
fn install_crash_handback(windows: &Windows) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        windows.prepare(&super::HandbackPlan::before_driver(windows));
        let earlier = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            hand_back_now();
            earlier(info);
        }));
    });
}

/// A hidden top-level window on its own thread: Windows sends it the
/// power and session messages every top-level window gets.
fn start_session_window() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG, WNDCLASSW,
        WS_OVERLAPPED,
    };
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
    match msg {
        WM_POWERBROADCAST if wparam == PBT_APMSUSPEND as usize => {
            debug!("Windows is going to sleep");
            notices().tell(SystemEvent::Suspending);
            1
        }
        WM_POWERBROADCAST if wparam == PBT_APMRESUMEAUTOMATIC as usize => {
            debug!("Windows woke");
            notices().tell(SystemEvent::Resumed);
            1
        }
        // Signing out or shutting down is fine by this app.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            // The process may end as soon as this returns: hand the sound
            // back here, then let the engine do the rest if it still can.
            info!("Windows is ending the session: giving the sound back");
            hand_back_now();
            notices().tell(SystemEvent::SessionEnding);
            0
        }
        // SAFETY: the arguments are the ones Windows passed in.
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
