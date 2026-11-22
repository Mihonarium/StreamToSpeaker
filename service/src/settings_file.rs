//! How `config.json` is written and read back so that no moment of a save
//! can leave the settings lost: what a power cut or a crash at any step
//! leaves on the disk still loads as the settings before the save or the
//! ones after it.
//!
//! A save:
//! 1. writes the new text to a temporary file with a name of its own
//!    (`config.json.<process>-<n>.tmp`: the app and `--web-link` may save
//!    at the same moment) and flushes it to the disk;
//! 2. renames it over `config.json`;
//! 3. writes the same text to `config.json.bak` the same way: a temporary
//!    file of its own, flushed, then renamed.
//!
//! `config.json` is complete and on the disk at every moment, because a
//! rename only takes the place of a file whose data is already flushed;
//! so is the backup. Before step 2 both hold the settings before the save,
//! between steps 2 and 3 `config.json` holds the new ones and the backup
//! the old, afterwards both hold the new. The backup is a mirror of what
//! this app last saved, not a history: it never holds raw bytes from
//! elsewhere (a broken file, secrets an older version left unsealed), and
//! a secret removed from the settings leaves the backup with the same
//! save. A loader that finds `config.json` missing, empty, zero-filled or
//! not a JSON object reads the backup instead (see `user_config`).
//!
//! The file operations go through [`FileOps`] so the tests can cut the
//! power after each one.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// The file operations a save and a load use.
pub trait FileOps {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Create `path`, which must not exist, holding `bytes`, flushed to
    /// the disk before this returns. Nothing is left at `path` on failure.
    fn create_flushed(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Rename `from` to `to`, replacing `to`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// The names of the files in `dir`.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// How long ago `path` was last written; `None` when unknown.
    fn age(&self, path: &Path) -> Option<Duration>;
}

/// The real disk.
pub struct Disk;

impl FileOps for Disk {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn create_flushed(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        // `sync_all` is FlushFileBuffers on Windows: the data is on the
        // disk, not only in the cache, before any rename can make this
        // file the settings.
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        if written.is_err() {
            drop(file);
            let _ = std::fs::remove_file(path);
        }
        written
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        Ok(std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect())
    }

