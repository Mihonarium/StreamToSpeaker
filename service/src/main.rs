//! Stream To Speaker — main binary entry point.
//!
//! Two modes:
//!   - GUI mode (default): native window + system tray on the main thread,
//!     audio + control planes on background threads. Closing the window
//!     minimises to tray; only "Quit" in the tray menu actually exits.
//!   - Headless mode (`--headless`): traditional CLI service, no GUI, no
//!     tray; the main thread waits for Ctrl+C while the same background
//!     threads run. Use for service installs or when running over SSH.
//!
//! The web UI and its HTTP/JSON API (`/api/*`) are OFF by default — pass
//! `--web` (or turn them on in the window). Both need the web UI's link,
//! which carries a secret key: the page lives under `/r/<key>/`, and API
//! calls send the key as `Authorization: Bearer <key>`. The window shows
//! the link; `--web-link` prints it and exits, for headless and service
//! installs, which have no window.

// Windows subsystem = "windows" — no console window ever auto-allocated.
// GUI mode: the egui window + tray are the entire user-facing surface.
// --headless: we explicitly AttachConsole(ATTACH_PARENT_PROCESS) at
// startup so output flows to the terminal that launched us (and if
// there isn't one, output goes nowhere — that's fine for service-style
// runs). This is the standard pattern for "GUI app that can also be a
// CLI"; the previous "console subsystem + hide it after the fact" had
// a race where conhost would briefly flash on screen.
#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use log::{debug, error, info, warn};
use std::net::{IpAddr, SocketAddr};
#[cfg(windows)]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use stream_to_speaker::airplay::AirPlayDiscoveryState;
use stream_to_speaker::app::{App, AppConfig};
use stream_to_speaker::audio_source::AudioSource;
use stream_to_speaker::gena::parse_rendering_notify;
use stream_to_speaker::http_server::{
    start_http_server, HttpServerConfig, LatencyAdjustCallback, ResyncCallback, SpeakerInfo,
    SpeakerListCallback, SpeakerSelectCallback, StreamClientAllowedCallback,
};
use stream_to_speaker::picker;
use stream_to_speaker::silence::DEFAULT_QUIESCENT_AFTER_PACKETS;
use stream_to_speaker::sine_source::SineSource;
use stream_to_speaker::ssdp::DiscoveryState;
use stream_to_speaker::{audio_loop, PRODUCT_NAME};
#[cfg(windows)]
use stream_to_speaker::wasapi_source::WasapiLoopbackSource;

// -----------------------------------------------------------------------------
// CLI
// -----------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum SourceKind {
    /// Try the kernel driver, falling back to WASAPI loopback if absent.
    Auto,
    /// Force the kernel driver; exit with an error if unavailable.
    Driver,
    /// WASAPI loopback (no driver required).
    WasapiLoopback,
    /// 440 Hz sine test source.
    Sine,
}

#[derive(Parser, Debug)]
#[command(
    name = "stream-to-speaker",
    version,
    about = "Stream To Speaker — plays this PC's sound on network speakers: Sonos and other UPnP \
             speakers, AirPlay and AirPlay 2, and Sendspin (Music Assistant)."
)]
struct Cli {
    /// Audio source.
    #[arg(long, value_enum, default_value_t = SourceKind::Auto)]
    source: SourceKind,

    /// TCP port for the optional web UI / API and for serving the
    /// audio stream URL the speaker pulls from.
    #[arg(long, default_value_t = 5901)]
    port: u16,

    /// Specific WASAPI device name (substring match). Only used for the
    /// wasapi-loopback source.
    #[arg(long)]
    device: Option<String>,

    /// Speaker to play on, any kind: its address, its full name, or part of
    /// a name (see --list-speakers). The app starts anyway. After the first
    /// search (a few seconds) it takes the best match: the speaker at that
    /// address, else the one with that full name, else a speaker shown in
    /// the list whose name contains it; if none matches yet, the first that
    /// does later. Without it, the speaker used last time; --headless in a
    /// terminal asks instead.
    #[arg(long)]
    player: Option<String>,

    /// Search for speakers, print the ones found and exit.
    #[arg(long, default_value_t = false)]
    list_speakers: bool,

    /// Skip the interactive picker even when stdin is a TTY — useful for
    /// running as a Windows service.
    #[arg(long, default_value_t = false)]
    no_interactive: bool,

    /// Run without the window and the tray icon, as a service or in a
    /// terminal (Ctrl+C quits). See --player, --web and --web-link for
    /// choosing a speaker and controlling it.
    #[arg(long, default_value_t = false)]
    headless: bool,

    /// Hide the system tray icon (only relevant in GUI mode).
    #[arg(long, default_value_t = false)]
    no_tray: bool,

    /// Launched by the sign-in (Run) entry: start in the tray without
    /// opening the window. Ignored with --no-tray.
    #[arg(long, default_value_t = false, hide = true)]
    startup: bool,

    /// Enable the HTTP / JSON API and built-in web UI. Both need the web
    /// UI's link, which carries a secret key (shown in the window; printed
    /// at start with --headless, and by --web-link). The audio stream
    /// itself (`/stream.raw`) is always served — Sonos needs it — but the
    /// page and `/api/*` only answer while the web UI is on.
    #[arg(long, default_value_t = false)]
    web: bool,

    /// Print the web UI's link and exit, making its key if there is none
    /// yet. For headless and service installs, which have no window to
    /// show it in: run it as the account the app runs under (the key is
    /// kept in that account's settings), with the app's --port and
    /// --advertise-ip if it has them.
    #[arg(long, default_value_t = false, conflicts_with = "list_speakers")]
    web_link: bool,

    /// Interval between SSDP re-discoveries (minutes; 0 counts as 1).
    #[arg(long, default_value_t = 5)]
    ssdp_interval: u64,

    /// Accepted for old shortcuts and services; it has no effect (no
    /// speaker is sent a prebuffer hint).
    #[arg(long, default_value_t = 50, hide = true)]
    initial_buffer_ms: u32,

    /// Skip SSDP discovery; serve HTTP only.
    #[arg(long, default_value_t = false)]
    no_discovery: bool,

    /// Skip AirPlay (mDNS) discovery. SSDP/UPnP is unaffected. Use
    /// this if mDNS is noisy / blocked on the local network or if you
    /// only ever stream to Sonos via UPnP and want to skip the
    /// background browse traffic.
    #[arg(long, default_value_t = false)]
    no_airplay: bool,

    /// Skip Sendspin: no Sendspin speakers in the list and no Music
    /// Assistant input.
    #[arg(long, default_value_t = false)]
    no_sendspin: bool,

    /// Offer this computer's audio to Music Assistant as a Sendspin
    /// input for this run, even when Sendspin is off in settings.
    #[arg(long, default_value_t = false)]
    sendspin_source: bool,

    /// Disable silence injection (will send literal zeros during silence).
    #[arg(long, default_value_t = false)]
    no_silence_injection: bool,

    /// Number of consecutive silent packets before entering quiescence.
    #[arg(long, default_value_t = DEFAULT_QUIESCENT_AFTER_PACKETS)]
    silence_packets_threshold: u32,

