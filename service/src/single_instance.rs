//! One GUI instance per user session, and handing a second launch over
//! to it.
//!
//! The first instance owns a `Local\` named mutex (per session, so other
//! users signed in on the same PC run their own instance) and an
//! auto-reset named event. A second launch finds the mutex, signals the
//! event and exits. The first instance has a thread parked on the event
//! from the start of `main` — before any window exists — so the hand-off
//! works even during startup or while the app sits hidden in the tray:
//! the request is remembered, the window is shown as soon as it exists
//! (and a start-at-sign-in launch doesn't hide it), or raised right away
//! if it already does.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

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
    fn activation_is_remembered_until_taken() {
        // No window registered: the request waits for the GUI.
        on_activation();
        assert!(activation_pending());
        assert!(take_activation());
        assert!(!activation_pending());
        assert!(!take_activation());
    }
}