    fn age(&self, path: &Path) -> Option<Duration> {
        std::fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()
    }
}

/// `config.json` → `config.json.bak` beside it.
pub fn backup_path(path: &Path) -> PathBuf {
    sibling(path, "bak")
}

/// `<name>.<suffix>` beside `path`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "config.json".into());
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Numbers the temporary files of this process.
static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Most names tried for one temporary file before giving up.
const TEMP_TRIES: u32 = 64;

/// Write `bytes` to a new temporary file beside `path`, flushed; its
/// name.
fn write_temp(fs: &dyn FileOps, path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let pid = std::process::id();
    let mut last = None;
    for _ in 0..TEMP_TRIES {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = sibling(path, &format!("{pid}-{n}.tmp"));
        match fs.create_flushed(&tmp, bytes) {
            Ok(()) => return Ok(tmp),
            // Left by a process that had this number before and stopped
            // mid-save: take the next name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free name")))
}

/// Rename `from` over `to`.
fn rename_patiently(fs: &dyn FileOps, from: &Path, to: &Path) -> io::Result<()> {
    fs.rename(from, to)
}

/// Why a save didn't replace the file.
#[derive(Debug)]
pub enum WriteError {
    /// The folder couldn't be made.
    Folder(io::Error),
    /// The new text couldn't be written beside the file.
    Write(io::Error),
    /// The new text couldn't take the file's place.
    Replace(io::Error),
}

/// Replace `path` with `bytes`, then the backup with the same, by the
/// steps in the module docs. The save has succeeded once `path` is
/// replaced: a backup that can't be written is logged (once each time
/// that changes) and doesn't make it fail. Nothing temporary is left
/// behind, whatever happens.
pub fn replace(fs: &dyn FileOps, path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs.create_dir_all(dir).map_err(WriteError::Folder)?;
    }
    let tmp = write_temp(fs, path, bytes).map_err(WriteError::Write)?;
    fs.rename(&tmp, path).map_err(|e| {
        let _ = fs.remove(&tmp);
        WriteError::Replace(e)
    })?;
    let backup = backup_path(path);
    let mirrored = write_temp(fs, &backup, bytes).and_then(|bak_tmp| {
        fs.rename(&bak_tmp, &backup).inspect_err(|_| {
            let _ = fs.remove(&bak_tmp);
        })
    });
    note_backup(&backup, mirrored);
    Ok(())
}

/// The last backup failure logged, so a failure that repeats at every
/// save is logged once (and its end once).
static BACKUP_FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn note_backup(backup: &Path, result: io::Result<()>) {
    let Ok(mut last) = BACKUP_FAILURE.lock() else { return };
    match result {
        Ok(()) => {
            if last.take().is_some() {
                log::info!("user_config: the backup {} is written again", backup.display());
            }
        }
        Err(e) => {
            let why = e.to_string();
            if last.as_deref() != Some(why.as_str()) {
                log::warn!(
                    "user_config: the settings are saved, but their backup {} couldn't be \
                     written ({why})",
                    backup.display()
                );
                *last = Some(why);
            }
        }
    }
}

/// Unix seconds now (0 if the clock is before 1970).
fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A name beside `path` that no file has: `<name>.<label>-<secs>`, or with
/// `-<n>` added when that one is taken (two in the same second).
pub fn fresh_name(fs: &dyn FileOps, path: &Path, label: &str, secs: u64) -> io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new(""));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config.json");
    let taken = fs.list(dir).unwrap_or_default();
    (0..TEMP_TRIES)
        .map(|n| if n == 0 { format!("{name}.{label}-{secs}") } else { format!("{name}.{label}-{secs}-{n}") })
        .find(|candidate| !taken.contains(candidate))
        .map(|candidate| dir.join(candidate))
        .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free name"))
}

/// Move `path` aside to a [`fresh_name`] labelled `label`; its new path.
pub fn move_aside(fs: &dyn FileOps, path: &Path, label: &str) -> io::Result<PathBuf> {
    let aside = fresh_name(fs, path, label, now_secs())?;
    fs.rename(path, &aside)?;
    Ok(aside)
}

/// Keep `bytes` as a copy of `path` under a [`fresh_name`] labelled
/// `label`, written the way a save writes (a temporary file, flushed, then
/// renamed), so the name only ever holds a complete copy. A copy for which
/// `same` holds is already kept: its path is returned and nothing is
/// written. Beyond `most` copies, the oldest others are removed.
pub fn keep_copy(
    fs: &dyn FileOps,
    path: &Path,
    label: &str,
    bytes: &[u8],
    most: usize,
    same: &dyn Fn(&[u8]) -> bool,
) -> io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new(""));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("config.json");
    let prefix = format!("{name}.{label}-");
    let mut kept: Vec<String> = fs
        .list(dir)?
        .into_iter()
        .filter(|n| n.starts_with(prefix.as_str()) && !n.ends_with(".tmp"))
        .collect();
    for copy in &kept {
        if fs.read(&dir.join(copy)).is_ok_and(|b| same(&b)) {
            return Ok(dir.join(copy));
        }
    }
    let copy = fresh_name(fs, path, label, now_secs())?;
    let tmp = write_temp(fs, &copy, bytes)?;
    if let Err(e) = rename_patiently(fs, &tmp, &copy) {
        let _ = fs.remove(&tmp);
        return Err(e);
    }
    // Oldest first: by the time in the name, then by the number after it.
    let when = |n: &str| -> (u64, u64) {
        let mut parts = n.get(prefix.len()..).unwrap_or("").split('-').map(|t| t.parse().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
    };
    kept.sort_by_key(|n| when(n));
    let excess = (kept.len() + 1).saturating_sub(most);
    for old in kept.iter().take(excess) {
        if let Err(e) = fs.remove(&dir.join(old)) {
            log::debug!("user_config: couldn't remove the older copy {old} ({e})");
        }
    }
    Ok(copy)
}

