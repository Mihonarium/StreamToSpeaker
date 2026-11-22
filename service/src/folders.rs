//! The folders the app writes to, in the person's profile:
//!
//! - `%APPDATA%\StreamToSpeaker\config.json` (roaming): the settings,
//!   pairings and identities (see `user_config`). Every version reads
//!   exactly this path.
//! - `%LOCALAPPDATA%\StreamToSpeaker\` (this PC only): the log,
//!   `startup.txt`, and `window-state.ron`, what the window remembers
//!   (open sections, the zoom set with Ctrl+Plus).
//!
//! Each base folder is the environment variable when it holds an absolute
//! path (tests, and people who redirect it, rely on that), else the folder
//! Windows reports, else the variable as it is.
//!
//! Versions before this one let the window's framework keep its state in a
//! folder of its own, `%APPDATA%\Stream To Speaker\data\app.ron`;
//! [`move_old_window_state`] moves that file here and removes the folders
//! it leaves empty.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The app's folder inside each base folder.
pub const APP_FOLDER: &str = "StreamToSpeaker";

/// What the window remembers, in the local folder.
pub const WINDOW_STATE_FILE: &str = "window-state.ron";

/// The folder older versions' window framework created in the roaming
/// base folder (named after the window's title), and its file.
const OLD_WINDOW_FOLDER: &str = "Stream To Speaker";
const OLD_WINDOW_DATA: &str = "data";
const OLD_WINDOW_FILE: &str = "app.ron";

/// A base folder from its environment variable `var` and Windows' answer
/// `known` (asked only when needed): the variable when it holds an
/// absolute path; else Windows' folder; else the variable as it is (empty
/// or relative, which is what earlier versions used).
fn base_folder(var: Option<OsString>, known: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    match var {
        Some(v) if Path::new(&v).is_absolute() => Some(PathBuf::from(v)),
        other => known().filter(|p| p.is_absolute()).or_else(|| other.map(PathBuf::from)),
    }
}

/// The roaming profile folder (`%APPDATA%`).
pub fn roaming_base() -> Option<PathBuf> {
    base_folder(std::env::var_os("APPDATA"), || known_folder(KnownFolder::Roaming))
}

/// The local profile folder (`%LOCALAPPDATA%`).
pub fn local_base() -> Option<PathBuf> {
    base_folder(std::env::var_os("LOCALAPPDATA"), || known_folder(KnownFolder::Local))
}

/// The folder `config.json` lives in. Off Windows (tests, checks) with no
/// `APPDATA`: `$XDG_CONFIG_HOME/stream-to-speaker` or
/// `$HOME/.config/stream-to-speaker`.
pub fn settings_dir() -> Option<PathBuf> {
    if let Some(base) = roaming_base() {
        return Some(base.join(APP_FOLDER));
    }
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(p).join("stream-to-speaker"));
    }
    std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config").join("stream-to-speaker"))
}

/// The local folder: the log, `startup.txt`, the window's state. Not
/// created here.
pub fn local_dir() -> Option<PathBuf> {
    local_base().map(|b| b.join(APP_FOLDER))
}

/// Where the window keeps what it remembers: the local folder, or the
/// settings folder when there is no local one. The window's framework is
/// always given a file, so it never picks a folder of its own.
pub fn window_state_file() -> Option<PathBuf> {
    local_dir().or_else(settings_dir).map(|d| d.join(WINDOW_STATE_FILE))
}

/// What [`move_old_window_state`] did with the old file.
#[derive(Debug, PartialEq, Eq)]
pub enum OldWindowState {
    /// There was none (or it isn't a plain file, so it was left alone).
    None,
    /// Moved to the new place.
    Moved,
    /// Deleted: the new place already had one, which is kept.
    Deleted,
    /// Left where it was: a step failed (logged).
    Kept,
}

/// Move the window state older versions kept in
/// `<roaming>\Stream To Speaker\data\app.ron` to `new_file`, or delete it
/// when `new_file` already exists (that one is kept); then remove `data`
/// and `Stream To Speaker`, each only if it is empty and a real folder
/// (not a link or a junction). Nothing is deleted recursively and nothing
/// else is touched. Every step is best effort: a failure is logged and
/// the rest goes on.
///
/// An older version started afterwards creates the old folder again; the
/// next start of this version moves it again.
pub fn move_old_window_state(roaming: &Path, new_file: &Path) -> OldWindowState {
    let old_folder = roaming.join(OLD_WINDOW_FOLDER);
    let data = old_folder.join(OLD_WINDOW_DATA);
    // A link or junction on the way leads somewhere else: leave it all.
    if !is_real_folder_at(&old_folder) {
        return OldWindowState::None;
    }
    let mut outcome = OldWindowState::None;
    if is_real_folder_at(&data) {
        outcome = take_old_file(&data.join(OLD_WINDOW_FILE), new_file);
        remove_if_empty(&data);
    }
    remove_if_empty(&old_folder);
    outcome
}

