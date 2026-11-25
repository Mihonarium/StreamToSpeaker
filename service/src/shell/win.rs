//! What the shell asks Windows: the monitors and their work areas, the
//! light or dark settings, and what it does to the app's own windows that
//! egui can't (exact pixel rectangles across monitors of different scales,
//! rounded corners, staying out of Alt+Tab).

use super::placement::{Monitor, PxRect};
use std::sync::Mutex;

use super::{Desktop, Foreground, KeptStyle, PanelMode, ShellWindow, PANEL_TITLE};

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// The real desktop.
#[derive(Default)]
pub struct WindowsDesktop {
    /// The panel's window this desktop last gave the flyout's look; found
    /// while it was hidden, kept for moving it once shown.
    styled: Option<isize>,
}

fn px(r: &RECT) -> PxRect {
    PxRect::new(r.left, r.top, r.right, r.bottom)
}

unsafe extern "system" fn add_monitor(m: HMONITOR, _dc: HDC, _clip: *mut RECT, data: LPARAM) -> BOOL {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
    // SAFETY: `data` is the address of the Vec that `monitors` passed to
    // EnumDisplayMonitors, alive for the whole call, which calls back on
    // the calling thread only, one call at a time.
    let list = &mut *(data as *mut Vec<Monitor>);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: `info` is a MONITORINFO with its size set, as the call asks.
    if GetMonitorInfoW(m, &mut info) == 0 {
        return 1;
    }
    let (mut dx, mut dy) = (96u32, 96u32);
    // SAFETY: two writable u32s; on failure they keep 96 (100 %).
    let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    list.push(Monitor {
        bounds: px(&info.rcMonitor),
        work: px(&info.rcWork),
        scale: dx.max(48) as f32 / 96.0,
        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
    });
    1
}

impl Desktop for WindowsDesktop {
    fn monitors(&self) -> Vec<Monitor> {
        let mut list: Vec<Monitor> = Vec::new();
        // SAFETY: the callback only writes into `list`, whose address stays
        // valid until EnumDisplayMonitors returns (it calls back before
        // returning).
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(add_monitor), &mut list as *mut Vec<Monitor> as LPARAM);
        }
        list
    }

    fn taskbar_is_dark(&self) -> bool {
        // Missing (before Windows 10 1903): the taskbar was dark.
        read_dword(PERSONALIZE, "SystemUsesLightTheme") != Some(1)
    }

    fn apps_are_dark(&self) -> bool {
        read_dword(PERSONALIZE, "AppsUseLightTheme") == Some(0)
    }

    fn place_popup(&mut self, title: &str, rect: PxRect) -> bool {
        Self::place_popup_window(title, rect)
    }

    fn place_panel(&mut self, rect: PxRect, mode: PanelMode) -> bool {
        // The window found before, while it still is the panel; else the
        // new panel, hidden until it is placed.
        let known = self.styled.map(|h| h as HWND).filter(|h| is_own_window_titled(*h, PANEL_TITLE));
        let Some(hwnd) = known.or_else(|| hidden_own_window(PANEL_TITLE)) else { return false };
        if self.styled != Some(hwnd as isize) {
            self.styled = Some(hwnd as isize);
            if mode == PanelMode::Flyout {
                round_corners(hwnd);
            }
            if let Some(keep) = ShellWindow::Panel(mode).kept_style() {
                keep_ex_style(hwnd, keep);
            }
        }
        set_rect(hwnd, rect);
        window_rect(hwnd) == Some(rect)
    }

    fn panel_window_shown(&self) -> Option<bool> {
        let hwnd = self.styled.map(|h| h as HWND).filter(|h| is_own_window_titled(*h, PANEL_TITLE))?;
        Some(is_visible(hwnd))
    }

    fn panel_ex_style(&self) -> Option<u32> {
        let hwnd = self.styled.map(|h| h as HWND).filter(|h| is_own_window_titled(*h, PANEL_TITLE))?;
        Some(ex_style(hwnd))
    }

    fn foreground(&self) -> Option<Foreground> {
        foreground()
    }
}