    /// Bind address for the HTTP server. Default 0.0.0.0. With 127.0.0.1
    /// only a speaker running on this PC can fetch the stream, and
    /// discovery doesn't list a speaker on this PC (loopback included): add
    /// it by address (+ Add, under Speakers in the window).
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Local IP to give every speaker (stream URI, callbacks, AirPlay), to
    /// search for speakers from, and to put in the web UI's link. Default:
    /// per speaker, the address this PC uses to reach that speaker;
    /// discovery on every interface; in the link, the address worked out
    /// when it is shown.
    #[arg(long)]
    advertise_ip: Option<IpAddr>,

    /// Log level: error, warn, info, debug, trace. Default: info; warn for
    /// --list-speakers and --web-link, which print what they are for.
    #[arg(long)]
    log_level: Option<String>,

    /// Silence-injection pacing (wall-clock ms per 10 ms silence packet).
    /// Default 10 = real-time; >10 drains the speaker's prebuffer.
    #[arg(long, default_value_t = 10)]
    silence_pace_ms: u64,

    /// Rate-fudge ppm for clock-skew compensation. Positive over-produces.
    #[arg(long, default_value_t = 0)]
    rate_fudge_ppm: i32,

    /// Max frames added/dropped per packet for runtime latency adjust.
    #[arg(long, default_value_t = 4)]
    latency_adjust_step_frames: u32,
}

impl Cli {
    /// Prints something and exits (`--list-speakers`, `--web-link`): not a
    /// running copy of the app, so it neither hands over to one nor counts
    /// as one.
    fn one_shot(&self) -> bool {
        self.list_speakers || self.web_link
    }

    /// Runs as a console program: output and errors go to the terminal
    /// that started it (or wherever its caller redirected them), never to
    /// a window.
    fn console(&self) -> bool {
        self.headless || self.one_shot()
    }
}

/// What parsing the command line produced instead of a command (help, the
/// version, or a mistake), as the text a message box shows when there is no
/// console to print it in.
#[cfg_attr(not(windows), allow(dead_code))]
fn argument_message(e: &clap::Error) -> String {
    let text = e.render().to_string();
    if !e.use_stderr() {
        // Help or the version.
        return text;
    }
    format!(
        "{} couldn't start: its command line has a mistake.\n\n{}\n\nCheck the shortcut or command that \
         started it.",
        PRODUCT_NAME,
        text.trim_end()
    )
}

/// Where a message from parsing the command line goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArgumentReport {
    /// The console it was run from, or its redirected output.
    Print,
    /// A message box: a start from a shortcut or the sign-in entry.
    Dialog,
    /// Nowhere: a --headless start (a service, a scheduled task) with no
    /// console, where a message box would wait on a desktop nobody sees.
    Nowhere,
}

/// `headless`: the arguments asked for `--headless` (looked up in the raw
/// arguments, since parsing failed); `can_print`: there is a console or a
/// redirected output.
fn argument_report(headless: bool, can_print: bool) -> ArgumentReport {
    match (can_print, headless) {
        (true, _) => ArgumentReport::Print,
        (false, true) => ArgumentReport::Nowhere,
        (false, false) => ArgumentReport::Dialog,
    }
}

/// Whether the raw command line asks for `--headless`.
fn asks_for_headless(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    args.into_iter().skip(1).take_while(|a| a != "--").any(|a| a == "--headless")
}

/// Show `e` where the person who ran the program sees it (see
/// [`argument_report`]). Returns the exit code.
fn report_arguments(e: &clap::Error) -> i32 {
    #[cfg(windows)]
    let can_print = attach_parent_console() || has_standard_output();
    #[cfg(not(windows))]
    let can_print = true;
    match argument_report(asks_for_headless(std::env::args_os()), can_print) {
        ArgumentReport::Print => {
            let _ = e.print();
        }
        ArgumentReport::Dialog => {
            let text = argument_message(e);
            #[cfg(windows)]
            if e.use_stderr() {
                show_crash_dialog(&text);
            } else {
                show_info_dialog(&text);
            }
            #[cfg(not(windows))]
            let _ = text;
        }
        ArgumentReport::Nowhere => {}
    }
    e.exit_code()
}

fn main() {
    // Startup tombstone. Written as the FIRST thing in main, before
    // any state initialization, CLI parsing, panic-hook install, or
    // singleton check could turn around and exit silently. If this
    // file exists but no log line follows it, we know the process
    // got at least this far and then died between the tombstone and
    // env_logger.init(). If the file DOESN'T exist after a launch
    // attempt, the binary isn't starting at all (SmartScreen,
    // antivirus quarantine, missing dependency DLL, broken install).
    write_startup_tombstone();

    // Help, the version and mistakes in the arguments are printed where
    // the person will see them, before anything else runs.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => std::process::exit(report_arguments(&e)),
    };

    // Console programs (--headless, --list-speakers, --web-link): attach to
    // the parent terminal so the user sees the output. GUI mode: no console
    // at all (windows_subsystem = "windows" already takes care of that):
    // attached, it would also get the terminal's Ctrl+C and be closed with
    // its window.
    #[cfg(windows)]
    if cli.console() {
        attach_parent_console();
    }

    // Initialize logging EARLY — before the panic hook and the
    // singleton-mutex gate. Previously these ran first and any
    // failure between them was invisible because log::error! was a
    // no-op until builder.init() landed.
    init_logging(&cli);
    info!("{} v{} entering main()", PRODUCT_NAME, stream_to_speaker::display_version());

    // Crash visibility. With windows_subsystem="windows" the default
    // panic handler writes to stderr — which is a dead handle in GUI
    // mode, so a panic just kills the process with no on-screen
    // indication. Install a hook that:
    //   1. logs the panic + a backtrace into the log file (so the
    //      Help menu's "Open log folder" gives the user something to
    //      report);
    //   2. on Windows GUI mode, shows a MessageBox so they at least
    //      see WHY the window disappeared, instead of "task manager
    //      shows it but nothing's painted."
    #[allow(unused_variables)]
    let console = cli.console();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg: String = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<Any>".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let bt = std::backtrace::Backtrace::force_capture();
        let full = format!("panic at {}: {}\n{}", location, msg, bt);
        log::error!("{}", full);
        #[cfg(windows)]
        if !console {
            show_crash_dialog(&format!("Stream To Speaker hit a fatal error:\n\n{}\n\n{}\n\nFull backtrace is in the log file (Help → Open log folder).",
                location, msg));
        }
    }));

    // Single-instance gate. If another instance is already running in
    // this session, ask it to show its window and exit — same pattern as
    // Slack / Discord / OBS. Without this, the duplicate launch races to
    // bind port 5901, the HTTP bind fails with WSAEADDRINUSE, run()
    // returns Err, the process exits silently — and the user sees the FIRST
    // instance in task manager and assumes the new install "doesn't
    // open". The hand-off goes through a named event the first instance
    // listens on from here on, so it works before that instance has a
    // window too; a copy that is quitting is waited for instead. Skipped in
    // --headless: headless is for service / CLI runs where multiple
    // invocations against the same port are the caller's problem. Skipped
    // for --web-link and --list-speakers too (see below).
    if !cli.console() {
        match stream_to_speaker::single_instance::claim() {
            stream_to_speaker::single_instance::Claim::First => {
                // This copy is the one that writes the log from now on.
                if let Some(path) = log_path() {
                    LOG.set_aside_if_larger(&path, LOG_MAX_BYTES);
                }
            }
            stream_to_speaker::single_instance::Claim::Another => {
                info!("another instance is running; asked it to show its window, exiting");
                return;
            }
            stream_to_speaker::single_instance::Claim::StillClosing => {
                info!("the copy that is quitting hasn't ended yet; exiting");
                if !cli.startup {
                    show_info_dialog(stream_to_speaker::single_instance::STILL_CLOSING);
                }
                return;
            }
        }
    }

    // Create the singleton mutex (one call, one process) AND get the
    // "was another instance already holding it?" signal in the same
    // round-trip — CreateMutexA reports both atomically via the
    // returned handle + GetLastError(). The handle is intentionally
    // leaked: it lives for the process lifetime so the Inno Setup
    // installer's AppMutex check (and our own duplicate-launch gate
    // below, for FUTURE launches) keep seeing the kernel object.
    // The Global\ mutex is what the installer watches for a running copy;
    // its answer also covers a copy that never takes the per-session
    // claim above: a --headless one, one in another session or account,
    // or one running elevated. After the claim, so a copy that was quitting
    // has let go of it. --web-link and --list-speakers are not a running
    // copy — they print and exit, usually while the app runs — so they take
    // no part in either.
    #[cfg(windows)]
    let (_mutex_handle, machine_lock) = if cli.one_shot() {
        (None, stream_to_speaker::single_instance::MachineLock::Unavailable)
    } else {
        create_singleton_mutex()
    };
    #[cfg(not(windows))]
    let machine_lock = stream_to_speaker::single_instance::MachineLock::Created;

    // Running, but not as a GUI in this session we could hand over to.
    // Starting anyway would only fail to bind the stream port.
    if !cli.console() {
        if let stream_to_speaker::single_instance::Launch::Exit { log, tell } =
            stream_to_speaker::single_instance::launch(machine_lock, cli.startup)
        {
            info!("{}; exiting", log);
            if let Some(message) = tell {
                show_info_dialog(message);
            }
            return;
        }
    }

    if let Err(e) = run(cli) {
        error!("fatal: {:#}", e);
        #[cfg(windows)]
        if !console {
            show_crash_dialog(&format!(
                "Stream To Speaker couldn't start:\n\n{:#}\n\nSee the log file for details (Help → Open log folder in a working instance, or %LOCALAPPDATA%\\StreamToSpeaker\\stream-to-speaker.log).",
                e
            ));
        }
        std::process::exit(1);
    }
}

