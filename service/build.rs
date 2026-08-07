//! Embeds the application icon into the executable on Windows.
//!
//! Without this the exe carries no icon resource, so Explorer, the Start
//! Menu and any shortcut fall back to the generic default. The window and
//! tray icons are set at runtime and are a separate concern — this one is
//! what the file itself looks like on disk.
fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../assets/StreamToSpeaker.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/StreamToSpeaker.ico");
        if let Err(e) = res.compile() {
            // Never fail the build over decoration: a resource compiler is
            // not present on every dev machine, and an icon-less binary
            // still runs correctly.
            println!("cargo:warning=icon resource not embedded: {e}");
        }
    }
}
