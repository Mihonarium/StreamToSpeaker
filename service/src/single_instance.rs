//! One GUI copy per PC, and handing a second launch over to it.
//!
//! The first GUI instance in a session owns a `Local\` named mutex and an
//! auto-reset named event. A second launch in that session finds the
//! mutex, signals the event and exits. The first instance has a thread
//! parked on the event from the start of `main` — before any window
//! exists — so the hand-off works even during startup or while the app
//! sits hidden in the tray: the request is remembered, the window is
//! shown as soon as it exists (and a start-at-sign-in launch doesn't hide
//! it), or raised right away if it already does. A copy that is quitting
//! says so first: it has no window left to show, so a launch then waits
//! for it to end and starts in its place.
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
use std::time::Duration;

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

/// Shown to someone who starts the app while a copy in their session is
/// still quitting after [`CLOSING_WAIT`].
pub const STILL_CLOSING: &str = "Stream To Speaker is still closing. Start it again in a moment.";

/// How long a launch waits for a copy that is quitting to end, and how
/// often it looks. A quit waits for the speaker a few seconds at most.
#[cfg_attr(not(windows), allow(dead_code))]
const CLOSING_WAIT: Duration = Duration::from_secs(15);
#[cfg_attr(not(windows), allow(dead_code))]
const CLOSING_POLL: Duration = Duration::from_millis(100);

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
#[derive(Debug)]
pub enum Claim {
    /// We are the first instance; keep this alive for the process.
    First,
    /// Another instance owns the session; it has been asked to show
    /// itself. The caller should exit.
    Another,
    /// A copy in this session is quitting and hadn't ended after
    /// [`CLOSING_WAIT`]. The caller should exit, saying [`STILL_CLOSING`].
    StillClosing,
}

/// Become the session's instance, or hand off to the one that is; wait
/// for one that is quitting.
pub fn claim() -> Claim {
    imp::claim()
}

/// Say that this copy is quitting, as it stops showing its window: a launch
/// from now on waits for it to end instead of handing over to it.
pub fn announce_closing() {
    imp::announce_closing()
}

/// One look at the session's instance.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Seen {
    /// Another copy holds the session's instance lock (else this look took
    /// it).
    exists: bool,
    /// That copy has said it is quitting.
    closing: bool,
}

/// Look until no copy holds the session (this launch is first), one runs
/// (hand over to it), or `pause` gives up on one that is quitting.
#[cfg_attr(not(windows), allow(dead_code))]
fn claim_with(mut look: impl FnMut() -> Seen, mut pause: impl FnMut() -> bool) -> Claim {
    loop {
        let seen = look();
        if !seen.exists {
            return Claim::First;
        }
        if !seen.closing {
            return Claim::Another;
        }
        if !pause() {
            return Claim::StillClosing;
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{Claim, Seen};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE,
    };

    const MUTEX_NAME: &str = "Local\\StreamToSpeaker.Instance";
    const EVENT_NAME: &str = "Local\\StreamToSpeaker.Activate";
    /// Exists while the session's copy is quitting.
    const CLOSING_NAME: &str = "Local\\StreamToSpeaker.Closing";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Take the session's instance lock if no copy holds it; else whether
    /// the copy that does is quitting.
    fn look() -> Seen {
        let name = wide(MUTEX_NAME);
        let (h, existed) = unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            (h, GetLastError() == ERROR_ALREADY_EXISTS)
        };
        if h.is_null() {
            log::warn!("single instance: CreateMutexW failed; continuing without a hand-off");
            return Seen { exists: false, closing: false };
        }
        if !existed {
            // Ours now; the handle is leaked on purpose: it must live as
            // long as the process.
            return Seen { exists: false, closing: false };
        }
        // Theirs: don't keep it alive past their exit.
        unsafe { CloseHandle(h) };
        let closing_name = wide(CLOSING_NAME);
        let closing = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, closing_name.as_ptr()) };
        if !closing.is_null() {
            unsafe { CloseHandle(closing) };
        }
        Seen { exists: true, closing: !closing.is_null() }
    }

    pub fn claim() -> Claim {
        let started = std::time::Instant::now();
        let claim = super::claim_with(look, || {
            if started.elapsed() >= super::CLOSING_WAIT {
                return false;
            }
            std::thread::sleep(super::CLOSING_POLL);
            true
        });
        match claim {
            Claim::First => {}
            Claim::Another => {
                signal_first();
                return Claim::Another;
            }
            Claim::StillClosing => return Claim::StillClosing,
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

    pub fn announce_closing() {
        let name = wide(CLOSING_NAME);
        // Leaked on purpose: it says "quitting" until this process ends.
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        if h.is_null() {
            log::warn!("single instance: couldn't say this copy is quitting; a launch meanwhile may do nothing");
        }
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
    pub fn announce_closing() {}
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

    fn seen(exists: bool, closing: bool) -> Seen {
        Seen { exists, closing }
    }

    /// `claim_with` over these looks, with a pause that allows `pauses`.
    fn claim_over(looks: &[Seen], pauses: usize) -> (Claim, usize) {
        let mut looks = looks.iter().copied();
        let mut paused = 0;
        let claim = claim_with(
            || looks.next().expect("looked once too often"),
            || {
                paused += 1;
                paused <= pauses
            },
        );
        (claim, paused)
    }

    #[test]
    fn a_launch_waits_for_a_copy_that_is_closing_and_then_starts() {
        let (claim, paused) = claim_over(&[seen(true, true), seen(true, true), seen(false, false)], 100);
        assert!(matches!(claim, Claim::First));
        assert_eq!(paused, 2);
    }

    #[test]
    fn a_launch_hands_over_to_a_copy_that_runs() {
        assert!(matches!(claim_over(&[seen(true, false)], 100).0, Claim::Another));
        assert!(matches!(claim_over(&[seen(false, false)], 100).0, Claim::First));
    }

    #[test]
    fn a_copy_that_takes_too_long_to_close_is_reported() {
        let (claim, _) = claim_over(&[seen(true, true); 4], 3);
        assert!(matches!(claim, Claim::StillClosing));
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