/// Write a one-line marker file to %LOCALAPPDATA%\StreamToSpeaker\
/// startup.txt so we can tell — even when env_logger hasn't been
/// initialized yet — whether the binary actually started executing.
/// If the file is absent after a launch attempt, the binary itself
/// didn't run (SmartScreen / antivirus / missing runtime DLL); if it's
/// present but the log file has no matching entry, main() got at least
/// to this point and died before init_logging.
fn write_startup_tombstone() {
    if let Some(mut path) = stream_to_speaker::log_dir() {
        path.push("startup.txt");
        set_aside_if_larger(&path, STARTUP_LOG_MAX_BYTES);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(
                f,
                "{}\tv{}\tmain() entered",
                now,
                stream_to_speaker::display_version()
            );
        }
    }
}

fn init_logging(cli: &Cli) {
    let default = if cli.one_shot() { log::LevelFilter::Warn } else { log::LevelFilter::Info };
    let level = cli.log_level.as_deref().and_then(|l| l.parse().ok()).unwrap_or(default);
    let mut builder = env_logger::Builder::from_default_env();
    // Log lines carry names devices gave themselves, and a console run
    // writes them to a terminal: no control characters.
    builder.filter_level(level).format(|buf, record| {
        use std::io::Write as _;
        writeln!(
            buf,
            "[{} {:<5} {}] {}",
            buf.timestamp_millis(),
            record.level(),
            record.target(),
            picker::printable_text(&record.args().to_string())
        )
    });
    // GUI mode has no console (windows_subsystem = "windows"), so
    // env_logger's default stderr target writes to a dead handle.
    // Pipe to %LOCALAPPDATA%\StreamToSpeaker\stream-to-speaker.log so
    // the user (and the GUI's Help menu) can find the log later.
    // Headless mode (and --web-link) keeps stderr so logs still appear in
    // the attached parent console.
    if !cli.console() {
        if let Some(file) = log_path().and_then(|p| open_log_at(&p)) {
            *LOG.0.lock().unwrap() = Some(file);
            builder.target(env_logger::Target::Pipe(Box::new(SharedLog)));
        }
    }
    builder.init();
}

/// The log file this process writes, shared with the logger so that the
/// copy which becomes the session's instance can set a big log aside and
/// go on in a fresh one.
struct LogSlot(std::sync::Mutex<Option<std::fs::File>>);

static LOG: LogSlot = LogSlot(std::sync::Mutex::new(None));

impl LogSlot {
    /// If the log at `path` is larger than `max_bytes`, move it aside to
    /// `<path>.1` (replacing an older one) and write a fresh one from now on.
    fn set_aside_if_larger(&self, path: &std::path::Path, max_bytes: u64) {
        let mut file = self.0.lock().unwrap();
        if std::fs::metadata(path).map_or(true, |m| m.len() <= max_bytes) {
            return;
        }
        let mut aside = path.as_os_str().to_owned();
        aside.push(".1");
        if std::fs::rename(path, aside).is_ok() {
            if let Some(fresh) = open_log_at(path) {
                *file = Some(fresh);
            }
        }
    }
}

/// The logger's end of [`LOG`].
struct SharedLog;

impl std::io::Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match LOG.0.lock().unwrap().as_mut() {
            Some(f) => f.write(buf),
            None => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match LOG.0.lock().unwrap().as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Windows MessageBox for fatal-error visibility. No-op on non-Windows.
#[cfg(windows)]
fn show_crash_dialog(body: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_TOPMOST, MB_SETFOREGROUND,
    };
    let body_w: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    let title_w: Vec<u16> = "Stream To Speaker"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

/// Windows MessageBox telling the user why nothing else happens. No-op on
/// non-Windows.
#[cfg(windows)]
fn show_info_dialog(body: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let body_w: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    let title_w: Vec<u16> = PRODUCT_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
fn show_info_dialog(_body: &str) {}

/// Largest the log file and `startup.txt` grow before the next start moves
/// them aside (one older copy is kept, as `<name>.1`).
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const STARTUP_LOG_MAX_BYTES: u64 = 256 * 1024;

/// Move `path` aside to `<path>.1` (replacing an older one) if it is
/// larger than `max_bytes`, so a log written by every run doesn't grow
/// without end.
fn set_aside_if_larger(path: &std::path::Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() <= max_bytes {
        return;
    }
    let mut aside = path.as_os_str().to_owned();
    aside.push(".1");
    let _ = std::fs::rename(path, aside);
}

fn log_path() -> Option<std::path::PathBuf> {
    Some(stream_to_speaker::log_dir()?.join("stream-to-speaker.log"))
}

/// Open the log at `path` for appending. Every launch does this, a second
/// one too while the first runs: only the session's instance sets a big
/// log aside (see [`LogSlot::set_aside_if_larger`]).
fn open_log_at(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new().append(true).create(true).open(path).ok()
}

/// Attach this process to its parent's console, if launched from a
/// terminal; whether it did. If the parent had no console (double-click
/// launch), output goes nowhere unless it was redirected — which is the
/// right thing for service-style runs.
#[cfg(windows)]
fn attach_parent_console() -> bool {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}

/// Whether this process was given somewhere to write (its output was
/// redirected to a file or a pipe), console or not.
#[cfg(windows)]
fn has_standard_output() -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].into_iter().any(|which| {
        let h = unsafe { GetStdHandle(which) };
        !h.is_null() && h != INVALID_HANDLE_VALUE
    })
}

