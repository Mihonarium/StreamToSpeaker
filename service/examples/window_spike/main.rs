//! A trial, not part of the app: can eframe 0.29 with the glow renderer
//! drive the windows the app needs (a borderless panel by the
//! clock that closes when it loses the focus, a confirmation that takes no
//! focus, Settings and a dialog) while the app's own main window stays
//! hidden? It prints what happens, then how many frames each window drew
//! per second.
//!
//!     cargo run --example window_spike -- [--root hidden|visible] [--seconds 14]
//!
//! The app's window library is built for Windows only, so on other systems
//! this example only says so.

#[cfg(windows)]
mod spike;

#[cfg(windows)]
fn main() -> eframe::Result {
    spike::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("window_spike: this trial runs on Windows, where the app's window library is built");
}
