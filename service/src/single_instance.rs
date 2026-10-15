//! One GUI copy per PC, and handing a second launch over to it.
//!
//! The first GUI instance in a session owns a `Local\` named mutex and an
//! auto-reset named event. A second launch in that session finds the
//! mutex, signals the event and exits. The first instance has a thread
//! parked on the event from the start of `main` — before any window
//! exists — so the hand-off works even during startup or while the app
//! sits hidden in the tray: the request is remembered, the window is
//! shown as soon as it exists (and a start-at-sign-in launch doesn't hide
//! it), or raised right away if it already does.
//!
//! Every instance, `--headless` included, also creates the machine-wide
//! `Global\StreamToSpeaker.Singleton` mutex (in `main`), which Setup
//! checks for a running copy. A GUI launch with no window in its own
//! session to hand over to finds that mutex when a copy runs anyway:
//! with `--headless`, in another session or account, or as administrator.
//! It doesn't start then, because two copies can't work side by side: the
//! second can't bind the stream port, and the driver hands each block of
//! audio to only one reader. [`launch`] decides what it does instead.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

/// What creating the machine-wide mutex said about other copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineLock {
    /// We created it: no other copy runs on this PC.
    Created,
    /// It existed and this account could open it: a copy runs under this
    /// account, with `--headless` or in another session.
    HeldByThisAccount,
    /// It exists but this account may not open it (access denied): a copy
    /// runs in another account or as administrator.
    HeldElsewhere,
    /// Neither created nor opened, for some other reason: start anyway.
    Unavailable,
}

/// Classify `CreateMutex`'s outcome: whether it returned a handle, and
/// `GetLastError()` right after the call.
pub fn machine_lock_from(handle: bool, last_error: u32) -> MachineLock {
    match (handle, last_error) {
        (true, ERROR_ALREADY_EXISTS) => MachineLock::HeldByThisAccount,
        (true, _) => MachineLock::Created,
        (false, ERROR_ACCESS_DENIED) => MachineLock::HeldElsewhere,
        (false, _) => MachineLock::Unavailable,
    }
}

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_ALREADY_EXISTS: u32 = 183;

/// Shown to someone who starts the app while a copy runs where this
/// launch can't reach it.
pub const RUNNING_ELSEWHERE: &str = "Stream To Speaker is already running on this PC, in another \
     account or as administrator. Only one copy can run at a time.";

/// What a GUI launch that has no window in its own session to hand over
/// to does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    Run,
    /// Don't start. `log` names the cause; `tell`, if set, is shown to the
    /// user.
    Exit { log: &'static str, tell: Option<&'static str> },
}

/// `at_sign_in`: launched by the start-at-sign-in entry, which never
/// shows a message (one at every sign-in would be a nuisance).
pub fn launch(lock: MachineLock, at_sign_in: bool) -> Launch {
    match lock {
        MachineLock::Created | MachineLock::Unavailable => Launch::Run,
        MachineLock::HeldByThisAccount => Launch::Exit {
            log: "a copy is already running for this account (with --headless, or in another session)",
            tell: None,
        },
        MachineLock::HeldElsewhere => Launch::Exit {
            log: "a copy is already running on this PC that this account can't reach (access to its \
                  instance lock denied: it runs in another account or as administrator)",
            tell: (!at_sign_in).then_some(RUNNING_ELSEWHERE),
        },
    }
}

/// Main window handle once the GUI has one (0 = none yet).
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// A second launch asked to be shown and the GUI hasn't acted on it yet.
static PENDING: AtomicBool = AtomicBool::new(false);

/// GUI: register the main window so later hand-offs can raise it.
pub fn set_window(hwnd: isize) {
    WINDOW.store(hwnd, Ordering::Release);
}