/// The window that has the foreground: its class, its process and program,
/// and its title when it is one of this app's windows (another program's
/// title and folder are never read: they can say what someone is doing).
fn foreground() -> Option<Foreground> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};
    // SAFETY: no arguments.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut buf = [0u16; 128];
    // SAFETY: a handle GetForegroundWindow just gave (a stale one makes the
    // call return 0); the buffer is writable for the length given.
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    let mut pid = 0u32;
    // SAFETY: as above; `pid` is a writable u32.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let own_title = (pid == std::process::id()).then(|| {
        let mut title = [0u16; 128];
        // SAFETY: as above.
        let n = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        String::from_utf16_lossy(&title[..n.max(0) as usize])
    });
    Some(Foreground { class, pid, program: program_name(pid), own_title })
}

/// The file name of the program process `pid` runs, if Windows says.
fn program_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: asks for the least access there is; a null handle on failure.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: a process handle just opened; the buffer is writable for
    // `len` characters, which the call sets to what it wrote.
    let ok = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } != 0;
    // SAFETY: the handle OpenProcess gave, closed once.
    unsafe { CloseHandle(process) };
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_string)
}

impl WindowsDesktop {
    /// The extended style bits of the small window called `title`, shown or
    /// not, if this thread has one (pop-ups have titles of their own).
    pub fn popup_ex_style(&self, title: &str) -> Option<u32> {
        own_window(title, false).map(ex_style)
    }

    /// Style and place a hidden popup window (see [`Desktop::place_popup`]).
    fn place_popup_window(title: &str, rect: PxRect) -> bool {
        let Some(hwnd) = hidden_own_window(title) else { return false };
        round_corners(hwnd);
        if let Some(keep) = ShellWindow::Popup.kept_style() {
            keep_ex_style(hwnd, keep);
        }
        set_rect(hwnd, rect);
        window_rect(hwnd) == Some(rect)
    }
}

/// A DWORD from the current user's registry, if there is one.
fn read_dword(key: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated and outlive the call; `data` and
    // `size` describe a writable u32, which is all RRF_RT_REG_DWORD writes.
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut data as *mut u32 as *mut _, &mut size)
    };
    (rc == 0).then_some(data)
}

/// The app's hidden top-level window called `title`, if this thread made
/// one: the panel before it is shown (a dialog may have the same title,
/// but it is never hidden).
pub fn hidden_own_window(title: &str) -> Option<HWND> {
    own_window(title, true)
}

/// The app's top-level window called `title`, if this thread made one;
/// with `hidden_only`, only one that is hidden.
fn own_window(title: &str, hidden_only: bool) -> Option<HWND> {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId};
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: no arguments.
    let me = unsafe { GetCurrentThreadId() };
    let mut after: HWND = std::ptr::null_mut();
    for _ in 0..64 {
        // SAFETY: a NUL-terminated title that outlives the call; no parent
        // (top-level windows), no class; `after` is null or a handle the
        // previous call returned.
        let hwnd = unsafe { FindWindowExW(std::ptr::null_mut(), after, std::ptr::null(), wide.as_ptr()) };
        if hwnd.is_null() {
            return None;
        }
        // SAFETY: a handle FindWindowExW just returned (a stale one only
        // makes the call return 0); the process id is not asked for.
        let thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
        if thread == me && !(hidden_only && is_visible(hwnd)) {
            return Some(hwnd);
        }
        after = hwnd;
    }
    None
}

/// A window's extended style bits (`GWL_EXSTYLE`).
pub fn ex_style(hwnd: HWND) -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_EXSTYLE};
    // SAFETY: a window of this thread; reading its style has no effect.
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

