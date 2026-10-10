//! Start at sign-in.
//!
//! A per-user `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value
//! named after the product, holding the quoted exe path plus
//! [`STARTUP_FLAG`], which makes the app start in the tray without
//! opening its window. The installer's optional "start when I sign in"
//! task writes the same value with the same content, so the in-app switch
//! and the installer can't disagree: the value itself is the setting
//! (there is no copy in config.json to drift from it).
//!
//! On every GUI launch an existing value is rewritten to point at the
//! running exe, so a moved install (or one the installer wrote before the
//! flag existed) heals itself. A missing value is left missing.

/// Command-line flag the Run entry passes: start hidden in the tray.
pub const STARTUP_FLAG: &str = "--startup";

/// Name of the Run value (shared with the installer's autostart task).
pub const VALUE_NAME: &str = crate::PRODUCT_NAME;

/// The Run value's data for `exe`.
pub fn command_line(exe: &std::path::Path) -> String {
    format!("\"{}\" {}", exe.display(), STARTUP_FLAG)
}

/// Whether the app is set to start at sign-in.
pub fn is_enabled() -> bool {
    imp::read().is_some()
}

/// Turn start-at-sign-in on (pointing at the running exe) or off.
pub fn set_enabled(on: bool) -> Result<(), String> {
    if on {
        let exe = std::env::current_exe().map_err(|e| format!("locating the app: {}", e))?;
        imp::write(&command_line(&exe))
    } else {
        imp::delete()
    }
}

/// Launch-time self-heal: if the value exists but doesn't match the
/// running exe (moved install, or written without the flag), rewrite it.
pub fn reapply() {
    let Some(current) = imp::read() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let wanted = command_line(&exe);
    if current != wanted {
        match imp::write(&wanted) {
            Ok(()) => log::info!("autostart: updated sign-in entry to {}", wanted),
            Err(e) => log::warn!("autostart: couldn't update sign-in entry: {}", e),
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::VALUE_NAME;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read() -> Option<String> {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let mut len: u32 = 0;
        // First call sizes the buffer (bytes, including the terminator).
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS || len < 2 || len > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    fn open_run_key() -> Result<HKEY, String> {
        let key = wide(RUN_KEY);
        let mut h: HKEY = std::ptr::null_mut();
        let rc =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut h) };
        if rc != ERROR_SUCCESS {
            return Err(format!("opening the Run key failed (error {})", rc));
        }
        Ok(h)
    }

    pub fn write(data: &str) -> Result<(), String> {
        let h = open_run_key()?;
        let name = wide(VALUE_NAME);
        let value = wide(data);
        let rc = unsafe {
            RegSetValueExW(
                h,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            )
        };
        unsafe { RegCloseKey(h) };
        if rc != ERROR_SUCCESS {
            return Err(format!("writing the sign-in entry failed (error {})", rc));
        }
        Ok(())
    }

    pub fn delete() -> Result<(), String> {
        let h = open_run_key()?;
        let name = wide(VALUE_NAME);
        let rc = unsafe { RegDeleteValueW(h, name.as_ptr()) };
        unsafe { RegCloseKey(h) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(format!("removing the sign-in entry failed (error {})", rc));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn read() -> Option<String> {
        None
    }
    pub fn write(_data: &str) -> Result<(), String> {
        Err("start at sign-in is only available on Windows".to_string())
    }
    pub fn delete() -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_the_path_and_adds_the_flag() {
        let exe = std::path::Path::new(r"C:\Program Files\Stream To Speaker\stream-to-speaker.exe");
        assert_eq!(
            command_line(exe),
            r#""C:\Program Files\Stream To Speaker\stream-to-speaker.exe" --startup"#
        );
    }
}
