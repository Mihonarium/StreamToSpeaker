//! Run the installed app with administrator approval and restore its render endpoint.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
    SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::{active_endpoint, install, pnp};
use crate::platform::repair_policy::{needs_restart, wait_for_tool, DriverTool};
use crate::platform::RepairExit;

fn system_directory() -> io::Result<PathBuf> {
    let mut system = [0u16; 260];
    // SAFETY: Windows writes at most the supplied number of UTF-16 units into system.
    let len = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    if len == 0 || len >= system.len() {
        return Err(io::Error::other(
            "Could not locate the Windows system directory",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&system[..len])))
}

fn launch(app: &Path, run_id: &str) -> io::Result<OwnedHandle> {
    let parameters: Vec<_> = format!("--repair-output {run_id}").encode_utf16().chain(Some(0)).collect();
    let file: Vec<_> = app.join(install::EXE).as_os_str().encode_wide().chain(Some(0)).collect();
    let directory: Vec<_> = app.as_os_str().encode_wide().chain(Some(0)).collect();

    // SAFETY: This new thread balances its COM initialization after ShellExecuteExW.
    // All input strings are NUL-terminated and remain live. Unused fields may be zero.
    unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        if hr < 0 {
            return Err(io::Error::other(format!("CoInitializeEx: {hr:#x}")));
        }
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpVerb = windows_sys::core::w!("runas");
        info.lpFile = file.as_ptr();
        info.lpParameters = parameters.as_ptr();
        info.lpDirectory = directory.as_ptr();
        info.nShow = SW_HIDE;
        let result = if ShellExecuteExW(&mut info) == 0 {
            Err(io::Error::last_os_error())
        } else if info.hProcess.is_null() {
            Err(io::Error::other("Windows returned no repair process"))
        } else {
            // SEE_MASK_NOCLOSEPROCESS transfers one process handle to the caller.
            Ok(OwnedHandle::from_raw_handle(info.hProcess))
        };
        CoUninitialize();
        result
    }
}

pub(super) fn run(approved: &mut dyn FnMut()) -> RepairExit {
    let Ok(app) = install::protected_app() else { return RepairExit::Failed };
    let run_id = format!("{:032x}", rand::random::<u128>());
    let process = match launch(&app, &run_id) {
        Ok(handle) => handle,
        Err(e) if e.raw_os_error() == Some(ERROR_CANCELLED as i32) => return RepairExit::Cancelled,
        Err(e) => {
            log::warn!("Could not launch output repair: {e}");
            return RepairExit::Failed;
        }
    };
    log::info!("Output repair log: {}", app.join("logs").join(format!("repair-output-{run_id}.log")).display());
    approved();
    let mut code = 0;
    // SAFETY: process owns the live process handle through both calls and code is writable.
    let exited = unsafe {
        WaitForSingleObject(process.as_raw_handle(), crate::platform::REPAIR_TIMEOUT_MS) == WAIT_OBJECT_0
            && GetExitCodeProcess(process.as_raw_handle(), &mut code) != 0
    };
    if exited && code == 0 {
        RepairExit::Success
    } else {
        log::warn!("Output repair failed (exited={exited}, code={code})");
        RepairExit::Failed
    }
}

/// Exit 0 means the output works. Exit 2 means the enabled device is present
/// but not working. Other failures exit 1.
pub fn repair_output(run_id: &str) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

    let deadline = Instant::now() + Duration::from_secs(100);
    let watchdog = std::thread::Builder::new().name("repair-deadline".into()).spawn(move || {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        // SAFETY: the pseudo-handle identifies this process. Termination releases
        // the mutex and the last job handle even if a driver or COM call is stuck.
        unsafe { TerminateProcess(GetCurrentProcess(), 1) };
    });
    if watchdog.is_err() { std::process::exit(1); }
    let Ok(_job) = tool_job() else { std::process::exit(1) };
    std::process::exit(restore_output(run_id).unwrap_or(1));
}

fn tool_job() -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: the unnamed job has a non-inheritable handle owned by this helper.
    // The sized limits structure lives through the call. Children inherit job
    // membership, so closing the handle at process exit also ends any live tool.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() { return Err(io::Error::last_os_error()); }
        let job = OwnedHandle::from_raw_handle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(job.as_raw_handle(), JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(), size_of_val(&limits) as u32) == 0
            || AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