/// The file step of [`move_old_window_state`].
fn take_old_file(old_file: &Path, new_file: &Path) -> OldWindowState {
    match std::fs::symlink_metadata(old_file) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return OldWindowState::None,
    }
    if std::fs::symlink_metadata(new_file).is_ok() {
        return match std::fs::remove_file(old_file) {
            Ok(()) => OldWindowState::Deleted,
            Err(e) => {
                log::info!("window state: couldn't delete the old copy {} ({e})", old_file.display());
                OldWindowState::Kept
            }
        };
    }
    match move_file(old_file, new_file) {
        Ok(()) => {
            log::info!("window state: moved {} to {}", old_file.display(), new_file.display());
            OldWindowState::Moved
        }
        Err(e) => {
            log::info!(
                "window state: couldn't move {} to {} ({e}); it stays where it is",
                old_file.display(),
                new_file.display()
            );
            OldWindowState::Kept
        }
    }
}

/// Rename `from` to `to` (creating `to`'s folder); across volumes (a
/// redirected roaming profile), copy and then delete the original. A
/// partial copy is removed again.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if let Err(e) = std::fs::copy(from, to) {
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    // The copy is in place; the original going too is best effort (the
    // next start deletes it, since the new place has one then).
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// Remove `folder`, a real folder, if it is empty: `remove_dir` itself
/// refuses a folder with anything in it.
fn remove_if_empty(folder: &Path) {
    match std::fs::remove_dir(folder) {
        Ok(()) => log::info!("window state: removed the empty folder {}", folder.display()),
        Err(e) => log::debug!("window state: kept {} ({e})", folder.display()),
    }
}

/// Whether `path` is a real folder: no link, junction or other reparse
/// point (the last component; callers check each one on the way).
fn is_real_folder_at(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| is_real_folder(&m))
}

fn is_real_folder(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    meta.file_type().is_dir()
}

#[derive(Clone, Copy)]
enum KnownFolder {
    Roaming,
    Local,
}

/// The folder Windows reports for `which` (`SHGetKnownFolderPath`);
/// `None` off Windows or when Windows has none.
#[cfg(windows)]
fn known_folder(which: KnownFolder) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_RoamingAppData, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
    };
    let id = match which {
        KnownFolder::Roaming => &FOLDERID_RoamingAppData,
        KnownFolder::Local => &FOLDERID_LocalAppData,
    };
    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `id` points to a GUID that outlives the call; `raw` receives
    // a string Windows allocates, freed below on every path (freeing a
    // null pointer is allowed).
    let hr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY as u32, std::ptr::null_mut(), &mut raw) };
    let path = if hr >= 0 && !raw.is_null() {
        // SAFETY: on success `raw` is a NUL-terminated wide string.
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        // SAFETY: `len` units before the terminator are readable.
        let wide = unsafe { std::slice::from_raw_parts(raw, len) };
        Some(PathBuf::from(OsString::from_wide(wide)))
    } else {
        None
    };
    // SAFETY: `raw` came from SHGetKnownFolderPath (or is null).
    unsafe { CoTaskMemFree(raw as *const _) };
    path
}