/// Open (or create) the named mutex the Inno Setup installer watches
/// via AppMutex. Held until process exit. Global\ prefix so the
/// elevated installer process can see a mutex held by the user-session
/// service.
///
/// A launch in the same session hands over to the running GUI through
/// `single_instance::claim`; this one tells Setup "the app is running",
/// and tells a GUI launch about a copy it can't hand over to.
///
/// Returns the handle (None if the mutex wasn't created or opened) and
/// what the call said about other copies. Another account's or an
/// elevated copy's mutex can't be opened (access denied): its creator's
/// default security applies.
#[cfg(windows)]
fn create_singleton_mutex() -> (Option<isize>, stream_to_speaker::single_instance::MachineLock) {
    use std::ffi::CString;
    use stream_to_speaker::single_instance::{machine_lock_from, MachineLock};
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::CreateMutexA;
    let Some(name) = CString::new("Global\\StreamToSpeaker.Singleton").ok() else {
        return (None, MachineLock::Unavailable);
    };
    unsafe {
        let h = CreateMutexA(std::ptr::null(), 0, name.as_ptr() as *const u8);
        let lock = machine_lock_from(!h.is_null(), GetLastError());
        ((!h.is_null()).then_some(h as isize), lock)
    }
}

fn run(cli: Cli) -> Result<()> {
    info!("{} v{}", PRODUCT_NAME, stream_to_speaker::display_version());

    // --list-speakers short-circuit
    if cli.list_speakers {
        return cmd_list_speakers(&cli);
    }
    // --web-link: before anything that claims the driver, the port or the
    // network, none of which printing a link needs.
    if cli.web_link {
        return cmd_web_link(&cli);
    }

    // Build the audio source up front so we fail fast if the driver is
    // missing under --source driver.
    let Source { audio: source, reopen: reopen_source, driver: uses_driver } = build_source(&cli)?;
    info!("audio source: {}", source.name());

    let app = setup_app(&cli)?;
    if cli.no_sendspin {
        app.disable_sendspin();
    } else {
        app.init_sendspin(!cli.no_discovery, cli.sendspin_source);
        if cli.player.is_none() {
            app.reconnect_saved_sendspin_speaker();
        }
    }

    // Always start the full HTTP server (audio + API + UI). The page and
    // /api/* routes check app.web_access at request time, so the GUI can
    // flip the web UI on and off without re-binding the socket. The
    // initial state mirrors --web.
    let port_chosen = start_http(&app)?;
    info!("stream server listening on port {}", port_chosen);
    match app.web_link() {
        // The link is the web UI's credential: printed once for a headless
        // run, which has no window to show it in, and never logged. A run
        // as a service may have no console to print to: --web-link prints
        // the link at any time.
        Some(link) if cli.headless => {
            // Not println!: it panics (here: aborts) when stdout is a
            // closed pipe.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stdout(),
                "Web UI: {}\nAnyone with this link can control {} on this network.",
                link,
                PRODUCT_NAME
            );
            info!("web UI on; its link went to standard output, not this log (--web-link prints it)");
        }
        Some(_) => info!("web UI on; its link is in the window's Web UI card"),
        None => info!("web UI off — turn it on in the window, or start with --web"),
    }

    // Driver-event consumer.
    #[cfg(windows)]
    spawn_driver_event_consumer(app.clone(), &cli);

    // The driver's device in Windows' sound settings is named after the
    // speaker being played on ("Stream To Speaker → Kitchen"), and only
    // while it is.
    #[cfg(windows)]
    if uses_driver {
        app.follow_endpoint_label(stream_to_speaker::endpoint_name::update_endpoint_name);
    }

    // Once-a-day "newer release on GitHub?" check (Advanced toggle; a
    // dev build without a baked release tag never checks).
    app.spawn_update_checker();

    // Now-playing metadata forwarder (off by default; pushes the OS's
    // current track to the speaker when enabled).
    app.spawn_now_playing_forwarder();

    // Ctrl-C handler — set the shutdown flag.
    install_signal_handler(app.clone());

    // Apply initial values from CLI to the runtime atomics.
    app.set_silence_pace_ms(cli.silence_pace_ms);
    app.set_rate_fudge_ppm(cli.rate_fudge_ppm);
    app.set_latency_adjust_step_frames(cli.latency_adjust_step_frames);

    // Start the audio loop before any speaker is told to play: from then
    // on /stream.raw always carries data, Windows' sound or, while Windows
    // plays nothing, silence in its place.
    let audio_app = app.clone();
    let _audio_thread = thread::Builder::new()
        .name("stream-to-speaker-audio".to_string())
        .spawn(move || {
            if let Err(e) = audio_loop::run(audio_app.clone(), source, reopen_source) {
                error!("audio loop couldn't start: {:#}", e);
                audio_app.record_error(format!(
                    "Couldn't start reading this PC's sound ({:#}). Quit {} and start it again.",
                    e, PRODUCT_NAME
                ));
            }
        })
        .context("spawning audio thread")?;

    // The first speaker: see `launch_choice`. With nobody to ask and
    // nothing saved, none: the window's getting-started card asks the
    // person, and their pick is saved for next time.
    //
    // A saved speaker added by address needs no discovery: connect in the
    // background (the window and tray come up meanwhile), with or without
    // --no-discovery.
    let saved = app.saved_speaker_id();
    let saved_needs_no_search = saved.as_deref().is_some_and(|id| {
        app.find_manual_speaker(id).is_some()
            && app.is_auto_reconnect_on_launch()
            && app.auto_connect_allowed_at_launch(id)
    });
    let facts = LaunchFacts {
        terminal: picker::is_interactive(),
        saved: saved.as_deref(),
        saved_needs_no_search,
        manual_speakers: app.manual_speakers().len(),
    };
    let choice = launch_choice(&cli, facts);
    // One owner of where the sound plays from here on: the window, the
    // tray, the web API and the terminal prompt send it commands. It brings
    // a saved speaker back by its own rule, and gives Windows' sound back
    // at quit.
    let engine = stream_to_speaker::engine::Engine::start(
        app.clone(),
        stream_to_speaker::platform::Platform::passive(),
        matches!(choice, LaunchChoice::Saved(_)),
    );
    if let LaunchChoice::Wish(query) = &choice {
        // In the background: the speaker may not be on the network yet.
        app.connect_when_listed(query.clone());
    } else if let LaunchChoice::Ask = choice {
        match picker::interactive_pick(&app, saved.as_deref())? {
            Some(id) => {
                if let Err(e) = app.select_speaker_from(stream_to_speaker::model::Surface::CommandLine, &id) {
                    warn!("starting initial session failed: {}", e);
                }
            }
            None => warn!("no speaker selected; pick one with the web UI / POST /api/select"),
        }
    } else if cli.headless && !matches!(choice, LaunchChoice::Saved(_)) {
        warn!("no speaker selected; pick one with the GUI / web UI / POST /api/select");
    }

    let ran = if cli.headless {
        let quitting = engine.clone();
        app.on_quit(move || quitting.quit());
        run_headless(app)
    } else {
        // Keep an existing sign-in entry pointing at this exe.
        stream_to_speaker::autostart::reapply();
        // From the moment Quit is chosen, a new launch waits for this copy
        // to end instead of handing over to it: it is about to have no
        // window to show.
        let quitting = engine.clone();
        app.on_quit(move || {
            stream_to_speaker::single_instance::announce_closing();
            quitting.quit();
        });
        run_gui_mode(app, cli.no_tray, cli.startup && !cli.no_tray)
    };
    // Nothing plays on a speaker any more: the device's plain name.
    #[cfg(windows)]
    if uses_driver {
        stream_to_speaker::endpoint_name::update_endpoint_name_within(None, Duration::from_secs(1));
    }
    #[cfg(not(windows))]
    let _ = uses_driver;
    ran
}

