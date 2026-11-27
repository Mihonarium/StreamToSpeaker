//! Permissions, exit codes and time limits for elevated Repair.

use std::path::Path;

/// Check each file beside the app or its driver tools, including files added after setup.
pub(super) fn check_install_files(
    app: &Path,
    mut check: impl FnMut(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for folder in [app, &app.join("driver")] {
        for entry in std::fs::read_dir(folder)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                check(&entry.path())?;
            }
        }
    }
    Ok(())
}

/// A binary SID starts with its revision, subauthority count and six-byte authority.
/// A callback ACE can carry more data after its SID.
pub(super) fn trusted_sid(bytes: &[u8]) -> bool {
    let Some((&[1, count, 0, 0, 0, 0, 0, 5], tail)) = bytes.split_at_checked(8) else {
        return false;
    };
    let expected: &[u32] = match count {
        1 => &[18],      // Local System
        2 => &[32, 544], // Builtin Administrators
        6 => &[
            80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464,
        ], // TrustedInstaller
        _ => return false,
    };
    tail.len() >= expected.len() * 4
        && expected
            .iter()
            .zip(tail.chunks_exact(4))
            .all(|(id, bytes)| id.to_le_bytes() == bytes)
}

pub(super) fn protected_ace(ace: &[u8]) -> bool {
    let [kind, flags, _, _, a, b, c, d, tail @ ..] = ace else {
        return false;
    };
    // An inherit-only ACE applies to descendants. Each path is checked in turn.
    if flags & 0x08 != 0 {
        return true;
    }
    let mask = u32::from_le_bytes([*a, *b, *c, *d]);
    // Write data, append, write extended attributes, delete child, write attributes,
    // delete, write DACL, write owner, generic all and generic write.
    const CHANGE: u32 =
        0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000 | 0x10000000 | 0x40000000;
    if mask & CHANGE == 0 {
        return true;
    }
    match kind {
        0 | 9 => trusted_sid(tail), // Allow and callback allow
        5 | 11 => {
            // Object allow and callback object allow
            let Some((&[a, b, c, d], fields)) = tail.split_at_checked(4) else {
                return false;
            };
            let flags = u32::from_le_bytes([a, b, c, d]);
            // Each of the two presence bits puts one GUID before the SID.
            flags & !3 == 0
                && fields
                    .get(16 * flags.count_ones() as usize..)
                    .is_some_and(trusted_sid)
        }
        1 | 6 | 10 | 12 => true, // Deny ACEs grant no rights.
        _ => false,
    }
}

#[derive(Clone, Copy)]
pub(super) enum DriverTool {
    PnpUtil,
    DevCon,
}

pub(super) fn find_active_endpoint(
    ids: impl Iterator<Item = anyhow::Result<String>>,
    parent: impl Fn(&str) -> anyhow::Result<Option<u32>>,
    accept: impl Fn(u32) -> bool,
) -> anyhow::Result<Option<String>> {
    let mut first_error = None;
    for id in ids {
        let reading = id.and_then(|id| Ok(parent(&id)?.is_some_and(&accept).then_some(id)));
        match reading {
            Ok(Some(id)) => return Ok(Some(id)),
            Ok(None) => {}
            Err(error) => { first_error.get_or_insert(error); }
        }
    }
    first_error.map_or(Ok(None), Err)
}

/// An enabled device needs a restart if Windows reports a problem or its output is absent.
pub(super) fn needs_restart(
    problem: u32,
    has_active_endpoint: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    Ok(problem != 0 || !has_active_endpoint()?)
}

impl DriverTool {
    pub(super) fn accepts(self, code: Option<i32>) -> bool {
        match self {
            // /install can report 259 after staging when no matching device needs this driver.
            Self::PnpUtil => matches!(code, Some(0 | 259 | 3010)),
            Self::DevCon => matches!(code, Some(0 | 1)),
        }
    }
}

