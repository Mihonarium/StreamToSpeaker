//! Session-wide delay keys. The thread that registers them also releases them.
use crate::platform::{DelayKeys, DelayPress, Hotkeys, KeyRegistration, KeySink, KeyStatus};
use std::{
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_HOTKEY, WM_QUIT,
};

pub(super) struct WindowsKeys;

pub(crate) fn windows_key_down() -> bool {
    // SAFETY: GetKeyState reads these virtual keys on the window thread and takes no pointers.
    unsafe { GetKeyState(i32::from(VK_LWIN)) < 0 || GetKeyState(i32::from(VK_RWIN)) < 0 }
}

struct Worker {
    id: u32,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // SAFETY: the worker created its queue before sending its id. WM_QUIT carries no pointers.
        unsafe { PostThreadMessageW(self.id, WM_QUIT, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Hotkeys for WindowsKeys {
    fn register(&self, keys: &DelayKeys, sink: KeySink) -> KeyRegistration {
        let keys = keys.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = thread::Builder::new()
            .name("stream-to-speaker-delay-keys".into())
            .spawn(move || {
                // SAFETY: MSG contains only integer and pointer fields and accepts zero initialization.
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                // SAFETY: PeekMessageW creates this thread's queue and can write to msg.
                unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE) };
                let mut registered = [KeyStatus::Rejected; 4];
                let mut used = Vec::new();
                for (i, press) in DelayPress::ALL.into_iter().enumerate() {
                    let Some(chord) = combination(&press.keys(&keys)) else {
                        continue;
                    };
                    if used.contains(&chord) { continue; }
                    // SAFETY: a null HWND associates the integer id with this thread's queue.
                    let ok = unsafe { RegisterHotKey(null_mut(), i as i32 + 1, chord.0 | MOD_NOREPEAT, chord.1) };
                    if ok != 0 {
                        registered[i] = KeyStatus::Registered;
                        used.push(chord);
                    } else {
                        // SAFETY: read this thread's error immediately after RegisterHotKey failed.
                        if unsafe { windows_sys::Win32::Foundation::GetLastError() }
                            == windows_sys::Win32::Foundation::ERROR_HOTKEY_ALREADY_REGISTERED {
                            registered[i] = KeyStatus::Taken;
                        }
                    }
                }
                // SAFETY: reads the id of the thread making this call.
                if ready
                    .send((unsafe { GetCurrentThreadId() }, registered))
                    .is_ok()
                {
                    // SAFETY: msg is writable and the queue belongs to this thread.
                    while !stop.load(Ordering::Acquire)
                        && unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0
                    {
                        if msg.message == WM_HOTKEY {
                            if let Some(i) = msg
                                .wParam
                                .checked_sub(1)
                                .filter(|i| registered.get(*i) == Some(&KeyStatus::Registered))
                            {
                                sink(DelayPress::ALL[i]);
                            }
                        }
                    }
                }
                for (i, ok) in registered.into_iter().enumerate() {
                    if ok == KeyStatus::Registered {
                        // SAFETY: this thread owns the registration with this id.
                        unsafe { UnregisterHotKey(null_mut(), i as i32 + 1) };
                    }
                }
            });
        if let Ok(thread) = thread {
            if let Ok((id, registered)) = started.recv() {
                return KeyRegistration::new(
                    registered,
                    Worker {
                        id,
                        stopping,
                        thread: Some(thread),
                    },
                );
            }
        }
        KeyRegistration::new([KeyStatus::Rejected; 4], ())
    }
}

fn combination(keys: &[String]) -> Option<(u32, u32)> {
    let (key, prefix) = keys.split_last()?;
    let mut mods = 0;
    for m in prefix {
        mods |= match m.as_str() {
            "Ctrl" => MOD_CONTROL,
            "Alt" => MOD_ALT,
            "Shift" => MOD_SHIFT,
            "Win" => MOD_WIN,
            _ => return None,
        };
    }
    if mods & (MOD_CONTROL | MOD_ALT) == 0 {
        return None;
    }
    let vk = match key.as_str() {
        "Down" => VK_DOWN,
        "Left" => VK_LEFT,
        "Right" => VK_RIGHT,
        "Up" => VK_UP,
        "Tab" => VK_TAB,
        "Backspace" => VK_BACK,
        "Enter" => VK_RETURN,
        "Space" => VK_SPACE,
        "Insert" => VK_INSERT,
        "Delete" => VK_DELETE,
        "Home" => VK_HOME,
        "End" => VK_END,
        "Page Up" => VK_PRIOR,
        "Page Down" => VK_NEXT,
        name if name.starts_with('F') && name.len() > 1 => {
            let n: u16 = name[1..].parse().ok()?;
            if !(1..=24).contains(&n) {
                return None;
            }
            VK_F1 + n - 1
        }
        name if name.len() == 1 => {
            // The layout supplies the virtual key and any modifiers its character requires.
            let ch = name.as_bytes()[0].to_ascii_lowercase();
            // SAFETY: VkKeyScanW takes one UTF-16 code unit and retains no pointers.
            let scan = unsafe { VkKeyScanW(u16::from(ch)) };
            if scan == -1 {
                return None;
            }
            if scan & 0x100 != 0 {
                mods |= MOD_SHIFT;
            }
            if scan & 0x200 != 0 {
                mods |= MOD_CONTROL;
            }
            if scan & 0x400 != 0 {
                mods |= MOD_ALT;
            }
            scan as u16 & 0xff
        }
        _ => return None,
    };
    Some((mods, u32::from(vk)))
}