// -----------------------------------------------------------------------------
// Headless run — block on shutdown signal; audio is on the bg thread
// spawned in run() above.
// -----------------------------------------------------------------------------

fn run_headless(app: Arc<App>) -> Result<()> {
    while !app.is_shutting_down() {
        std::thread::sleep(Duration::from_millis(200));
    }
    app.shut_down();
    Ok(())
}

// -----------------------------------------------------------------------------
// GUI run — eframe on main thread; audio is on the bg thread spawned
// in run() above.
// -----------------------------------------------------------------------------

#[cfg(windows)]
fn run_gui_mode(app: Arc<App>, no_tray: bool, start_hidden: bool) -> Result<()> {
    // Run the GUI (blocks until window/tray exits).
    if let Err(e) = stream_to_speaker::gui::run(app.clone(), !no_tray, start_hidden) {
        warn!("GUI exited with error: {:#}", e);
        // Surface to the user — without this the process just
        // disappears from the screen (window never created) but
        // remains in the tray, looking like "no window opened at all".
        show_crash_dialog(&format!(
            "Stream To Speaker couldn't open its window:\n\n{:#}\n\nLog file: %LOCALAPPDATA%\\StreamToSpeaker\\stream-to-speaker.log",
            e
        ));
    }

    // Tell the audio loop to stop. Its reader thread is deliberately not
    // joined: it waits inside the driver's read for the next packet,
    // which while Windows plays nothing never comes. Process exit ends
    // it, and Windows closes its handle.
    app.request_shutdown();
    app.shut_down();
    Ok(())
}

#[cfg(not(windows))]
fn run_gui_mode(_app: Arc<App>, _no_tray: bool, _start_hidden: bool) -> Result<()> {
    anyhow::bail!("GUI mode is Windows-only — pass --headless on this platform")
}

// -----------------------------------------------------------------------------
// Setup helpers
// -----------------------------------------------------------------------------

/// Time between the periodic UPnP searches. 0 used to mean searching
/// without a pause (and none at all while the chosen adapter was away), so
/// it counts as 1 minute.
fn search_interval(cli: &Cli) -> Duration {
    if cli.ssdp_interval == 0 {
        warn!("--ssdp-interval 0 would search without a pause; searching every minute instead");
    }
    Duration::from_secs(cli.ssdp_interval.max(1).saturating_mul(60))
}

fn setup_app(cli: &Cli) -> Result<Arc<App>> {
    let bind_addr: IpAddr = cli
        .bind
        .parse()
        .with_context(|| format!("parsing --bind {}", cli.bind))?;
    let bind_socket = SocketAddr::new(bind_addr, cli.port);

    let ssdp_iface = if cli.no_discovery {
        None
    } else {
        stream_to_speaker::app::pick_ssdp_iface(cli.advertise_ip, &cli.bind)
    };

    // The browsers are started by the app once its saved settings (the
    // chosen network adapter) are loaded.
    let discovery = (!cli.no_discovery).then(DiscoveryState::new);
    let airplay_discovery = (!cli.no_airplay).then(AirPlayDiscoveryState::new);

    let config = AppConfig {
        advertise_ip: cli.advertise_ip,
        bind: bind_socket,
        initial_buffer_ms: cli.initial_buffer_ms,
        silence_packets_threshold: cli.silence_packets_threshold,
        no_silence_injection: cli.no_silence_injection,
        no_discovery: cli.no_discovery,
        web_enabled: cli.web,
        ssdp_iface,
    };

    let app = App::new(config, discovery, airplay_discovery);
    app.start_discovery(search_interval(cli));
    Ok(app)
}

// -----------------------------------------------------------------------------
// HTTP server setup
// -----------------------------------------------------------------------------

fn start_http(app: &Arc<App>) -> Result<u16> {
    let gena_callback = {
        let vsync = app.vsync.clone();
        let transport = app.clone();
        let pusher = build_driver_volume_pusher();
        let pusher = pusher.map(Arc::new);
        // The window's volume slider moves the Windows slider through the
        // same push.
        if let Some(p) = pusher.clone() {
            app.set_windows_volume_push(move |mb, muted| p.push(mb, muted));
        }
        Arc::new(move |path: &str, body: &str| {
            debug!("GENA NOTIFY on {}: {} bytes", path, body.len());
            if let Some(change) = stream_to_speaker::gena::parse_transport_notify(body) {
                transport.note_transport(change);
            }
            if let Some(change) = parse_rendering_notify(body) {
                // Set our node's dB to the linear position for the
                // speaker's percent (Windows then shows the slider at the
                // same %), keeping level and mute consistent.
                if let Some((mb, muted)) = vsync.speaker_event(change.volume, change.mute) {
                    info!(
                        "speaker -> windows: volume {:?} mute {:?} (mb={}, muted={})",
                        change.volume, change.mute, mb, muted
                    );
                    if let Some(p) = pusher.as_ref() {
                        if let Err(e) = p.push(mb, muted) {
                            warn!("failed to push volume/mute to driver: {}", e);
                        }
                    }
                }
            }
        }) as Arc<dyn Fn(&str, &str) + Send + Sync>
    };

    let speaker_list: SpeakerListCallback = {
        let app2 = app.clone();
        Arc::new(move || app2.speaker_view().speakers)
            as Arc<dyn Fn() -> Vec<SpeakerInfo> + Send + Sync>
    };
    let speaker_select: SpeakerSelectCallback = {
        let app2 = app.clone();
        Arc::new(move |id: &str| app2.select_speaker_from_remote(id))
            as Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>
    };
    let resync: ResyncCallback = {
        let app2 = app.clone();
        Arc::new(move || app2.resync())
            as Arc<dyn Fn() -> Result<(), String> + Send + Sync>
    };
    let latency_adjust: LatencyAdjustCallback = {
        let app2 = app.clone();
        Arc::new(move |ms| app2.adjust_latency(ms))
            as Arc<dyn Fn(i32) -> i64 + Send + Sync>
    };
    let stream_client_allowed: StreamClientAllowedCallback = {
        let app2 = app.clone();
        Arc::new(move |ip| app2.check_stream_client(ip))
    };

    start_http_server(HttpServerConfig {
        bind: app.config.bind,
        hub: app.hub.clone(),
        gena_callback: Some(gena_callback),
        speaker_list: Some(speaker_list),
        speaker_select: Some(speaker_select),
        resync: Some(resync),
        latency_adjust: Some(latency_adjust),
        web_access: Some(app.web_access.clone()),
        stream_client_allowed: Some(stream_client_allowed),
        // Host names besides this PC's own addresses, which are always
        // accepted: an --advertise-ip that names a proxy in front of it.
        extra_hosts: app.config.advertise_ip.iter().map(|ip| ip.to_string()).collect(),
        speaker_peer: Some({
            let app2 = app.clone();
            Arc::new(move |ip| app2.is_speaker_peer(ip))
        }),
        catch_up: Some(app.delay.catch_up()),
    })
}

