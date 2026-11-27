//! The machine's installed app and the files Repair can use.

use anyhow::{ensure, Context, Result};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
};

const WINDOWS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";
const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{8A6C7F92-3E1B-4A2B-9F1C-77F1B6B2A6E0}_is1";
pub(super) const EXE: &str = "stream-to-speaker.exe";

// ShellExecute and driver tools take ordinary Win32 paths. Check that stripping
// the extended prefix still opens exactly the object returned by canonicalize.
fn resolved(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let mut name: Vec<_> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: name is terminated and its capacity in UTF-16 units includes that terminator.
    let hr =
        unsafe { windows_sys::Win32::UI::Shell::PathCchStripPrefix(name.as_mut_ptr(), name.len()) };
    ensure!(
        hr >= 0,
        "Cannot convert the installed path (HRESULT {hr:#x})"
    );
    let end = name
        .iter()
        .position(|c| *c == 0)
        .context("Unterminated Windows path")?;
    let dos = PathBuf::from(OsString::from_wide(&name[..end]));
    ensure!(
        dos.is_absolute() && dos.canonicalize()? == canonical,
        "Ambiguous Windows install path"
    );
    Ok(dos)
}

fn machine_path(key: &str, value: &str) -> Result<PathBuf> {
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut buf = vec![0u16; 32_768];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: both names are terminated, and bytes describes the writable UTF-16 buffer.
    let code = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(key).as_ptr(),
            wide(value).as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    ensure!(code == 0, "Cannot read HKLM {value} (Windows error {code})");
    let chars = &buf[..bytes as usize / 2];
    let path = PathBuf::from(OsString::from_wide(
        chars
            .strip_suffix(&[0])
            .context("Unterminated install path")?,
    ));
    ensure!(path.is_absolute(), "Expected an absolute machine path");
    resolved(&path)
}

/// Read the owner and discretionary ACL from the same security descriptor.
pub(super) fn check_permissions(path: &Path) -> Result<()> {
    use crate::platform::repair_policy::{protected_ace, trusted_sid};
    use std::ptr::null_mut;
    use std::slice::from_raw_parts;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{GetAce, GetLengthSid, IsValidAcl, IsValidSid, ACE_HEADER,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};

    let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut descriptor, mut owner, mut acl) = (null_mut(), null_mut(), null_mut());
    // SAFETY: name is terminated. Windows allocates the descriptor and returns its
    // owner and ACL as pointers into that allocation. They are used before LocalFree.
    let code = unsafe { GetNamedSecurityInfoW(name.as_ptr(), SE_FILE_OBJECT,
        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION, &mut owner, null_mut(),
        &mut acl, null_mut(), &mut descriptor) };
    ensure!(code == 0, "Cannot read permissions on {} (Windows error {code})", path.display());
    // SAFETY: the validated SID and ACL describe their own lengths. GetAce returns
    // an ACE in this ACL, with its full size in the header. No slice escapes the call.
    let protected = unsafe {
        let protected = !owner.is_null() && IsValidSid(owner) != 0
            && trusted_sid(from_raw_parts(owner.cast(), GetLengthSid(owner) as usize))
            && !acl.is_null() && IsValidAcl(acl) != 0 && (0..u32::from((*acl).AceCount)).all(|i| {
                let mut ace = null_mut();
                GetAce(acl, i, &mut ace) != 0 && protected_ace(from_raw_parts(
                    ace.cast(), (*ace.cast::<ACE_HEADER>()).AceSize as usize))
            });
        LocalFree(descriptor);
        protected
    };
    ensure!(protected, "Unprotected permissions on {}", path.display());
    Ok(())
}

pub(super) fn protected_app() -> Result<PathBuf> {
    let app = machine_path(UNINSTALL, "InstallLocation")?;
    let root = ["ProgramFilesDir", "ProgramFilesDir (x86)"]
        .iter()
        .filter_map(|value| machine_path(WINDOWS, value).ok())
        .find(|root| app != *root && app.starts_with(root))
        .context("InstallLocation is outside Program Files")?;
    let mut folder = root.clone();
    check_permissions(&folder)?;
    for part in app.strip_prefix(&root)?.components() {
        folder.push(part);
        check_permissions(&folder)?;
    }
    ensure!(
        resolved(&std::env::current_exe()?)? == app.join(EXE),
        "This executable is outside the registered install"
    );
    let driver = installed_path(&app, "driver")?;
    check_permissions(&driver)?;
    for path in [app.join(EXE), driver.join("devcon.exe"), driver.join("StreamToSpeaker.inf")] {
        ensure!(path.is_file(), "Missing installed file {}", path.display());
    }
    crate::platform::repair_policy::check_install_files(&app, |path| {
        ensure!(resolved(path)? == path, "Unexpected target for {}", path.display());
        check_permissions(path)
    })?;
    Ok(app)
}

pub(super) fn installed_path(app: &Path, relative: &str) -> Result<PathBuf> {
    let path = app.join(relative);
    ensure!(resolved(&path)? == path, "Unexpected target for {relative}");
    Ok(path)
}
