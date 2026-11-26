//! The media sessions reached by Windows' transport controls.

use crate::platform::app_key;

struct Process {
    pid: u32,
    file: String,
    aumid: Option<String>,
    family: Option<String>,
}

fn matching_process<'a>(id: &str, processes: &'a [Process]) -> Option<&'a Process> {
    if id.is_empty() || id.contains(['/', '\\']) {
        return None;
    }
    let key = app_key(id);
    if let Some(p) = processes.iter().find(|p| app_key(&p.file) == key) {
        return Some(p);
    }
    let (family, _) = id.split_once('!')?;
    processes
        .iter()
        .find(|p| p.aumid.as_deref() == Some(id))
        .or_else(|| processes.iter().find(|p| p.family.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(family))))
}

#[cfg(windows)]
pub(super) use native::players;

#[cfg(windows)]
mod native {
    use super::{app_key, matching_process, Process};
    use crate::now_playing::{NowPlaying, Session};
    use crate::platform::win::{file_description, find_process, notices, program_path, text_of, Apartment};
    use crate::platform::{Player, PlayerStatus, Players, SystemEvent};
    use crossbeam_channel::{at, bounded, never, select, tick, unbounded, Receiver, Sender};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use windows::core::{Owned, RuntimeType, HSTRING, PWSTR};
    use windows::ApplicationModel::AppInfo;
    use windows::Foundation::{
        AsyncOperationCompletedHandler, AsyncStatus, EventRegistrationToken, IAsyncOperation, TypedEventHandler,
    };
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as MediaSession,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionMediaProperties as Properties,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };
    use windows::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, WIN32_ERROR};
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::Storage::Packaging::Appx::{
        GetApplicationUserModelId, GetPackageFamilyName, APPLICATION_USER_MODEL_ID_MAX_LENGTH,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};

    const REFRESH: Duration = Duration::from_secs(2);
    const RESPONSE_TIME: Duration = Duration::from_secs(2);

    fn package_ids(pid: u32) -> (Option<String>, Option<String>) {
        // SAFETY: Owned closes this query-only process handle. Each output is
        // a writable UTF-16 buffer with its capacity supplied in characters.
        unsafe {
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return (None, None) };
            let process = Owned::new(process);
            let mut buf = [0; APPLICATION_USER_MODEL_ID_MAX_LENGTH as usize + 1];
            let mut read = |get: unsafe fn(HANDLE, *mut u32, PWSTR) -> WIN32_ERROR| {
                let mut len = buf.len() as u32;
                if get(*process, &mut len, PWSTR(buf.as_mut_ptr())).is_err() {
                    return None;
                }
                let chars = buf.get(..len as usize)?.split(|&c| c == 0).next()?;
                (!chars.is_empty()).then(|| String::from_utf16_lossy(chars))
            };
            (read(GetApplicationUserModelId), read(GetPackageFamilyName))
        }
    }

    fn window_process(id: &str) -> Option<u32> {
        // EnumWindows invokes this synchronously with the vector supplied below.
        unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> BOOL {
            // SAFETY: EnumWindows supplies a top-level HWND. Its caller lends
            // the vector exclusively to these serial callbacks until it returns.
            unsafe {
                if IsWindowVisible(hwnd).as_bool() {
                    (&mut *(param.0 as *mut Vec<HWND>)).push(hwnd);
                }
            }
            true.into()
        }
        let mut windows = Vec::<HWND>::new();
        // SAFETY: the vector outlives the enumeration. Window handles are
        // borrowed from the OS; a closed window simply fails its property read.
        unsafe {
            EnumWindows(Some(collect), LPARAM((&raw mut windows) as isize)).ok()?;
            windows.into_iter().find_map(|hwnd| {
                let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
                if text_of(&store, &PKEY_AppUserModel_ID).as_deref() != Some(id) {
                    return None;
                }
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                (pid != 0).then_some(pid)
            })
        }
    }

    fn resolve(id: &str) -> (Option<String>, Option<String>) {
        let mut processes = Vec::new();
        find_process(|pid, file| {
            processes.push(Process { pid, file: file.into(), aumid: None, family: None });
            false
        });
        if id.contains('!') && matching_process(id, &processes).is_none() {
            for p in &mut processes {
                (p.aumid, p.family) = package_ids(p.pid);
            }
        }
        let found = matching_process(id, &processes);
        let pid = found.map(|p| p.pid).or_else(|| window_process(id));
        let path = pid.and_then(program_path);
        let file = pid.and_then(|pid| processes.iter().find(|p| p.pid == pid)).map(|p| p.file.as_str());
        let key = path.as_deref().or(file).map(app_key);
        let name = AppInfo::GetFromAppUserModelId(&HSTRING::from(id))
            .and_then(|a| a.DisplayInfo())
            .and_then(|d| d.DisplayName())
            .ok()
            .map(|s| s.to_string_lossy())
            .filter(|s| !s.is_empty())
            .or_else(|| path.as_deref().and_then(file_description));
        (key, name)
    }

    fn title_of(props: &Properties) -> Option<NowPlaying> {
        let text =
            |s: windows::core::Result<HSTRING>| s.map(|s| s.to_string_lossy().trim().to_string()).unwrap_or_default();
        let np =
            NowPlaying { title: text(props.Title()), artist: text(props.Artist()), album: text(props.AlbumTitle()) };
        (!np.is_empty()).then_some(np)
    }

    fn on_change<A: RuntimeType + 'static, B: RuntimeType + 'static>(wake: &Sender<()>) -> TypedEventHandler<A, B> {
        let wake = wake.clone();
        TypedEventHandler::new(move |_, _| {
            let _ = wake.try_send(());
            Ok(())
        })
    }

    // Completion wakes the worker. Result extraction, cache updates and sink
    // calls take place outside Windows' event handlers.
    fn on_completion<T: RuntimeType + 'static>(op: &IAsyncOperation<T>, wake: &Sender<()>) {
        let wake = wake.clone();
        let _ = op.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
            let _ = wake.try_send(());
            Ok(())
        }));
    }

    struct Entry {
        session: MediaSession,
        source: String,
        player: Player,
        legacy_playing: bool,
        playback: Option<EventRegistrationToken>,
        media: Option<EventRegistrationToken>,
        title_read: Option<(Instant, IAsyncOperation<Properties>)>,
        reread: bool,
    }

    impl Entry {
        fn new(session: MediaSession, entries: &[Entry], wake: &Sender<()>) -> Self {
            let source = session.SourceAppUserModelId().map(|s| s.to_string_lossy()).unwrap_or_default();
            let base = if source.is_empty() { "Player" } else { &source };
            let mut id = base.to_owned();
            let mut suffix = 2;
            while entries.iter().any(|e| e.player.id == id) {
                id = format!("{base}#{suffix}");
                suffix += 1;
            }
            let (app, name) = resolve(&source);
            let player = Player { id, app, name, status: PlayerStatus::Other, title: None };
            let playback = session.PlaybackInfoChanged(&on_change(wake)).ok();
            let media = session.MediaPropertiesChanged(&on_change(wake)).ok();
            Self { session, source, player, legacy_playing: true, playback, media, title_read: None, reread: false }
        }

        fn refresh(&mut self, ready: &Sender<()>) {
            let status = self.session.GetPlaybackInfo().and_then(|info| info.PlaybackStatus()).ok();
            self.legacy_playing = status.is_none_or(|s| s == Status::Playing);
            self.player.status = match status {
                Some(Status::Playing) => PlayerStatus::Playing,
                Some(Status::Paused) => PlayerStatus::Paused,
                Some(Status::Stopped) => PlayerStatus::Stopped,
                _ => PlayerStatus::Other,
            };
            self.reread = true;
            self.update_title(ready);
        }

        fn update_title(&mut self, ready: &Sender<()>) {
            if let Some((at, op)) = &self.title_read {
                if op.Status() == Ok(AsyncStatus::Started) {
                    if at.elapsed() < REFRESH {
                        return;
                    }
                    let _ = op.Cancel();
                    self.player.title = None;
                } else {
                    self.player.title = op.GetResults().ok().and_then(|props| title_of(&props));
                }
                self.title_read = None;
            }
            if std::mem::take(&mut self.reread) {
                if let Ok(op) = self.session.TryGetMediaPropertiesAsync() {
                    on_completion(&op, ready);
                    self.title_read = Some((Instant::now(), op));
                } else {
                    self.player.title = None;
                }
            }
        }
    }

    impl Drop for Entry {
        fn drop(&mut self) {
            if let Some(token) = self.playback {
                let _ = self.session.RemovePlaybackInfoChanged(token);
            }
            if let Some(token) = self.media {
                let _ = self.session.RemoveMediaPropertiesChanged(token);
            }
            if let Some((_, op)) = &self.title_read {
                let _ = op.Cancel();
            }
        }
    }

    struct Watch {
        manager: Manager,
        sessions: Option<EventRegistrationToken>,
        focus: Option<EventRegistrationToken>,
    }

    impl Watch {
        fn open(wake: &Sender<()>) -> Option<Self> {
            let op = Manager::RequestAsync().ok()?;
            let (tx, rx) = bounded(1);
            on_completion(&op, &tx);
            if rx.recv_timeout(RESPONSE_TIME).is_err() {
                let _ = op.Cancel();
                return None;
            }
            let manager = op.GetResults().ok()?;
            let sessions = manager.SessionsChanged(&on_change(wake)).ok();
            let focus = manager.CurrentSessionChanged(&on_change(wake)).ok();
            Some(Self { manager, sessions, focus })
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            if let Some(t) = self.sessions {
                let _ = self.manager.RemoveSessionsChanged(t);
            }
            if let Some(t) = self.focus {
                let _ = self.manager.RemoveCurrentSessionChanged(t);
            }
        }
    }

    #[derive(Default)]
    struct Snapshot {
        players: Vec<Player>,
        focused: Option<Session>,
    }

    struct Command {
        id: String,
        play: bool,
        deadline: Instant,
        reply: Sender<Result<(), String>>,
    }

    struct Action {
        request: Command,
        operation: IAsyncOperation<bool>,
    }

    impl Command {
        fn start(self, entries: &[Entry], ready: &Sender<()>) -> Option<Action> {
            if Instant::now() >= self.deadline {
                return None;
            }
            let Some(entry) = entries.iter().find(|e| e.player.id == self.id) else {
                let _ = self.reply.try_send(Err("That player is gone.".into()));
                return None;
            };
            let operation = if self.play { entry.session.TryPlayAsync() } else { entry.session.TryPauseAsync() };
            match operation {
                Ok(operation) => {
                    on_completion(&operation, ready);
                    Some(Action { request: self, operation })
                }
                Err(_) => {
                    let _ = self.reply.try_send(Err("Windows could not reach that player.".into()));
                    None
                }
            }
        }
    }

    impl Action {
        fn waiting(&self) -> bool {
            let result = if self.operation.Status() == Ok(AsyncStatus::Started) {
                if Instant::now() < self.request.deadline {
                    return true;
                }
                let _ = self.operation.Cancel();
                Err("The player did not answer in time.".into())
            } else if self.operation.GetResults() == Ok(true) {
                Ok(())
            } else {
                Err("The player did not accept the request.".into())
            };
            let _ = self.request.reply.try_send(result);
            false
        }
    }

    pub(in crate::platform::win) struct WindowsPlayers {
        seen: Mutex<Snapshot>,
        commands: Sender<Command>,
    }

    impl WindowsPlayers {
        fn ask(&self, id: &str, play: bool) -> Result<(), String> {
            let deadline = Instant::now() + RESPONSE_TIME;
            if !self.seen.lock().unwrap().players.iter().any(|p| p.id == id) {
                return Err("That player is gone.".into());
            }
            let (reply, rx) = bounded(1);
            self.commands
                .send(Command { id: id.into(), play, deadline, reply })
                .map_err(|_| "The player service has stopped.")?;
            rx.recv_deadline(deadline).unwrap_or_else(|_| Err("The player did not answer in time.".into()))
        }

        fn publish(&self, entries: &[Entry], focus: Option<&MediaSession>, notified: bool) {
            let players = entries.iter().map(|e| e.player.clone()).collect();
            let focused = entries.iter().find(|e| Some(&e.session) == focus).map(|e| Session {
                app: (!e.source.trim().is_empty()).then(|| e.source.trim().to_string()),
                playing: e.legacy_playing,
                title: e.player.title.clone(),
            });
            let changed = {
                let mut seen = self.seen.lock().unwrap();
                let changed = seen.players != players || seen.focused != focused;
                *seen = Snapshot { players, focused };
                changed
            };
            if notified || changed {
                notices().tell(SystemEvent::PlayersChanged);
            }
        }

        fn monitor(&self, commands: Receiver<Command>) {
            let Ok(_apartment) = Apartment::enter() else { return };
            let (wake, events) = bounded(1);
            let (ready, titles) = bounded(1);
            let timer = tick(REFRESH);
            let mut watch = None;
            let mut entries = Vec::<Entry>::new();
            let mut actions = Vec::new();
            let mut notified = false;
            loop {
                if watch.is_none() {
                    watch = Watch::open(&wake);
                }
                let sessions = watch.as_ref().and_then(|w| w.manager.GetSessions().ok());
                let focus = if let Some(list) = sessions {
                    let sessions: Vec<_> = list.into_iter().collect();
                    // WinRT session equality compares COM identity. Keep the
                    // id, process lookup and event tokens when the list reorders.
                    entries.retain(|e| sessions.contains(&e.session));
                    for session in sessions {
                        if !entries.iter().any(|e| e.session == session) {
                            entries.push(Entry::new(session, &entries, &wake));
                        }
                    }
                    for e in &mut entries {
                        e.refresh(&ready);
                    }
                    watch.as_ref().and_then(|w| w.manager.GetCurrentSession().ok())
                } else {
                    entries.clear();
                    watch = None;
                    None
                };
                self.publish(&entries, focus.as_ref(), notified);
                loop {
                    actions.retain(Action::waiting);
                    let timeout = actions.iter().map(|a| a.request.deadline).min().map(at).unwrap_or_else(never);
                    select! {
                        recv(timeout) -> _ => {}
                        recv(events) -> _ => { notified = true; break; }
                        recv(timer) -> _ => { notified = false; break; }
                        recv(titles) -> _ => {
                            for e in &mut entries { e.update_title(&ready); }
                            self.publish(&entries, focus.as_ref(), false);
                        }
                        recv(commands) -> command => match command {
                            Ok(c) => if let Some(action) = c.start(&entries, &ready) { actions.push(action); },
                            Err(_) => return,
                        }
                    }
                }
            }
        }
    }

    impl Players for WindowsPlayers {
        fn list(&self) -> Vec<Player> {
            self.seen.lock().unwrap().players.clone()
        }
        fn pause(&self, id: &str) -> Result<(), String> {
            self.ask(id, false)
        }
        fn play(&self, id: &str) -> Result<(), String> {
            self.ask(id, true)
        }
    }

    pub(in crate::platform::win) fn players() -> &'static Arc<WindowsPlayers> {
        static PLAYERS: OnceLock<Arc<WindowsPlayers>> = OnceLock::new();
        PLAYERS.get_or_init(|| {
            let (commands, rx) = unbounded();
            let players = Arc::new(WindowsPlayers { seen: Mutex::default(), commands });
            let worker = players.clone();
            if let Err(e) =
                std::thread::Builder::new().name("stream-to-speaker-players".into()).spawn(move || worker.monitor(rx))
            {
                log::debug!("Could not start the player monitor: {e}");
            }
            players
        })
    }

    /// The session Windows has given media focus, from the media worker's last read.
    /// Nothing calls it: `now_playing` still reads Windows by itself.
    #[allow(dead_code)]
    pub fn current_session() -> Option<Session> {
        players().seen.lock().unwrap().focused.clone()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn lists_players_and_refuses_a_missing_session() {
            let players = players();
            let _ = players.list();
            assert!(players.pause("no-such-id").is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_executable_then_package_identity_then_family() {
        let processes = [
            (1, "Spotify.EXE", None, None),
            (2, "chrome.exe", None, None),
            (3, "msedge.exe", None, None),
            (4, "helper.exe", Some("Family!Helper"), Some("Family")),
            (5, "music.exe", Some("Family!Player"), Some("Family")),
        ]
        .map(|(pid, file, id, family)| Process {
            pid,
            file: file.into(),
            aumid: id.map(str::to_owned),
            family: family.map(str::to_owned),
        });
        let matched = |id| matching_process(id, &processes).map(|p| (p.pid, app_key(&p.file)));
        for (id, pid, key) in [
            ("Spotify.exe", 1, "spotify"),
            ("Chrome", 2, "chrome"),
            ("MSEdge", 3, "msedge"),
            ("Family!Player", 5, "music"),
            ("Family!Unlisted", 4, "helper"),
        ] {
            assert_eq!(matched(id), Some((pid, key.into())));
        }
        for id in ["", "firefox-explicit-id", "Unknown!Player", r"C:\fake\msedge.exe"] {
            assert_eq!(matched(id), None);
        }
    }
}