// -----------------------------------------------------------------------------
// --list-speakers
// -----------------------------------------------------------------------------

fn cmd_list_speakers(cli: &Cli) -> Result<()> {
    use std::io::Write as _;
    if cli.no_discovery {
        return Err(anyhow!("--list-speakers searches for speakers: leave out --no-discovery"));
    }
    // The same discovery a run with these flags and settings has, without
    // the stream server or the audio: another copy may be running.
    let app = setup_app(cli)?;
    if !cli.no_sendspin {
        app.browse_sendspin_speakers();
    }
    picker::search_and_wait(&app, picker::SEARCH_WAIT);
    let speakers = app.speaker_view_opts(true).speakers;
    picker::write_speaker_list(&mut std::io::stdout(), &speakers, app.saved_speaker_id().as_deref());
    if speakers.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "No speakers found. Check that this PC is on the same network as the speakers, and that \
             the firewall lets {} search it.",
            PRODUCT_NAME
        );
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// --web-link
// -----------------------------------------------------------------------------

/// Print the web UI's link and exit. The key comes from the settings of
/// the account this runs as (made and saved there if there is none yet),
/// so it must be the account the app runs under; the address and port are
/// worked out as a run with the same flags does before it has a speaker.
/// The link is the web UI's credential: it goes to standard output only,
/// never to the log.
fn cmd_web_link(cli: &Cli) -> Result<()> {
    use std::io::Write as _;
    use stream_to_speaker::app;
    use stream_to_speaker::user_config::UserConfig;
    let bind: IpAddr = cli.bind.parse().with_context(|| format!("parsing --bind {}", cli.bind))?;
    let host = app::this_pc_address_from(
        cli.advertise_ip,
        bind,
        None,
        || None,
        app::default_route_ip,
        app::first_local_ipv4,
    );
    let link = stream_to_speaker::web_access::link_from_settings(
        &host.to_string(),
        cli.port,
        UserConfig::load,
        UserConfig::save,
    )
    .map_err(|e| anyhow!("--web-link: {e}"))?;
    writeln!(std::io::stdout(), "{link}").context("writing the link to standard output")?;
    let _ = writeln!(
        std::io::stderr(),
        "Anyone with this link can control {} on this network.",
        PRODUCT_NAME
    );
    if let Some(note) = web_link_note(host, cli.advertise_ip, bind) {
        let _ = writeln!(std::io::stderr(), "{note}");
    }
    Ok(())
}

/// What to add when the link printed for `host` works on this PC only.
fn web_link_note(host: IpAddr, explicit: Option<IpAddr>, bind: IpAddr) -> Option<&'static str> {
    if !host.is_loopback() {
        return None;
    }
    if explicit.is_none() && bind.is_unspecified() {
        Some("This link works on this PC only: it has no network connection yet. Run --web-link again once it has.")
    } else {
        Some("This link works on this PC only.")
    }
}

// -----------------------------------------------------------------------------
// Source selection
// -----------------------------------------------------------------------------

/// The audio source a run reads.
struct Source {
    audio: Box<dyn AudioSource>,
    /// How to open the same kind again if it fails while the app runs (the
    /// audio loop does that).
    reopen: audio_loop::Reopen,
    /// It is the Stream To Speaker driver.
    driver: bool,
}

impl Source {
    fn new(audio: Box<dyn AudioSource>, reopen: audio_loop::Reopen, driver: bool) -> Self {
        Self { audio, reopen, driver }
    }
}

/// The audio source the flags ask for. Under `auto`, reopening keeps to the
/// kind found at start: a driver that goes away during a driver update is
/// waited for, not swapped for WASAPI.
fn build_source(cli: &Cli) -> Result<Source> {
    match cli.source {
        SourceKind::Sine => Ok(Source::new(open_sine()?, Box::new(open_sine), false)),
        SourceKind::WasapiLoopback => {
            #[cfg(windows)]
            {
                Ok(Source::new(open_wasapi(cli.device.as_deref())?, reopen_wasapi(cli.device.clone()), false))
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("--source wasapi-loopback only works on Windows")
            }
        }
        SourceKind::Driver => {
            #[cfg(windows)]
            {
                let src = open_driver()
                    .context("opening Stream-To-Speaker kernel driver (--source driver was specified)")?;
                Ok(Source::new(src, Box::new(open_driver), true))
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("--source driver only works on Windows")
            }
        }
        SourceKind::Auto => {
            #[cfg(windows)]
            {
                match open_driver() {
                    Ok(s) => Ok(Source::new(s, Box::new(open_driver), true)),
                    Err(e) => {
                        info!("driver not present, falling back to WASAPI loopback: {}", e);
                        Ok(Source::new(open_wasapi(cli.device.as_deref())?, reopen_wasapi(cli.device.clone()), false))
                    }
                }
            }
            #[cfg(not(windows))]
            {
                warn!("non-Windows host; using sine source for --source auto");
                Ok(Source::new(open_sine()?, Box::new(open_sine), false))
            }
        }
    }
}

fn open_sine() -> Result<Box<dyn AudioSource>> {
    Ok(Box::new(SineSource::new()))
}

#[cfg(windows)]
fn open_driver() -> Result<Box<dyn AudioSource>> {
    Ok(Box::new(stream_to_speaker::ioctl_source::IoctlAudioSource::open_audio_only()?))
}

#[cfg(windows)]
fn open_wasapi(device: Option<&str>) -> Result<Box<dyn AudioSource>> {
    Ok(Box::new(WasapiLoopbackSource::new(device)?))
}

#[cfg(windows)]
fn reopen_wasapi(device: Option<String>) -> audio_loop::Reopen {
    Box::new(move || open_wasapi(device.as_deref()))
}

// -----------------------------------------------------------------------------
// Driver event consumer + volume pusher
// -----------------------------------------------------------------------------

#[cfg(windows)]
fn spawn_driver_event_consumer(app: Arc<App>, cli: &Cli) {
    if cli.source == SourceKind::WasapiLoopback || cli.source == SourceKind::Sine {
        return;
    }
    use stream_to_speaker::ioctl_source::IoctlAudioSource;
    thread::Builder::new()
        .name("stream-to-speaker-driver-events".to_string())
        .spawn(move || {
            // The first open says whether there is a driver at all (`auto`
            // runs on WASAPI without one). Once open, a consumer that ends
            // (the device was disabled, removed or updated) is opened
            // again, as the audio source is. One consumer at a time: two
            // would split the driver's single event stream between them.
            let mut src = match IoctlAudioSource::open() {
                Ok(s) => s,
                Err(e) => {
                    warn!("driver-event consumer: open failed: {}", e);
                    return;
                }
            };
            let mut attempts = 0;
            loop {
                if consume_driver_events(&app, &src) > 0 {
                    attempts = 0;
                }
                if app.is_shutting_down() {
                    return;
                }
                app.note_driver_events_lost("the driver ended its event stream");
                drop(src);
                let reopened =
                    audio_loop::open_again(&app, &mut attempts, "the driver's events", IoctlAudioSource::open);
                src = match reopened {
                    Some(s) => s,
                    None => return,
                };
                app.note_driver_events_back();
            }
        })
        .ok();
}

