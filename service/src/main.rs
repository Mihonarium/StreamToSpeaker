//! Stream To Speaker — main binary entry point.
//!
//! Two modes:
//!   - GUI mode (default): native window + system tray on the main thread,
//!     audio + control planes on background threads. Closing the window
//!     minimises to tray; only "Quit" in the tray menu actually exits.
//!   - Headless mode (`--headless`): traditional CLI service, no GUI, no
//!     tray. Audio loop runs on the main thread. Use for service installs
//!     or when running over SSH.
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

    /// Speaker to play on: its address, or part of its name, as
    /// --list-speakers shows them (any kind of speaker). The app starts
    /// anyway and connects when that speaker is found. Without it, the
    /// speaker used last time; --headless in a terminal asks instead.
    #[arg(long)]
    player: Option<String>,

    /// Search for speakers, print the ones found and exit.
    #[arg(long, default_value_t = false)]
    list_speakers: bool,

    /// Skip the interactive picker even when stdin is a TTY — useful for
    /// running as a Windows service.
    #[arg(long, default_value_t = false)]
    no_interactive: bool,

    /// Run in headless mode (no GUI window, no system tray). Audio loop
    /// runs on the main thread, lifecycle is the same as before the GUI
    /// was added.
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

    /// Interval between SSDP re-discoveries (minutes).
    #[arg(long, default_value_t = 5)]
    ssdp_interval: u64,

    /// Initial buffer hint sent to the speaker in DIDL metadata (ms).
    #[arg(long, default_value_t = 50)]
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