/// A temporary file older than this is left from a save that was cut
/// short (a crash, a power cut): no save takes this long.
const STALE_TEMP: Duration = Duration::from_secs(60 * 60);

/// Remove temporary files of `path`'s saves that were left behind by a
/// save cut short, earlier versions' `config.json.tmp` included. Best
/// effort.
pub fn remove_stale_temps(fs: &dyn FileOps, path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(names) = fs.list(dir) else { return };
    let prefixes = [format!("{name}."), format!("{name}.bak.")];
    for file in names {
        let ours = file.ends_with(".tmp") && prefixes.iter().any(|p| file.starts_with(p.as_str()));
        let stale = dir.join(&file);
        if ours && fs.age(&stale).is_some_and(|age| age >= STALE_TEMP) {
            if let Err(e) = fs.remove(&stale) {
                log::debug!("user_config: couldn't remove {} ({e})", stale.display());
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// One file as the disk holds it: what was written, and whether it was
    /// flushed. After a power cut, a file that wasn't flushed has its
    /// length but zeros for its data (what NTFS shows for data the cache
    /// hadn't written yet).
    #[derive(Clone, Debug)]
    struct Node {
        bytes: Vec<u8>,
        flushed: bool,
    }

    /// A disk in memory whose metadata operations (create, rename, remove)
    /// are journaled in order, as NTFS does: the power can be cut after
    /// any number of operations, and what is left is the state after
    /// exactly those, with unflushed data lost.
    #[derive(Default)]
    pub(crate) struct PowerCutDisk {
        files: RefCell<BTreeMap<PathBuf, Node>>,
        /// Operations allowed before the power goes; `None` = no cut.
        budget: RefCell<Option<usize>>,
        /// Operations done (or refused for the cut).
        ops: RefCell<usize>,
        /// Writes that leave data in the cache only (an `fs::write`).
        pub(crate) cached_writes: bool,
        /// The one operation (counted from 0) that fails; the others work.
        fail_only: RefCell<Option<usize>>,
    }

    impl PowerCutDisk {
        pub(crate) fn with_file(path: &Path, bytes: &[u8]) -> Self {
            let d = Self::default();
            d.files.borrow_mut().insert(path.to_path_buf(), Node { bytes: bytes.to_vec(), flushed: true });
            d
        }

        /// Cut the power after `n` more operations.
        pub(crate) fn cut_after(&self, n: usize) {
            *self.budget.borrow_mut() = Some(n);
            *self.ops.borrow_mut() = 0;
        }

        pub(crate) fn ops(&self) -> usize {
            *self.ops.borrow()
        }

        /// What the disk holds after the power came back.
        pub(crate) fn after_power_cut(&self) -> Self {
            let files = self
                .files
                .borrow()
                .iter()
                .map(|(p, n)| {
                    let bytes = if n.flushed { n.bytes.clone() } else { vec![0; n.bytes.len()] };
                    (p.clone(), Node { bytes, flushed: true })
                })
                .collect();
            Self { files: RefCell::new(files), ..Self::default() }
        }

        pub(crate) fn names(&self) -> Vec<String> {
            self.files
                .borrow()
                .keys()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect()
        }

        pub(crate) fn get(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.borrow().get(path).map(|n| n.bytes.clone())
        }

        /// Count an operation; an error once the power is gone.
        fn step(&self) -> io::Result<()> {
            let mut ops = self.ops.borrow_mut();
            if let Some(budget) = *self.budget.borrow() {
                if *ops >= budget {
                    return Err(io::Error::new(io::ErrorKind::Other, "power cut"));
                }
            }
            *ops += 1;
            if *self.fail_only.borrow() == Some(*ops - 1) {
                return Err(io::Error::new(io::ErrorKind::Other, "failed"));
            }
            Ok(())
        }
    }

    impl FileOps for PowerCutDisk {
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.get(path).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn create_flushed(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if self.files.borrow().contains_key(path) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            self.step()?;
            let flushed = !self.cached_writes;
            self.files.borrow_mut().insert(path.to_path_buf(), Node { bytes: bytes.to_vec(), flushed });
            Ok(())
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.step()?;
            let mut files = self.files.borrow_mut();
            let node = files.remove(from).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            files.insert(to.to_path_buf(), node);
            Ok(())
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.step()?;
            self.files.borrow_mut().remove(path).map(|_| ()).ok_or_else(|| io::ErrorKind::NotFound.into())
        }

        fn create_dir_all(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }

        fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
            Ok(self
                .files
                .borrow()
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect())
        }

        fn age(&self, _: &Path) -> Option<Duration> {
            Some(STALE_TEMP * 2)
        }
    }

    fn path() -> PathBuf {
        PathBuf::from("/profile/StreamToSpeaker/config.json")
    }

    #[test]
    fn a_save_leaves_the_file_its_mirror_and_nothing_else() {
        let disk = PowerCutDisk::with_file(&path(), b"{\"old\":1}");
        replace(&disk, &path(), b"{\"new\":2}").unwrap();
        assert_eq!(disk.get(&path()).unwrap(), b"{\"new\":2}");
        assert_eq!(disk.get(&backup_path(&path())).unwrap(), b"{\"new\":2}");
        assert_eq!(disk.names(), ["config.json", "config.json.bak"]);
    }

    #[test]
    fn a_failed_save_leaves_nothing_temporary() {
        // A full save is four operations; make each one fail in turn (the
        // others work). The file is the old or the new text, the backup
        // the old or the new, no temporary file is left, and the save has
        // failed exactly when the file is still the old text.
        let full = {
            let disk = PowerCutDisk::with_file(&path(), b"{\"old\":1}");
            replace(&disk, &path(), b"{\"new\":2}").unwrap();
            disk.ops()
        };
        assert_eq!(full, 4);
        for n in 0..full {
            let disk = PowerCutDisk::with_file(&path(), b"{\"old\":1}");
            disk.create_flushed(&backup_path(&path()), b"{\"old\":1}").unwrap();
            disk.cut_after(usize::MAX);
            *disk.fail_only.borrow_mut() = Some(n);
            let result = replace(&disk, &path(), b"{\"new\":2}");
            let names = disk.names();
            assert!(names.iter().all(|f| !f.ends_with(".tmp")), "op {n}: {names:?}");
            let file = disk.get(&path()).unwrap();
            assert!(file == b"{\"old\":1}" || file == b"{\"new\":2}", "op {n}");
            assert_eq!(result.is_ok(), file == b"{\"new\":2}", "op {n}: {result:?}");
            let bak = disk.get(&backup_path(&path())).unwrap();
            assert!(bak == b"{\"old\":1}" || bak == b"{\"new\":2}", "op {n}");
        }
        // A rename that fails takes its temporary file with it.
        struct NoRename<'a>(&'a PowerCutDisk);
        impl FileOps for NoRename<'_> {
            fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
                self.0.read(p)
            }
            fn create_flushed(&self, p: &Path, b: &[u8]) -> io::Result<()> {
                self.0.create_flushed(p, b)
            }
            fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
            fn remove(&self, p: &Path) -> io::Result<()> {
                self.0.remove(p)
            }
            fn create_dir_all(&self, d: &Path) -> io::Result<()> {
                self.0.create_dir_all(d)
            }
            fn list(&self, d: &Path) -> io::Result<Vec<String>> {
                self.0.list(d)
            }
            fn age(&self, p: &Path) -> Option<Duration> {
                self.0.age(p)
            }
        }
        let disk = PowerCutDisk::with_file(&path(), b"{\"old\":1}");
        let err = replace(&NoRename(&disk), &path(), b"{\"new\":2}").unwrap_err();
        assert!(matches!(err, WriteError::Replace(_)), "{err:?}");
        assert_eq!(disk.names(), ["config.json"]);
        assert_eq!(disk.get(&path()).unwrap(), b"{\"old\":1}");
    }

    #[test]
    fn two_processes_saving_at_once_never_share_a_temporary_file() {
        // A file another save is writing has the next name: this save takes
        // one after it instead of writing into it.
        let disk = PowerCutDisk::with_file(&path(), b"{}");
        let next = TEMP_COUNTER.load(Ordering::Relaxed);
        let theirs = sibling(&path(), &format!("{}-{next}.tmp", std::process::id()));
        disk.create_flushed(&theirs, b"theirs").unwrap();
        replace(&disk, &path(), b"{\"mine\":1}").unwrap();
        assert_eq!(disk.get(&theirs).unwrap(), b"theirs");
        assert_eq!(disk.get(&path()).unwrap(), b"{\"mine\":1}");
    }

    #[test]
    fn leftovers_of_saves_cut_short_are_removed() {
        let disk = PowerCutDisk::with_file(&path(), b"{}");
        for name in ["config.json.tmp", "config.json.77-3.tmp", "config.json.bak.77-4.tmp", "other.tmp", "config.json.bak"] {
            disk.create_flushed(&path().with_file_name(name), b"x").unwrap();
        }
        remove_stale_temps(&disk, &path());
        assert_eq!(disk.names(), ["config.json", "config.json.bak", "other.tmp"]);
    }

    #[test]
    fn copies_kept_aside_are_not_made_twice_and_at_most_three_stay() {
        let disk = PowerCutDisk::with_file(&path(), b"{}");
        let never = |_: &[u8]| false;
        let mut made = Vec::new();
        for n in 0..5 {
            let copy = keep_copy(&disk, &path(), "before-reset", format!("copy {n}").as_bytes(), 3, &never).unwrap();
            assert!(copy.file_name().unwrap().to_str().unwrap().starts_with("config.json.before-reset-"));
            made.push(copy);
        }
        // The newest three stay, made in this order within one second or not.
        let mut left: Vec<Vec<u8>> = disk
            .names()
            .iter()
            .filter(|n| n.starts_with("config.json.before-reset-"))
            .map(|n| disk.get(&path().with_file_name(n)).unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [b"copy 2".to_vec(), b"copy 3".to_vec(), b"copy 4".to_vec()]);
        // One the same as a kept copy is not made again.
        let same = |b: &[u8]| b == b"copy 3";
        let again = keep_copy(&disk, &path(), "before-reset", b"copy 3 again", 3, &same).unwrap();
        assert_eq!(again, made[3]);
        assert_eq!(disk.names().len(), 4, "{:?}", disk.names());
    }

    #[test]
    fn a_copy_kept_aside_is_complete_under_its_name_or_not_there() {
        let never = |_: &[u8]| false;
        for n in 0..=2 {
            let disk = PowerCutDisk::with_file(&path(), b"{}");
            disk.cut_after(n);
            let _ = keep_copy(&disk, &path(), "before-reset", b"the whole copy", 3, &never);
            let after = disk.after_power_cut();
            for name in after.names().iter().filter(|f| f.starts_with("config.json.before-reset-") && !f.ends_with(".tmp")) {
                assert_eq!(after.get(&path().with_file_name(name)).unwrap(), b"the whole copy", "cut after {n}");
            }
        }
    }

    #[test]
    fn real_saves_flush_and_leave_no_temporary_file() {
        let dir = std::env::temp_dir().join(format!("sts-settings-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        replace(&Disk, &path, b"{\"a\":1}").unwrap();
        replace(&Disk, &path, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":2}");
        assert_eq!(std::fs::read(backup_path(&path)).unwrap(), b"{\"a\":2}");
        let mut names: Vec<_> = Disk.list(&dir).unwrap();
        names.sort();
        assert_eq!(names, ["config.json", "config.json.bak"]);
        // A file in the way of the temporary name is never written into.
        let n = TEMP_COUNTER.load(Ordering::Relaxed);
        let blocker = sibling(&path, &format!("{}-{n}.tmp", std::process::id()));
        std::fs::write(&blocker, b"keep").unwrap();
        replace(&Disk, &path, b"{\"a\":3}").unwrap();
        assert_eq!(std::fs::read(&blocker).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