/// Act on the driver's events until its event stream ends or the app
/// shuts down. Returns how many there were.
#[cfg(windows)]
fn consume_driver_events(app: &Arc<App>, src: &stream_to_speaker::ioctl_source::IoctlAudioSource) -> u64 {
    use stream_to_speaker::ioctl_source::DriverEvent;
    use stream_to_speaker::volume_sync::SpeakerCommand;
    let rx = src.events();
    let mut count = 0;
    while let Ok(ev) = rx.recv() {
        if app.is_shutting_down() {
            return count;
        }
        count += 1;
        match ev {
            DriverEvent::VolumeChanged { level_millibels } => {
                // Windows maps our hardware volume node's slider linearly
                // across its dB range, so convert the dB back to a percent
                // linearly to track it 1:1.
                let level = stream_to_speaker::volume_sync::millibels_to_sonos(level_millibels);
                if let Some(level) = app.vsync.driver_changed(level) {
                    info!("windows -> speaker: volume {} (mb={})", level, level_millibels);
                    // Queued for whichever session kind is active and sent
                    // from the volume thread: a speaker round trip here
                    // would hold up the events behind this one (and, under
                    // the session lock, the window).
                    app.send_to_speaker(SpeakerCommand::Volume(level));
                }
            }
            DriverEvent::MuteChanged { muted } => {
                info!("driver -> speaker: mute={}", muted);
                app.vsync.note_muted(muted);
                app.send_to_speaker(SpeakerCommand::Mute(muted));
            }
            DriverEvent::StreamStart => {
                info!("driver: stream start");
                app.stream_active.store(true, Ordering::Release);
            }
            DriverEvent::StreamStop => {
                info!("driver: stream stop");
                app.stream_active.store(false, Ordering::Release);
            }
            DriverEvent::FormatChange {
                sample_rate,
                bits_per_sample,
                channels,
            } => {
                info!(
                    "driver: format change to {}/{}-bit/{}ch",
                    sample_rate, bits_per_sample, channels
                );
            }
        }
    }
    count
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn spawn_driver_event_consumer(_app: Arc<App>, _cli: &Cli) {}

trait DriverVolumePush: Send + Sync {
    fn push(&self, mb: i32, muted: bool) -> Result<()>;
}

#[cfg(windows)]
struct IoctlPusher {
    src: std::sync::Mutex<stream_to_speaker::ioctl_source::IoctlAudioSource>,
}

#[cfg(windows)]
impl DriverVolumePush for IoctlPusher {
    fn push(&self, mb: i32, muted: bool) -> Result<()> {
        use stream_to_speaker::ioctl_source::IoctlAudioSource;
        let mut s = self.src.lock().unwrap();
        match s.push_volume(mb, muted) {
            Ok(()) => Ok(()),
            // After the device was restarted under the app (Device Manager,
            // a driver update) the old handle is dead and a new one works.
            Err(e) => match IoctlAudioSource::open_audio_only() {
                Ok(fresh) => {
                    *s = fresh;
                    s.push_volume(mb, muted)
                }
                Err(_) => Err(e),
            },
        }
    }
}

#[cfg(windows)]
fn build_driver_volume_pusher() -> Option<Box<dyn DriverVolumePush>> {
    use stream_to_speaker::ioctl_source::IoctlAudioSource;
    match IoctlAudioSource::open_audio_only() {
        Ok(s) => Some(Box::new(IoctlPusher {
            src: std::sync::Mutex::new(s),
        })),
        Err(e) => {
            debug!("no driver for volume pusher: {}", e);
            None
        }
    }
}

#[cfg(not(windows))]
fn build_driver_volume_pusher() -> Option<Box<dyn DriverVolumePush>> {
    None
}

// -----------------------------------------------------------------------------
// Signal handler
// -----------------------------------------------------------------------------

/// Which speaker a launch starts with, by the flags and the saved choice.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LaunchChoice {
    /// `--player`: the speaker it names.
    Wish(String),
    /// Ask in the terminal.
    Ask,
    /// The speaker saved last time (if its settings allow reconnecting).
    Saved(String),
    /// None: the person picks one.
    Nothing,
}

/// What a launch's choice depends on besides the flags.
#[derive(Clone, Copy, Debug, Default)]
struct LaunchFacts<'a> {
    /// Standard input and output are a terminal someone can answer in.
    terminal: bool,
    /// The speaker saved last time.
    saved: Option<&'a str>,
    /// It is added by address and its settings let it reconnect at
    /// launch: it needs no search.
    saved_needs_no_search: bool,
    /// How many speakers are added by address (listed without a search).
    manual_speakers: usize,
}

/// A saved speaker always wins over one that merely answered first:
/// without someone to ask, nothing is picked for them. A terminal is asked
/// only when there is something to choose from: without discovery, not
/// when the saved speaker can be reconnected or nothing could be listed.
fn launch_choice(cli: &Cli, facts: LaunchFacts) -> LaunchChoice {
    let nothing_to_choose =
        cli.no_discovery && (facts.saved_needs_no_search || facts.manual_speakers == 0);
    if let Some(q) = &cli.player {
        LaunchChoice::Wish(q.clone())
    } else if cli.headless && !cli.no_interactive && facts.terminal && !nothing_to_choose {
        LaunchChoice::Ask
    } else if let Some(id) = facts.saved {
        LaunchChoice::Saved(id.to_string())
    } else {
        LaunchChoice::Nothing
    }
}