/// GUI: whether a hand-off is waiting, without consuming it.
pub fn activation_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// GUI: consume a waiting hand-off (true = the user asked to see us).
pub fn take_activation() -> bool {
    PENDING.swap(false, Ordering::AcqRel)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn on_activation() {
    PENDING.store(true, Ordering::Release);
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd != 0 {
        imp::show_window(hwnd);
    }
}

/// Outcome of [`claim`].
pub enum Claim {
    /// We are the first instance; keep this alive for the process.
    First,
    /// Another instance owns the session; it has been asked to show
    /// itself. The caller should exit.
    Another,
}

/// Become the session's instance, or hand off to the one that is.
pub fn claim() -> Claim {
    imp::claim()
}

#[cfg(windows)]
mod imp {
    use super::Claim;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE,
    };

    const MUTEX_NAME: &str = "Local\\StreamToSpeaker.Instance";
    const EVENT_NAME: &str = "Local\\StreamToSpeaker.Activate";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn claim() -> Claim {
        let name = wide(MUTEX_NAME);
        // The handle is leaked on purpose: it must live as long as the
        // process.
        let (h, existed) = unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            (h, GetLastError() == ERROR_ALREADY_EXISTS)
        };
        if h.is_null() {
            log::warn!("single instance: CreateMutexW failed; continuing without a hand-off");
            return Claim::First;
        }
        if existed {
            signal_first();
            return Claim::Another;
        }
        let ev_name = wide(EVENT_NAME);
        let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, ev_name.as_ptr()) };
        if ev.is_null() {
            log::warn!("single instance: CreateEventW failed; a second launch can't raise us");
            return Claim::First;
        }
        let ev = ev as isize;
        std::thread::Builder::new()
            .name("stream-to-speaker-activate".into())
            .spawn(move || loop {
                let rc = unsafe { WaitForSingleObject(ev as HANDLE, INFINITE) };
                if rc != 0 {
                    // WAIT_FAILED etc.: don't spin.
                    log::warn!("single instance: activation wait failed ({})", rc);
                    return;
                }
                log::info!("another launch asked this instance to show itself");
                super::on_activation();
            })
            .ok();
        Claim::First
    }

    /// Wake the first instance. Its event can lag its mutex by a moment
    /// at startup, so retry briefly.
    fn signal_first() {
        let name = wide(EVENT_NAME);
        for _ in 0..20 {
            let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
            if !h.is_null() {
                unsafe {
                    // We are the process the user just launched, so we
                    // may lend the foreground to the instance we wake.
                    windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                        windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
                    );
                    SetEvent(h);
                    windows_sys::Win32::Foundation::CloseHandle(h);
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        log::warn!("single instance: the running instance didn't answer the hand-off");
    }

    /// Show + raise from another thread (same sequence the tray uses).
    pub fn show_window(hwnd: isize) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsIconic, SetForegroundWindow, SetWindowPos, ShowWindowAsync, HWND_TOP, SWP_NOMOVE,
            SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE,
        };
        unsafe {
            let h = hwnd as _;
            SetWindowPos(h, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
            if IsIconic(h) != 0 {
                ShowWindowAsync(h, SW_RESTORE);
            }
            SetForegroundWindow(h);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Claim;
    pub fn claim() -> Claim {
        Claim::First
    }
    #[allow(dead_code)]
    pub fn show_window(_hwnd: isize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_mutex_outcomes() {
        assert_eq!(machine_lock_from(true, 0), MachineLock::Created);
        assert_eq!(machine_lock_from(true, 183), MachineLock::HeldByThisAccount);
        assert_eq!(machine_lock_from(false, 5), MachineLock::HeldElsewhere);
        assert_eq!(machine_lock_from(false, 1450), MachineLock::Unavailable);
    }

    #[cfg(windows)]
    #[test]
    fn error_codes_are_windows_own() {
        use windows_sys::Win32::Foundation as F;
        assert_eq!(ERROR_ACCESS_DENIED, F::ERROR_ACCESS_DENIED);
        assert_eq!(ERROR_ALREADY_EXISTS, F::ERROR_ALREADY_EXISTS);
    }

    #[test]
    fn a_copy_out_of_reach_is_reported_to_whoever_started_the_app() {
        match launch(MachineLock::HeldElsewhere, false) {
            Launch::Exit { log, tell } => {
                assert_eq!(
                    tell,
                    Some(
                        "Stream To Speaker is already running on this PC, in another account or as \
                         administrator. Only one copy can run at a time."
                    )
                );
                assert!(log.contains("denied") && log.contains("another account"), "{log}");
            }
            Launch::Run => panic!("started next to another copy"),
        }
        // At sign-in: the same exit, logged only.
        match launch(MachineLock::HeldElsewhere, true) {
            Launch::Exit { log, tell } => {
                assert_eq!(tell, None);
                assert!(log.contains("another account"), "{log}");
            }
            Launch::Run => panic!("started next to another copy"),
        }
    }

    #[test]
    fn this_accounts_own_copy_keeps_the_quiet_exit() {
        for at_sign_in in [false, true] {
            match launch(MachineLock::HeldByThisAccount, at_sign_in) {
                Launch::Exit { log, tell } => {
                    assert_eq!(tell, None);
                    assert!(!log.contains("administrator"), "{log}");
                }
                Launch::Run => panic!("started next to another copy"),
            }
            assert_eq!(launch(MachineLock::Created, at_sign_in), Launch::Run);
            assert_eq!(launch(MachineLock::Unavailable, at_sign_in), Launch::Run);
        }
    }

    #[test]
    fn activation_is_remembered_until_taken() {
        // No window registered: the request waits for the GUI.
        on_activation();
        assert!(activation_pending());
        assert!(take_activation());
        assert!(!activation_pending());
        assert!(!take_activation());
    }
}