/// Whether `hwnd` is still a window of this thread called `title`.
fn is_own_window_titled(hwnd: HWND, title: &str) -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextW, GetWindowThreadProcessId, IsWindow};
    // SAFETY: IsWindow accepts any value and only answers.
    if unsafe { IsWindow(hwnd) } == 0 {
        return false;
    }
    // SAFETY: a live handle (a stale one makes the call return 0); no
    // process id asked for.
    let thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
    // SAFETY: no arguments.
    if thread != unsafe { GetCurrentThreadId() } {
        return false;
    }
    let mut buf = [0u16; 128];
    // SAFETY: the buffer is writable for its whole length, which is given.
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == title
}

/// Open a page of Windows' settings, on a thread of its own (Windows may
/// take a moment to start the Settings app).
pub fn open_windows_page(page: super::content::WindowsPage) {
    let uri = page.uri();
    let spawned = std::thread::Builder::new().name("stream-to-speaker-open".into()).spawn(move || {
        use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (verb, file) = (wide("open"), wide(uri));
        // SAFETY: plain COM initialisation for this new thread, which
        // ShellExecute's handlers may need.
        let hr = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
        // SAFETY: both strings are NUL-terminated and outlive the call; the
        // address is one of three fixed ms-settings: pages.
        let rc = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        if hr >= 0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        if rc as usize <= 32 {
            log::warn!("couldn't open Windows' {page:?} settings");
        }
    });
    if spawned.is_err() {
        log::warn!("couldn't open Windows' {page:?} settings: no thread");
    }
}

/// Where a window is, frame included, in physical pixels.
fn window_rect(hwnd: HWND) -> Option<PxRect> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    // SAFETY: plain data, filled by the call.
    let mut r: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: a window of this thread and a writable RECT.
    (unsafe { GetWindowRect(hwnd, &mut r) } != 0).then(|| px(&r))
}

/// Move and size a window to `rect` in physical pixels, without changing
/// its place in the stacking order or activating it. Windows is told not
/// to copy the old picture to the new place: it would sit at the new top
/// edge for a frame, so rows would jump; the new frame is drawn at once
/// instead.
fn set_rect(hwnd: HWND, rect: PxRect) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOOWNERZORDER, SWP_NOZORDER};
    // SAFETY: a window of this thread; no window to insert after.
    unsafe {
        SetWindowPos(hwnd, std::ptr::null_mut(), rect.left, rect.top, rect.width(), rect.height(), SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS);
    }
}

/// Round the corners of a window without a frame, as Windows 11 rounds
/// its flyouts (Windows 10 ignores the request).
fn round_corners(hwnd: HWND) {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
    let pref = DWMWCP_ROUND;
    // SAFETY: a window of this thread; the value is an i32 read during the
    // call, and its size is given.
    unsafe {
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &pref as *const i32 as *const _, std::mem::size_of::<i32>() as u32);
    }
}

/// The windows whose extended style the shell keeps: each with the
/// window procedure it had and the style it keeps.
static KEPT: Mutex<Vec<(isize, isize, KeptStyle)>> = Mutex::new(Vec::new());

fn kept() -> std::sync::MutexGuard<'static, Vec<(isize, isize, KeptStyle)>> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Give `hwnd` the extended style bits of `keep` now, and keep them there
/// for the window's life. winit writes a window's whole extended style again
/// whenever it changes a flag of its own (showing the window is one), with
/// no tool-window or never-activate bit and with `WS_EX_APPWINDOW`; that
/// put the panel and the root window back in Alt+Tab and the root back in
/// reach of the focus the moment they were shown. The window's procedure is
/// wrapped so that every change of its extended style keeps these bits
/// (`WM_STYLECHANGING` lets a window change the style about to be set).
pub fn keep_ex_style(hwnd: HWND, keep: KeptStyle) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWLP_WNDPROC, GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };
    {
        let mut kept = kept();
        match kept.iter_mut().find(|(h, _, _)| *h == hwnd as isize) {
            Some(entry) => entry.2 = keep,
            None => {
                // SAFETY: a window of this thread; its procedure becomes
                // `keep_style_proc`, which passes every message on to the one
                // returned here, kept with it.
                let old = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, keep_style_proc as *const () as isize) };
                if old == 0 {
                    // The bits are still given once, below; a later write
                    // of the style by winit takes them off again.
                    log::warn!("shell: Windows didn't let the shell keep a window's style; giving it once");
                } else {
                    kept.push((hwnd as isize, old, keep));
                }
            }
        }
    }
    // SAFETY: a window of this thread; reading its extended style has no
    // effect.
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let new = keep.applied_to(ex);
    if new != ex {
        // SAFETY: a window of this thread; only the kept bits change, then
        // the frame is told to take them.
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new as isize);
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        }
    }
}