/// Show `e` where the person who ran the program sees it: in the console
/// it was run from (or wherever its output was redirected), else in a
/// message box. Returns the exit code.
fn report_arguments(e: &clap::Error) -> i32 {
    #[cfg(windows)]
    if !attach_parent_console() && !has_standard_output() {
        let text = argument_message(e);
        if e.use_stderr() {
            show_crash_dialog(&text);
        } else {
            show_info_dialog(&text);
        }
        return e.exit_code();
    }
    let _ = e.print();
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
            stream_to_speaker::single_instance::Claim::First => {}
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
    builder.filter_level(level).format_timestamp_millis();
    // GUI mode has no console (windows_subsystem = "windows"), so
    // env_logger's default stderr target writes to a dead handle.
    // Pipe to %LOCALAPPDATA%\StreamToSpeaker\stream-to-speaker.log so
    // the user (and the GUI's Help menu) can find the log later.
    // Headless mode (and --web-link) keeps stderr so logs still appear in
    // the attached parent console.
    if !cli.console() {
        if let Some(file) = open_log_file() {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();
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

fn open_log_file() -> Option<std::fs::File> {
    let mut path = stream_to_speaker::log_dir()?;
    path.push("stream-to-speaker.log");
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .ok()
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
    let (source, reopen_source) = build_source(&cli)?;
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

    // Auto-reconnect watchdog — replaces a dropped AirPlay session with a
    // fresh one so the UI never shows a zombie "streaming" state.
    app.spawn_reconnect_watchdog();

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
    let choice = launch_choice(&cli, picker::is_interactive(), saved.as_deref());
    let saved_manual = match &choice {
        LaunchChoice::Saved(id) => Some(id.clone())
            .filter(|_| app.is_auto_reconnect_on_launch())
            .filter(|id| app.find_manual_speaker(id).is_some())
            .filter(|id| app.auto_connect_allowed_at_launch(id)),
        _ => None,
    };
    if let LaunchChoice::Wish(query) = &choice {
        // In the background: the speaker may not be on the network yet.
        app.connect_when_listed(query.clone());
    } else if let Some(id) = saved_manual {
        info!("auto-reconnect: connecting to saved manual speaker {:?}", id);
        app.connect_in_background(&id);
    } else if let LaunchChoice::Ask = choice {
        match picker::interactive_pick(&app)? {
            Some(id) => {
                if let Err(e) = app.select_speaker(&id) {
                    warn!("starting initial session failed: {}", e);
                }
            }
            None => warn!("no speaker selected; pick one with the web UI / POST /api/select"),
        }
    } else if !cli.no_discovery {
        let discovery = app.discovery.as_ref().unwrap();
        let mut pair_wait_started = false;
        let initial = match choice {
            LaunchChoice::Wish(_) | LaunchChoice::Ask => None,
            LaunchChoice::Saved(saved_id) => {
                if !app.is_auto_reconnect_on_launch() {
                    info!("auto-reconnect disabled by user preference; saved={:?}", saved_id);
                    None
                } else if !app.auto_connect_allowed_at_launch(&saved_id) {
                    info!("auto-reconnect: {:?} is set to never auto-connect", saved_id);
                    None
                } else {
                    info!("auto-reconnect: trying saved speaker {:?}", saved_id);
                    // Wait briefly for the first SSDP sweep to populate
                    // discovery before we look the saved id up.
                    picker::wait_for_first_discovery(discovery, Duration::from_secs(5));
                    let found = discovery.find_by_id(&saved_id);
                    if found.is_none() {
                        match discovery.identity_conflict(&saved_id) {
                            Some(addrs) => warn!(
                                "auto-reconnect skipped: different devices at {:?} all claim to be the saved \
                                 speaker {:?}; pick the right one by address",
                                addrs, saved_id
                            ),
                            None if !saved_id.starts_with("airplay:") => {
                                info!("auto-reconnect: saved speaker {:?} not found on the network", saved_id)
                            }
                            None => {}
                        }
                    }
                    // A saved HomePod stereo pair (or one of its members): give
                    // both members time to resolve in the background, then play
                    // the pair — unless the user picked something meanwhile.
                    if found.is_none() && saved_id.starts_with("airplay:") {
                        if let Some(ad) = app.airplay_discovery.clone() {
                            let app = app.clone();
                            pair_wait_started = std::thread::Builder::new()
                                .name("stream-to-speaker-pair-reconnect".into())
                                .spawn(move || {
                                    let Some(id) = ad.wait_for_pair(&saved_id, Duration::from_secs(15)) else {
                                        return;
                                    };
                                    if app.session.lock().unwrap().is_some()
                                        || app.connecting.lock().unwrap().is_some()
                                    {
                                        return;
                                    }
                                    info!("auto-reconnect: stereo pair {:?}", id);
                                    app.select_speaker_async(&id);
                                })
                                .is_ok();
                        }
                    }
                    found
                }
            }
            LaunchChoice::Nothing => {
                info!("first launch (no saved speaker) — waiting for manual pick");
                None
            }
        };
        if let Some(r) = initial {
            let id = r.stable_id();
            if let Err(e) = app.select_speaker(&id) {
                warn!("starting initial session failed: {}", e);
            }
        } else if cli.headless && !pair_wait_started {
            warn!(
                "no speaker selected; pick one with the GUI / web UI / POST /api/select"
            );
        }
    }

    // Speakers set to connect automatically whenever they appear.
    app.spawn_auto_connect();

    if cli.headless {
        run_headless(app)
    } else {
        // Keep an existing sign-in entry pointing at this exe.
        stream_to_speaker::autostart::reapply();
        run_gui_mode(app, cli.no_tray, cli.startup && !cli.no_tray)
    }
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

    // From here a new launch waits for this copy to end instead of
    // handing over to it: there is no window left to show.
    stream_to_speaker::single_instance::announce_closing();
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
    app.start_discovery(Duration::from_secs(cli.ssdp_interval * 60));
    Ok(app)
}

// -----------------------------------------------------------------------------
// HTTP server setup
// -----------------------------------------------------------------------------

fn start_http(app: &Arc<App>) -> Result<u16> {
    let gena_callback = {
        let vsync = app.vsync.clone();
        let pusher = build_driver_volume_pusher();
        let pusher = pusher.map(Arc::new);
        // The window's volume slider moves the Windows slider through the
        // same push.
        if let Some(p) = pusher.clone() {
            app.set_windows_volume_push(move |mb, muted| p.push(mb, muted));
        }
        Arc::new(move |path: &str, body: &str| {
            debug!("GENA NOTIFY on {}: {} bytes", path, body.len());
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
        Arc::new(move |id: &str| app2.select_speaker(id))
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
    picker::write_speaker_list(&mut std::io::stdout(), &speakers);
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
    Ok(())
}

// -----------------------------------------------------------------------------
// Source selection
// -----------------------------------------------------------------------------

/// The audio source the flags ask for, and how to open the same kind again
/// if it fails while the app runs (the audio loop does that). Under `auto`
/// that is the kind found at start: a driver that goes away during a
/// driver update is waited for, not swapped for WASAPI.
fn build_source(cli: &Cli) -> Result<(Box<dyn AudioSource>, audio_loop::Reopen)> {
    match cli.source {
        SourceKind::Sine => Ok((open_sine()?, Box::new(open_sine))),
        SourceKind::WasapiLoopback => {
            #[cfg(windows)]
            {
                Ok((open_wasapi(cli.device.as_deref())?, reopen_wasapi(cli.device.clone())))
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
                Ok((src, Box::new(open_driver)))
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
                    Ok(s) => Ok((s, Box::new(open_driver))),
                    Err(e) => {
                        info!("driver not present, falling back to WASAPI loopback: {}", e);
                        Ok((open_wasapi(cli.device.as_deref())?, reopen_wasapi(cli.device.clone())))
                    }
                }
            }
            #[cfg(not(windows))]
            {
                warn!("non-Windows host; using sine source for --source auto");
                Ok((open_sine()?, Box::new(open_sine)))
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

/// `terminal`: standard input and output are a terminal someone can
/// answer in. A saved speaker always wins over one that merely answered
/// first: without someone to ask, nothing is picked for them.
fn launch_choice(cli: &Cli, terminal: bool, saved: Option<&str>) -> LaunchChoice {
    if let Some(q) = &cli.player {
        LaunchChoice::Wish(q.clone())
    } else if cli.headless && !cli.no_interactive && terminal {
        LaunchChoice::Ask
    } else if let Some(id) = saved {
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
            let _ = writeln!(
                std::io::stderr(),
                "Quitting: telling the speaker to stop (press Ctrl+C again to quit at once)…"
            );
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
        assert_eq!(launch_choice(&cli(&["--headless"]), false, Some("uuid:A")), LaunchChoice::Saved("uuid:A".into()));
        assert_eq!(launch_choice(&cli(&["--headless"]), false, None), LaunchChoice::Nothing);
    }

    #[test]
    fn a_headless_run_in_a_terminal_asks_unless_told_not_to() {
        let saved = Some("uuid:A");
        assert_eq!(launch_choice(&cli(&["--headless"]), true, saved), LaunchChoice::Ask);
        assert_eq!(
            launch_choice(&cli(&["--headless", "--no-interactive"]), true, saved),
            LaunchChoice::Saved("uuid:A".into())
        );
        // The window is where a GUI run picks, terminal or not.
        assert_eq!(launch_choice(&cli(&[]), true, saved), LaunchChoice::Saved("uuid:A".into()));
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
    fn player_names_the_speaker_whatever_else_holds() {
        for terminal in [false, true] {
            assert_eq!(
                launch_choice(&cli(&["--headless", "--player", "Kitchen"]), terminal, Some("uuid:A")),
                LaunchChoice::Wish("Kitchen".into())
            );
        }
    }
}