fn restore_output(run_id: &str) -> Result<i32> {
    ensure!(run_id.len() == 32 && run_id.bytes().all(|b| b.is_ascii_hexdigit()), "Invalid repair run id");
    let app = install::protected_app()?;
    // SAFETY: this call checks membership of the current process token.
    ensure!(unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() } != 0, "Repair needs administrator approval");
    std::fs::create_dir_all(app.join("logs"))?;
    let folder = install::installed_path(&app, "logs")?;
    install::check_permissions(&folder)?;
    let mut log = File::create_new(folder.join(format!("repair-output-{run_id}.log")))?;
    writeln!(log, "Repair {run_id} started")?;
    match restore(&app, &mut log) {
        Ok(Restored::Active) => {
            writeln!(log, "Repair finished with a working device and active render endpoint")?;
            Ok(0)
        }
        Ok(Restored::DeviceNotWorking) => {
            writeln!(log, "Repair ended with the device present but not working")?;
            Ok(2)
        }
        Err(e) => {
            writeln!(log, "Repair failed: {e:#}")?;
            Ok(1)
        }
    }
}

fn repair_lock() -> Result<OwnedHandle> {
    // SAFETY: no handle is inherited. A new named mutex belongs to this run until
    // its last handle closes. A concurrent run closes its handle and exits.
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, windows_sys::core::w!("Global\\StreamToSpeaker.OutputRepair"));
        if handle.is_null() { return Err(io::Error::last_os_error().into()); }
        let exists = GetLastError() == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle::from_raw_handle(handle);
        ensure!(!exists, "Another repair is already running");
        Ok(handle)
    }
}

fn tool(log: &mut File, kind: DriverTool, system: &Path, path: &Path, args: &[&OsStr]) -> Result<()> {
    writeln!(log, "{} {args:?}", path.display())?;
    let mut child = Command::new(path).args(args).current_dir(system).env("PATH", system)
        .stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log.try_clone()?)
        .creation_flags(CREATE_NO_WINDOW).spawn().with_context(|| format!("Start {}", path.display()))?;
    let status = wait_for_tool(&mut child, Duration::from_secs(60))
        .with_context(|| format!("Wait for {}", path.display()))?;
    writeln!(log, "\nExit: {status}")?;
    ensure!(kind.accepts(status.code()), "{} failed with {status}", path.display());
    Ok(())
}

enum Restored { Active, DeviceNotWorking }

fn restore_device(d: &pnp::Device, devcon: &Path, system: &Path, log: &mut File) -> Result<()> {
    if d.disabled() { return d.enable(); }
    if needs_restart(d.problem, || Ok(active_endpoint(|node| node == d.node)?.is_some()))? {
        let instance = format!("@{}", d.instance);
        tool(log, DriverTool::DevCon, system, devcon, &["restart".as_ref(), instance.as_ref()])?;
    }
    Ok(())
}

fn restore(app: &Path, log: &mut File) -> Result<Restored> {
    let _lock = repair_lock()?;
    let system = system_directory()?;
    let pnputil = system.join("pnputil.exe");
    let inf = install::installed_path(app, r"driver\StreamToSpeaker.inf")?;
    tool(log, DriverTool::PnpUtil, &system, &pnputil, &["/add-driver".as_ref(), inf.as_os_str(), "/install".as_ref()])?;
    let devices = pnp::devices()?;
    let devcon = install::installed_path(app, r"driver\devcon.exe")?;
    if devices.is_empty() {
        tool(log, DriverTool::DevCon, &system, &devcon, &["install".as_ref(), inf.as_os_str(), pnp::HARDWARE_ID.as_ref()])?;
    }
    for d in devices {
        writeln!(log, "{} has problem code {}", d.instance, d.problem)?;
        if let Err(e) = restore_device(&d, &devcon, &system, log) {
            writeln!(log, "Could not change the state of {}: {e:#}", d.instance)?;
        }
    }

    let until = Instant::now() + Duration::from_secs(15);
    loop {
        match active_endpoint(pnp::working) {
            Ok(Some(id)) => {
                if let Err(e) = crate::endpoint_name::set_endpoint_friendly_name(&id, crate::PRODUCT_NAME) {
                    writeln!(log, "Could not name the render endpoint {id}: {e:#}")?;
                }
                match crate::endpoint_name::endpoint_friendly_name(&id) {
                    Some(name) if name != crate::PRODUCT_NAME => writeln!(log, "Windows kept the endpoint name {name:?}")?,
                    None => writeln!(log, "Could not read the endpoint name back")?,
                    _ => {}
                }
                if active_endpoint(pnp::working)?.as_ref() == Some(&id) {
                    writeln!(log, "Active render endpoint {id}")?;
                    return Ok(Restored::Active);
                }
            }
            Err(e) => writeln!(log, "Reading the output: {e:#}")?,
            Ok(None) => {}
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() { break; }
        std::thread::sleep(left.min(Duration::from_millis(250)));
    }
    let devices = pnp::devices()?;
    for d in &devices { writeln!(log, "{} has problem code {}", d.instance, d.problem)?; }
    if !devices.is_empty() && devices.iter().all(pnp::Device::enabled_but_not_working) {
        return Ok(Restored::DeviceNotWorking);
    }
    anyhow::bail!("No active render endpoint on a working {} device", pnp::HARDWARE_ID)
}