/// The procedure of a window whose extended style the shell keeps: it keeps
/// the bits in every change of that style, and passes every message on.
unsafe extern "system" fn keep_style_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallWindowProcW, DefWindowProcW, GWL_EXSTYLE, STYLESTRUCT, WM_NCDESTROY, WM_STYLECHANGING, WNDPROC};
    let found = kept().iter().find(|(h, _, _)| *h == hwnd as isize).map(|(_, old, keep)| (*old, *keep));
    let Some((old, keep)) = found else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    if msg == WM_STYLECHANGING && wparam as i32 == GWL_EXSTYLE && lparam != 0 {
        // SAFETY: for WM_STYLECHANGING, lParam points at the STYLESTRUCT
        // Windows is about to apply, which the window may change.
        let style = &mut *(lparam as *mut STYLESTRUCT);
        style.styleNew = keep.applied_to(style.styleNew);
    }
    // SAFETY: the procedure the window had, a window procedure (a non-zero
    // GWLP_WNDPROC value), stored when it was replaced.
    let previous: WNDPROC = std::mem::transmute::<isize, WNDPROC>(old);
    let result = CallWindowProcW(previous, hwnd, msg, wparam, lparam);
    if msg == WM_NCDESTROY {
        kept().retain(|(h, _, _)| *h != hwnd as isize);
    }
    result
}

/// Whether a window is shown.
pub fn is_visible(hwnd: HWND) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    // SAFETY: a window of this thread; reading only.
    unsafe { IsWindowVisible(hwnd) != 0 }
}

/// Hide a window (eframe shows the root after its first frame whatever it
/// was told).
pub fn hide(hwnd: HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    // SAFETY: a window of this thread; hiding has no other effect.
    unsafe {
        ShowWindow(hwnd, SW_HIDE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, RegisterClassW, SetWindowLongW, GWL_EXSTYLE, WNDCLASSW, WS_EX_ACCEPTFILES,
        WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_OVERLAPPED,
    };

    fn is_kept(hwnd: HWND) -> bool {
        kept().iter().any(|(h, _, _)| *h == hwnd as isize)
    }

    #[test]
    fn a_wrapped_window_keeps_the_shells_bits_when_its_style_is_written_again() {
        let class: Vec<u16> = "StreamToSpeakerKeptStyleTest\0".encode_utf16().collect();
        // SAFETY: a plain window of this thread, never shown, made from a
        // class whose procedure is Windows' default one; every handle used
        // is the one CreateWindowExW returned, until DestroyWindow.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: instance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
            // Registered already when the test runs again in this process.
            let _ = RegisterClassW(&wc);
            let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
            assert!(!hwnd.is_null(), "no window for the test");
            keep_ex_style(hwnd, KeptStyle::NEVER_ACTIVE);
            assert!(is_kept(hwnd));
            // What winit 0.30 writes when it shows a window without an owner.
            SetWindowLongW(hwnd, GWL_EXSTYLE, (WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES | WS_EX_APPWINDOW) as i32);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let bits = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_APPWINDOW;
            assert_eq!(ex & bits, WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, "kept: {}", super::super::ex_style_words(ex));
            // The rest of what was written stays.
            assert_eq!(ex & (WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES), WS_EX_WINDOWEDGE | WS_EX_ACCEPTFILES);
            DestroyWindow(hwnd);
            assert!(!is_kept(hwnd), "the window is forgotten once it is gone");
        }
    }
}