/// Reap the tool on both completion and timeout. Its output goes straight to the run log.
pub(super) fn wait_for_tool(
    child: &mut std::process::Child,
    limit: std::time::Duration,
) -> std::io::Result<std::process::ExitStatus> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            child.kill()?;
            child.wait()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Tool exceeded {limit:?}"),
            ));
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(ids: &[u32]) -> Vec<u8> {
        let mut sid = vec![1, ids.len() as u8, 0, 0, 0, 0, 0, 5];
        sid.extend(ids.iter().flat_map(|n| n.to_le_bytes()));
        sid
    }

    #[test]
    fn every_direct_install_and_driver_file_must_pass_the_permission_check() {
        use std::fs;
        let app = std::env::temp_dir().join(format!("sts-repair-files-{:x}", rand::random::<u64>()));
        fs::create_dir_all(app.join("driver")).unwrap();
        fs::create_dir(app.join("logs")).unwrap();
        let required = ["app.exe", "extra.dll", "driver/devcon.exe", "driver/sound.inf",
            "driver/sound.sys", "driver/sound.cat", "driver/extra.dll"];
        for file in required.into_iter().chain(["logs/run.log"]) {
            fs::write(app.join(file), []).unwrap();
        }
        let mut checked = Vec::new();
        check_install_files(&app, |file| { checked.push(file.to_owned()); Ok(()) }).unwrap();
        checked.sort();
        let mut expected = required.map(|file| app.join(file));
        expected.sort();
        assert_eq!(checked, expected);
        for denied in expected {
            assert!(check_install_files(&app, |file| {
                anyhow::ensure!(file != denied, "unprotected file");
                Ok(())
            }).is_err());
        }
        fs::remove_dir_all(app).unwrap();
    }

    #[test]
    fn only_the_three_machine_trustees_can_own_or_modify_the_install() {
        let system = sid(&[18]);
        let admins = sid(&[32, 544]);
        let installer = sid(&[
            80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464,
        ]);
        let users = sid(&[32, 545]);
        for (owner, trusted) in [
            (&system, true),
            (&admins, true),
            (&installer, true),
            (&users, false),
        ] {
            assert_eq!(trusted_sid(owner), trusted);
            for mask in [
                2u32, 4, 16, 64, 256, 0x10000, 0x40000, 0x80000, 0x10000000, 0x40000000, 0x1200a9,
            ] {
                for kind in [0, 5, 9, 11] {
                    let mut ace = vec![kind, 0x10, 0, 0];
                    ace.extend(mask.to_le_bytes());
                    if matches!(kind, 5 | 11) {
                        ace.extend(3u32.to_le_bytes());
                        ace.extend([0; 32]);
                    }
                    ace.extend(owner);
                    assert_eq!(protected_ace(&ace), trusted || mask == 0x1200a9);
                    ace[1] |= 8;
                    assert!(protected_ace(&ace));
                    ace[1] = 0;
                    ace[0] = 1;
                    assert!(protected_ace(&ace));
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_blocked_on_its_input_is_terminated_at_its_deadline() {
        use std::process::{Command, Stdio};
        use std::time::Duration;
        let mut child = Command::new("cat").stdin(Stdio::piped()).spawn().unwrap();
        let error = wait_for_tool(&mut child, Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(!child.try_wait().unwrap().unwrap().success());
        let mut finished = Command::new("true").spawn().unwrap();
        assert!(wait_for_tool(&mut finished, Duration::from_secs(5))
            .unwrap()
            .success());
    }

    #[test]
    fn tool_exit_codes_depend_on_the_command() {
        for (code, pnp, devcon) in [
            (0, true, true),
            (1, false, true),
            (2, false, false),
            (259, true, false),
            (3010, true, false),
            (1641, false, false),
            (-1, false, false),
        ] {
            assert_eq!(DriverTool::PnpUtil.accepts(Some(code)), pnp);
            assert_eq!(DriverTool::DevCon.accepts(Some(code)), devcon);
        }
        assert!(!DriverTool::PnpUtil.accepts(None));
    }

    #[test]
    fn an_unreadable_endpoint_id_or_parent_prevents_a_restart() {
        for bad_id in [true, false] {
            for found in [false, true] {
                let first = if bad_id { Err(anyhow::anyhow!("read ID")) } else { Ok("unknown".into()) };
                let ids = [first, Ok("other".into())].into_iter().chain(found.then(|| Ok("ours".into())));
                let endpoint = find_active_endpoint(ids, |id| match id {
                    "unknown" => anyhow::bail!("read parent"),
                    "ours" => Ok(Some(42)),
                    _ => Ok(None),
                }, |node| node == 42);
                let restart = needs_restart(0, || Ok(endpoint?.is_some()));
                if found {
                    assert!(!restart.unwrap());
                } else {
                    assert_eq!(restart.unwrap_err().to_string(), if bad_id { "read ID" } else { "read parent" });
                }
            }
        }
    }

    #[test]
    fn an_enabled_device_with_no_endpoint_needs_a_restart_even_at_problem_code_zero() {
        assert!(needs_restart(0, || Ok(false)).unwrap());
        assert!(!needs_restart(0, || Ok(true)).unwrap());
        assert!(needs_restart(10, || panic!("device already reports a problem")).unwrap());
        assert!(needs_restart(0, || anyhow::bail!("endpoint read failed")).is_err());
    }
}
