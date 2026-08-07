//! Embeds the application icon into the executable on Windows.
//!
//! Without this the exe carries no icon resource, so Explorer, the Start
//! Menu and any shortcut fall back to the generic default. The window and
//! tray icons are set at runtime and are a separate concern — this one is
//! what the file itself looks like on disk.
fn main() {
    #[cfg(windows)]
    {
        const ICON: &str = "../assets/StreamToSpeaker.ico";
        println!("cargo:rerun-if-changed={ICON}");

        // Fail loudly. A resource compiler comes with the Windows SDK,
        // which is already required to build this project, so if it is
        // missing the build environment is broken — and the failure mode
        // if we merely warned is a release binary that silently ships
        // with no icon.
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ICON);
        res.compile().unwrap_or_else(|e| {
            panic!(
                "failed to embed {ICON} into the executable: {e}\n\
                 A resource compiler (rc.exe from the Windows SDK, or windres) \
                 must be on PATH. Building without it would produce a binary \
                 with no icon in Explorer, the Start Menu or shortcuts."
            )
        });
    }
}