/// Ctrl+C quits, telling the speaker first; a second Ctrl+C quits without
/// waiting for it.
fn install_signal_handler(app: Arc<App>) {
    if let Err(e) = ctrlc::set_handler(move || {
        // Not eprintln!: it panics (here: aborts) when stderr is a closed pipe.
        use std::io::Write as _;
        if !app.is_shutting_down() {
            let line = match app.selected_speaker() {
                Some(s) => format!(
                    "Quitting: telling {} to stop (press Ctrl+C again to quit at once)…",
                    picker::printable(&s.friendly_name)
                ),
                None => "Quitting…".to_string(),
            };
            let _ = writeln!(std::io::stderr(), "{line}");
            app.request_shutdown();
        } else {
            let _ = writeln!(std::io::stderr(), "Quitting without waiting for the speaker.");
            app.quit_without_waiting();
        }
    }) {
        warn!("could not install Ctrl-C handler: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("stream-to-speaker").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn a_headless_run_without_a_terminal_reconnects_the_saved_speaker() {
        // A service, a scheduled task, a hidden window: nobody to ask.
        let saved = LaunchFacts { saved: Some("uuid:A"), ..Default::default() };
        assert_eq!(launch_choice(&cli(&["--headless"]), saved), LaunchChoice::Saved("uuid:A".into()));
        assert_eq!(launch_choice(&cli(&["--headless"]), LaunchFacts::default()), LaunchChoice::Nothing);
    }

    #[test]
    fn a_headless_run_in_a_terminal_asks_unless_told_not_to() {
        let facts = LaunchFacts { terminal: true, saved: Some("uuid:A"), ..Default::default() };
        assert_eq!(launch_choice(&cli(&["--headless"]), facts), LaunchChoice::Ask);
        assert_eq!(
            launch_choice(&cli(&["--headless", "--no-interactive"]), facts),
            LaunchChoice::Saved("uuid:A".into())
        );
        // The window is where a GUI run picks, terminal or not.
        assert_eq!(launch_choice(&cli(&[]), facts), LaunchChoice::Saved("uuid:A".into()));
    }

    #[test]
    fn without_discovery_a_terminal_is_asked_only_when_there_is_a_choice() {
        let headless = cli(&["--headless", "--no-discovery"]);
        // The saved speaker added by address reconnects, as it always did.
        let reconnectable = LaunchFacts {
            terminal: true,
            saved: Some("manual:den"),
            saved_needs_no_search: true,
            manual_speakers: 2,
        };
        assert_eq!(launch_choice(&headless, reconnectable), LaunchChoice::Saved("manual:den".into()));
        // Nothing could be listed: run without a speaker, as it always did.
        let empty = LaunchFacts { terminal: true, ..Default::default() };
        assert_eq!(launch_choice(&headless, empty), LaunchChoice::Nothing);
        // Speakers added by address, and nothing to reconnect: ask.
        let choice = LaunchFacts { terminal: true, manual_speakers: 2, ..Default::default() };
        assert_eq!(launch_choice(&headless, choice), LaunchChoice::Ask);
    }

    #[test]
    fn listing_speakers_is_a_console_command_that_hands_nothing_over() {
        assert!(cli(&["--list-speakers"]).console());
        assert!(cli(&["--list-speakers"]).one_shot());
        assert!(cli(&["--web-link"]).one_shot());
        assert!(cli(&["--headless"]).console());
        assert!(!cli(&["--headless"]).one_shot());
        assert!(!cli(&[]).console());
    }

    fn parse_error(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(std::iter::once("stream-to-speaker").chain(args.iter().copied())).unwrap_err()
    }

    #[test]
    fn a_mistake_in_the_arguments_is_shown_with_what_to_check() {
        let text = argument_message(&parse_error(&["--plaeyr", "Kitchen"]));
        assert!(text.starts_with("Stream To Speaker couldn't start"), "{text}");
        assert!(text.contains("--plaeyr"), "{text}");
        assert!(text.contains("Check the shortcut"), "{text}");
        assert!(!text.contains('\u{1b}'), "terminal colours in a message box: {text:?}");
    }

    #[test]
    fn help_and_the_version_are_shown_as_they_are() {
        let help = argument_message(&parse_error(&["--help"]));
        assert!(help.contains("--list-speakers") && !help.contains("couldn't start"), "{help}");
        let version = argument_message(&parse_error(&["--version"]));
        assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
    }

    #[test]
    fn every_search_interval_that_used_to_start_still_does() {
        let minutes = |m: &str| search_interval(&cli(&["--ssdp-interval", m]));
        // 0 searched without a pause: now once a minute.
        assert_eq!(minutes("0"), Duration::from_secs(60));
        assert_eq!(minutes("5"), Duration::from_secs(300));
        assert_eq!(minutes("10000"), Duration::from_secs(600_000));
        assert_eq!(minutes("18446744073709551615"), Duration::from_secs(u64::MAX));
        assert_eq!(search_interval(&cli(&[])), Duration::from_secs(300));
    }

    #[test]
    fn port_zero_still_starts() {
        // It streams to AirPlay and Sendspin speakers; UPnP and the web
        // link don't use the port really bound yet.
        assert_eq!(cli(&["--port", "0"]).port, 0);
    }

    #[test]
    fn a_headless_start_never_waits_on_a_message_box() {
        let args = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert!(asks_for_headless(args(&["x", "--plaeyr", "K", "--headless"])));
        assert!(!asks_for_headless(args(&["x", "--plaeyr", "K"])));
        assert!(!asks_for_headless(args(&["x", "--", "--headless"])));
        assert_eq!(argument_report(true, false), ArgumentReport::Nowhere);
        assert_eq!(argument_report(true, true), ArgumentReport::Print);
        assert_eq!(argument_report(false, false), ArgumentReport::Dialog);
        assert_eq!(argument_report(false, true), ArgumentReport::Print);
    }

    #[test]
    fn opening_the_log_leaves_a_big_one_in_place() {
        // A second launch opens the running copy's log; it must not move it.
        let dir = std::env::temp_dir().join(format!("sts-log-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stream-to-speaker.log");
        std::fs::write(&log, vec![b'x'; (LOG_MAX_BYTES + 1) as usize]).unwrap();
        drop(open_log_at(&log));
        assert_eq!(std::fs::metadata(&log).unwrap().len(), LOG_MAX_BYTES + 1);
        assert!(!dir.join("stream-to-speaker.log.1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_instance_sets_a_big_log_aside_and_writes_a_fresh_one() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("sts-log-slot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stream-to-speaker.log");
        let slot = LogSlot(std::sync::Mutex::new(open_log_at(&log)));
        slot.0.lock().unwrap().as_mut().unwrap().write_all(&[b'o'; 101]).unwrap();
        slot.set_aside_if_larger(&log, 100);
        slot.0.lock().unwrap().as_mut().unwrap().write_all(b"after").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"after");
        assert_eq!(std::fs::read(dir.join("stream-to-speaker.log.1")).unwrap(), vec![b'o'; 101]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_for_this_pc_only_says_so() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let any = ip("0.0.0.0");
        assert!(web_link_note(ip("127.0.0.1"), None, any).unwrap().contains("no network connection"));
        assert_eq!(web_link_note(ip("127.0.0.1"), None, ip("127.0.0.1")), Some("This link works on this PC only."));
        assert_eq!(web_link_note(ip("192.168.1.20"), None, any), None);
    }

    #[test]
    fn a_log_that_grew_too_big_is_set_aside_once() {
        let dir = std::env::temp_dir().join(format!("sts-log-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stream-to-speaker.log");
        let previous = dir.join("stream-to-speaker.log.1");
        std::fs::write(&log, vec![b'x'; 100]).unwrap();
        set_aside_if_larger(&log, 100);
        assert_eq!(std::fs::read(&log).unwrap().len(), 100, "a log at the cap stays");
        std::fs::write(&log, vec![b'y'; 101]).unwrap();
        std::fs::write(&previous, b"older").unwrap();
        set_aside_if_larger(&log, 100);
        assert!(!log.exists(), "the big log is moved aside");
        assert_eq!(std::fs::read(&previous).unwrap(), vec![b'y'; 101], "it replaces the older one");
        set_aside_if_larger(&log, 100); // no log at all: nothing to do
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_help_offers_no_flag_that_does_nothing() {
        let help = argument_message(&parse_error(&["--help"]));
        assert!(!help.contains("--initial-buffer-ms"), "{help}");
        // Still accepted, so shortcuts and services that pass it start.
        assert!(Cli::try_parse_from(["x", "--initial-buffer-ms", "50"]).is_ok());
    }

    #[test]
    fn player_names_the_speaker_whatever_else_holds() {
        for terminal in [false, true] {
            assert_eq!(
                launch_choice(
                    &cli(&["--headless", "--player", "Kitchen"]),
                    LaunchFacts { terminal, saved: Some("uuid:A"), ..Default::default() }
                ),
                LaunchChoice::Wish("Kitchen".into())
            );
        }
    }
}
