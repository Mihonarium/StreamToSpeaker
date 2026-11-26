//! Windows' screen reader setting and the built-in Narrator.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::BOOL;
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETSCREENREADER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

#[derive(Default)]
pub(super) struct WindowsDesk(Mutex<Option<(Instant, bool)>>);

impl crate::platform::Desk for WindowsDesk {
    fn screen_reader(&self) -> bool {
        let mut cached = self.0.lock().unwrap();
        if let Some((at, running)) = *cached {
            if at.elapsed() < Duration::from_secs(1) {
                return running;
            }
        }
        let mut enabled = BOOL::default();
        // SAFETY: SPI_GETSCREENREADER writes one BOOL to this local variable.
        let set = unsafe {
            SystemParametersInfoW(
                SPI_GETSCREENREADER,
                0,
                Some((&raw mut enabled).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .is_ok()
            && enabled.as_bool();
        let running = set || super::find_process(|_, file| file.eq_ignore_ascii_case("Narrator.exe"));
        *cached = Some((Instant::now(), running));
        running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Desk;

    #[test]
    fn reads_the_desktop_setting() {
        let desk = WindowsDesk::default();
        let _ = desk.screen_reader();
    }
}