#[cfg(not(windows))]
fn known_folder(_: KnownFolder) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(p: &str) -> PathBuf {
        std::env::temp_dir().join(p)
    }

    #[test]
    fn an_absolute_variable_wins_and_windows_is_not_asked() {
        let var = abs("profile").into_os_string();
        let got = base_folder(Some(var.clone()), || panic!("asked Windows"));
        assert_eq!(got, Some(PathBuf::from(var)));
    }

    #[test]
    fn a_missing_empty_or_relative_variable_asks_windows() {
        let windows = abs("from-windows");
        for var in [None, Some(OsString::new()), Some(OsString::from("relative\\AppData"))] {
            assert_eq!(base_folder(var.clone(), || Some(windows.clone())), Some(windows.clone()), "{var:?}");
        }
    }

    #[test]
    fn without_an_answer_from_windows_the_variable_is_used_as_before() {
        assert_eq!(base_folder(None, || None), None);
        assert_eq!(base_folder(Some("".into()), || None), Some(PathBuf::new()));
        assert_eq!(base_folder(Some("rel".into()), || None), Some(PathBuf::from("rel")));
        // A relative answer is no answer.
        assert_eq!(base_folder(Some("rel".into()), || Some("also-rel".into())), Some(PathBuf::from("rel")));
    }

    /// A scratch profile: `<tmp>/<tag>/Roaming` and the new file under
    /// `<tmp>/<tag>/Local`.
    struct Profile {
        root: PathBuf,
    }

    impl Profile {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("sts-folders-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("Roaming")).unwrap();
            Self { root }
        }
        fn roaming(&self) -> PathBuf {
            self.root.join("Roaming")
        }
        fn old_folder(&self) -> PathBuf {
            self.roaming().join("Stream To Speaker")
        }
        fn old_file(&self) -> PathBuf {
            self.old_folder().join("data").join("app.ron")
        }
        fn new_file(&self) -> PathBuf {
            self.root.join("Local").join(APP_FOLDER).join(WINDOW_STATE_FILE)
        }
        fn write_old(&self, body: &str) {
            std::fs::create_dir_all(self.old_file().parent().unwrap()).unwrap();
            std::fs::write(self.old_file(), body).unwrap();
        }
        fn run(&self) -> OldWindowState {
            move_old_window_state(&self.roaming(), &self.new_file())
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_old_state_is_moved_and_its_folders_removed() {
        let p = Profile::new("moved");
        p.write_old("(zoom: 1.5)");
        assert_eq!(p.run(), OldWindowState::Moved);
        assert_eq!(std::fs::read_to_string(p.new_file()).unwrap(), "(zoom: 1.5)");
        assert!(!p.old_folder().exists(), "the old folder is gone");
        assert!(p.roaming().exists(), "the roaming folder itself stays");
        // A second start finds nothing to do.
        assert_eq!(p.run(), OldWindowState::None);
        assert_eq!(std::fs::read_to_string(p.new_file()).unwrap(), "(zoom: 1.5)");
    }

    #[test]
    fn when_both_exist_the_new_one_is_kept_and_the_old_deleted() {
        let p = Profile::new("both");
        std::fs::create_dir_all(p.new_file().parent().unwrap()).unwrap();
        std::fs::write(p.new_file(), "new").unwrap();
        p.write_old("old");
        assert_eq!(p.run(), OldWindowState::Deleted);
        assert_eq!(std::fs::read_to_string(p.new_file()).unwrap(), "new");
        assert!(!p.old_folder().exists());
    }

    #[test]
    fn a_folder_with_anything_else_in_it_is_kept() {
        let p = Profile::new("non-empty");
        p.write_old("old");
        std::fs::write(p.old_file().with_file_name("notes.txt"), "mine").unwrap();
        std::fs::write(p.old_folder().join("other.txt"), "mine too").unwrap();
        assert_eq!(p.run(), OldWindowState::Moved);
        assert_eq!(std::fs::read_to_string(p.old_folder().join("data").join("notes.txt")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(p.old_folder().join("other.txt")).unwrap(), "mine too");
        // `data` emptied but its parent holds a file: only `data` goes.
        std::fs::remove_file(p.old_folder().join("data").join("notes.txt")).unwrap();
        assert_eq!(p.run(), OldWindowState::None);
        assert!(!p.old_folder().join("data").exists());
        assert!(p.old_folder().join("other.txt").exists());
    }

    #[test]
    fn nothing_there_changes_nothing() {
        let p = Profile::new("nothing");
        assert_eq!(p.run(), OldWindowState::None);
        assert!(!p.new_file().exists());
        assert!(!p.root.join("Local").exists(), "no new folder made for nothing");
        // Empty folders an older version left behind are removed.
        std::fs::create_dir_all(p.old_folder().join("data")).unwrap();
        assert_eq!(p.run(), OldWindowState::None);
        assert!(!p.old_folder().exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_left_alone() {
        let p = Profile::new("links");
        // The old folder is a link to a folder elsewhere: the link and what
        // it points to stay.
        let elsewhere = p.root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("data")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, p.old_folder()).unwrap();
        p.run();
        assert!(std::fs::symlink_metadata(p.old_folder()).unwrap().file_type().is_symlink());
        assert!(elsewhere.join("data").exists(), "nothing removed through the link");
        // `data` a link inside a real folder: kept, and so is its parent.
        std::fs::remove_file(p.old_folder()).unwrap();
        std::fs::create_dir_all(p.old_folder()).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("data"), p.old_folder().join("data")).unwrap();
        p.run();
        assert!(std::fs::symlink_metadata(p.old_folder().join("data")).unwrap().file_type().is_symlink());
        assert!(p.old_folder().exists());
        // `app.ron` a link: not moved, not deleted.
        std::fs::remove_file(p.old_folder().join("data")).unwrap();
        std::fs::create_dir_all(p.old_folder().join("data")).unwrap();
        std::fs::write(elsewhere.join("target.ron"), "theirs").unwrap();
        std::os::unix::fs::symlink(elsewhere.join("target.ron"), p.old_file()).unwrap();
        assert_eq!(p.run(), OldWindowState::None);
        assert!(std::fs::symlink_metadata(p.old_file()).is_ok());
        assert!(!p.new_file().exists());
        assert_eq!(std::fs::read_to_string(elsewhere.join("target.ron")).unwrap(), "theirs");
    }

    #[test]
    fn a_step_that_fails_leaves_the_file_and_goes_on() {
        let p = Profile::new("fails");
        p.write_old("old");
        // The new place can't be made: a file holds its folder's name.
        std::fs::create_dir_all(p.root.join("Local")).unwrap();
        std::fs::write(p.new_file().parent().unwrap(), "in the way").unwrap();
        assert_eq!(p.run(), OldWindowState::Kept);
        assert_eq!(std::fs::read_to_string(p.old_file()).unwrap(), "old");
        assert!(p.old_folder().exists(), "kept, since it still holds the file");
        // Once the way is clear, the next start moves it.
        std::fs::remove_file(p.new_file().parent().unwrap()).unwrap();
        assert_eq!(p.run(), OldWindowState::Moved);
        assert!(!p.old_folder().exists());
    }
}
