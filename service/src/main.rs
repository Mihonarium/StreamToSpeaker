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
//! The HTTP/JSON API (web UI at `:5901/`, `/api/*` endpoints) is OFF by
//! default — pass `--web` to enable it. Even with `--web`, prefer
//! `--bind 127.0.0.1` unless you trust everyone on the LAN.

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

use stream_to_speaker::airplay::{spawn_airplay_discovery, AirPlayDiscoveryState};
use stream_to_speaker::app::{App, AppConfig};
use stream_to_speaker::audio_source::AudioSource;
use stream_to_speaker::gena::parse_rendering_notify;
use stream_to_speaker::http_server::{
    start_http_server, HttpServerConfig, LatencyAdjustCallback, ResyncCallback, SpeakerInfo,
    SpeakerListCallback, SpeakerSelectCallback,
};
use stream_to_speaker::picker;
use stream_to_speaker::silence::DEFAULT_QUIESCENT_AFTER_PACKETS;
use stream_to_speaker::sine_source::SineSource;
use stream_to_speaker::ssdp::{spawn_discovery, DiscoveryState};
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
    about = "Stream To Speaker — streams Windows audio to UPnP/OpenHome network speakers."
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

    /// Speaker to send to: friendly name substring, IP, or omit to be
    /// prompted (in a terminal) / pick the first one (non-interactive).
    #[arg(long)]
    player: Option<String>,

    /// Print discovered speakers and exit.
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

    /// Enable the HTTP / JSON API and built-in web UI. The audio stream
    /// itself (`/stream.raw`) is always served — Sonos needs it — but the
    /// rest of the endpoints (`/`, `/api/*`) are only registered when
    /// this flag is set. Off by default to close the LAN-side hole.
    #[arg(long, default_value_t = false)]
    web: bool,

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

    /// Disable silence injection (will send literal zeros during silence).
    #[arg(long, default_value_t = false)]
    no_silence_injection: bool,

    /// Number of consecutive silent packets before entering quiescence.
    #[arg(long, default_value_t = DEFAULT_QUIESCENT_AFTER_PACKETS)]
    silence_packets_threshold: u32,

    /// Bind address for the HTTP server. Default 0.0.0.0; consider
    /// 127.0.0.1 + a speaker on the loopback alias.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Public IP to advertise to the speaker in the stream URI. Required
    /// when bind is 0.0.0.0. Defaults to the first non-loopback IPv4.
    #[arg(long)]
    advertise_ip: Option<String>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info")]
    log_level: String,

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

    let cli = Cli::parse();

    // Headless mode: try to attach to the parent terminal so the user
    // sees log output. GUI mode: no console at all (windows_subsystem
    // = "windows" already takes care of that).
    #[cfg(windows)]
    if cli.headless {
        attach_parent_console();
    }

    // Initialize logging EARLY — before the panic hook and the
    // singleton-mutex gate. Previously these ran first and any
    // failure between them was invisible because log::error! was a
    // no-op until builder.init() landed.
    init_logging(&cli);
    info!("{} v{} entering main()", PRODUCT_NAME, env!("CARGO_PKG_VERSION"));

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
    let headless = cli.headless;
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
        if !headless {
            show_crash_dialog(&format!("Stream To Speaker hit a fatal error:\n\n{}\n\n{}\n\nFull backtrace is in the log file (Help → Open log folder).",
                location, msg));
        }
    }));

    // Create the singleton mutex (one call, one process) AND get the
    // "was another instance already holding it?" signal in the same
    // round-trip — CreateMutexA reports both atomically via the
    // returned handle + GetLastError(). The handle is intentionally
    // leaked: it lives for the process lifetime so the Inno Setup
    // installer's AppMutex check (and our own duplicate-launch gate
    // below, for FUTURE launches) keep seeing the kernel object.
    #[cfg(windows)]
    let (_mutex_handle, another_running) = create_singleton_mutex();
    #[cfg(not(windows))]
    let another_running = false;
    info!("singleton: another_instance_running={}", another_running);

    // Single-instance gate. If another instance is already running,
    // raise its window and exit — same pattern as Slack / Discord /
    // OBS. Without this, the duplicate launch races to bind port 5901,
    // tiny_http fails with WSAEADDRINUSE, run() returns Err, the
    // process exits silently — and the user sees the FIRST instance
    // in task manager and assumes the new install "doesn't open".
    // Skipped in --headless: headless is for service / CLI runs where
    // multiple invocations against the same port are the caller's
    // problem.
    if !cli.headless && another_running {
        info!("another instance is running; raising its window and exiting");
        #[cfg(windows)]
        raise_existing_window();
        return;
    }

    if let Err(e) = run(cli) {
        error!("fatal: {:#}", e);
        #[cfg(windows)]
        if !headless {
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
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}

fn init_logging(cli: &Cli) {
    let mut builder = env_logger::Builder::from_default_env();
    builder
        .filter_level(cli.log_level.parse().unwrap_or(log::LevelFilter::Info))
        .format_timestamp_millis();
    // GUI mode has no console (windows_subsystem = "windows"), so
    // env_logger's default stderr target writes to a dead handle.
    // Pipe to %LOCALAPPDATA%\StreamToSpeaker\stream-to-speaker.log so
    // the user (and the GUI's Help menu) can find the log later.
    // Headless mode keeps stderr so logs still appear in the attached
    // parent console.
    if !cli.headless {
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
/// terminal. If the parent had no console (double-click launch), this
/// is a no-op and output silently goes nowhere — which is the right
/// thing for service-style runs.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        // Return value is ignored on purpose: failure (no parent
        // console) just means we operate without one.
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Open (or create) the named mutex the Inno Setup installer watches
/// via AppMutex. Held until process exit. Global\ prefix so the
/// elevated installer process can see a mutex held by the user-session
/// service.
///
/// Returns (handle, was_already_existing): if `was_already_existing`
/// is true, another instance of the app already owns the kernel
/// object and we are the duplicate launch — the caller should defer
/// to that instance (raise its window) and exit.
///
/// IMPORTANT: there is exactly ONE call to CreateMutexA per process.
/// Calling it twice within the same process would trip
/// ERROR_ALREADY_EXISTS on the second call (the first call IS the
/// existing owner) and make every launch look like a duplicate.
#[cfg(windows)]
fn create_singleton_mutex() -> (Option<isize>, bool) {
    use std::ffi::CString;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexA;
    let Some(name) = CString::new("Global\\StreamToSpeaker.Singleton").ok() else {
        return (None, false);
    };
    unsafe {
        let h = CreateMutexA(std::ptr::null(), 0, name.as_ptr() as *const u8);
        let was_existing = GetLastError() == ERROR_ALREADY_EXISTS;
        let handle = if h.is_null() { None } else { Some(h as isize) };
        (handle, was_existing)
    }
}

/// Find the existing instance's main window by title and bring it to
/// the foreground (un-minimize + raise Z order). Used by the
/// single-instance path. Best-effort; if FindWindowW returns null
/// (e.g. the other instance is mid-startup and hasn't created its
/// window yet) we just return — the user will see nothing happen but
/// the other instance is still running.
#[cfg(windows)]
fn raise_existing_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsIconic, SetForegroundWindow, SetWindowPos, ShowWindowAsync,
        HWND_TOP, SW_RESTORE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    let title: Vec<u16> = "Stream To Speaker"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            log::info!("another instance is running but its window isn't findable yet");
            return;
        }
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        if IsIconic(hwnd) != 0 {
            ShowWindowAsync(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

fn run(cli: Cli) -> Result<()> {
    info!("{} v{}", PRODUCT_NAME, env!("CARGO_PKG_VERSION"));

    // --list-speakers short-circuit
    if cli.list_speakers {
        return cmd_list_speakers(&cli);
    }

    // Build the audio source up front so we fail fast if the driver is
    // missing under --source driver.
    let source: Box<dyn AudioSource> = build_source(&cli)?;
    info!("audio source: {}", source.name());

    let app = setup_app(&cli)?;

    // Always start the full HTTP server (audio + API + UI). The /, /api/*
    // routes honour app.is_web_ui_enabled() at request time, so the GUI
    // can flip the web UI on and off without re-binding the socket. The
    // initial state mirrors --web.
    let port_chosen = start_http(&app)?;
    if app.is_web_ui_enabled() {
        info!("web UI: http://{}:{}/", app.config.advertise_ip, port_chosen);
    } else {
        info!(
            "stream URL: http://{}:{}/stream.raw (web UI off — toggle in the GUI)",
            app.config.advertise_ip, port_chosen
        );
    }

    // Driver-event consumer.
    #[cfg(windows)]
    spawn_driver_event_consumer(app.clone(), &cli);

    // Auto-reconnect watchdog — replaces a dropped AirPlay session with a
    // fresh one so the UI never shows a zombie "streaming" state.
    app.spawn_reconnect_watchdog();

    // Now-playing metadata forwarder (off by default; pushes the OS's
    // current track to the speaker when enabled).
    app.spawn_now_playing_forwarder();

    // Ctrl-C handler — set the shutdown flag.
    install_signal_handler(app.clone());

    // Apply initial values from CLI to the runtime atomics.
    app.set_silence_pace_ms(cli.silence_pace_ms);
    app.set_rate_fudge_ppm(cli.rate_fudge_ppm);
    app.set_latency_adjust_step_frames(cli.latency_adjust_step_frames);

    // Start the audio loop BEFORE we issue any UPnP Play. With the
    // old order (Play → spawn audio thread) the speaker connected to
    // /stream.raw before the audio loop was producing data — Sonos
    // would receive an empty HTTP body, time out, and stay in a
    // "I'm connected but not playing" state until the user hit
    // Resync. Spawning audio first means silence injection is already
    // running by the time we send Play, so the speaker gets a
    // populated stream from byte zero.
    let audio_app = app.clone();
    let _audio_thread = thread::Builder::new()
        .name("stream-to-speaker-audio".to_string())
        .spawn(move || {
            if let Err(e) = audio_loop::run(audio_app.clone(), source) {
                error!("audio loop exited with error: {:#}", e);
            }
        })
        .context("spawning audio thread")?;

    // Resolve initial speaker.
    //
    // Priority chain:
    //   1. `--player <hint>`  — explicit override, fuzzy match
    //   2. interactive headless TTY — prompt the user
    //   3. saved speaker id from user_config — auto-reconnect to the
    //      one the user last explicitly picked
    //   4. otherwise — DON'T auto-select. The GUI's onboarding card
    //      kicks in so the user picks manually one time; their
    //      choice gets persisted and step 3 takes over from then on.
    //
    // The old fallback ("first discovered") was the reason GUI users
    // never got to see the onboarding card and ended up with a random
    // speaker bound at launch.
    if !cli.no_discovery {
        let discovery = app.discovery.as_ref().unwrap();
        let initial = if cli.player.is_some() {
            picker::resolve(discovery, cli.player.as_deref(), false)?
        } else if !cli.no_interactive && cli.headless {
            picker::resolve(discovery, None, true)?
        } else if let Some(saved_id) = app.saved_speaker_id() {
            if !app.is_auto_reconnect_on_launch() {
                info!("auto-reconnect disabled by user preference; saved={:?}", saved_id);
                None
            } else {
                info!("auto-reconnect: trying saved speaker {:?}", saved_id);
                // Wait briefly for the first SSDP sweep to populate
                // discovery before we look the saved id up.
                picker::wait_for_first_discovery(discovery, Duration::from_secs(5));
                discovery.find_by_id(&saved_id)
            }
        } else {
            info!("first launch (no saved speaker) — waiting for manual pick");
            None
        };
        if let Some(r) = initial {
            let id = r.stable_id();
            if let Err(e) = app.select_speaker(&id) {
                warn!("starting initial session failed: {}", e);
            }
        } else if cli.headless {
            warn!(
                "no speaker selected; pick one with the GUI / web UI / POST /api/select"
            );
        }
    }

    if cli.headless {
        run_headless(app)
    } else {
        run_gui_mode(app, cli.no_tray)
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
    shutdown_cleanup(&app);
    Ok(())
}

// -----------------------------------------------------------------------------
// GUI run — eframe on main thread; audio is on the bg thread spawned
// in run() above.
// -----------------------------------------------------------------------------

#[cfg(windows)]
fn run_gui_mode(app: Arc<App>, no_tray: bool) -> Result<()> {
    // Run the GUI (blocks until window/tray exits).
    if let Err(e) = stream_to_speaker::gui::run(app.clone(), !no_tray) {
        warn!("GUI exited with error: {:#}", e);
        // Surface to the user — without this the process just
        // disappears from the screen (window never created) but
        // remains in the tray, looking like "no window opened at all".
        show_crash_dialog(&format!(
            "Stream To Speaker couldn't open its window:\n\n{:#}\n\nLog file: %LOCALAPPDATA%\\StreamToSpeaker\\stream-to-speaker.log",
            e
        ));
    }

    // Tell the audio loop to stop. We deliberately do NOT join() it:
    // it's blocked inside the kernel IOCTL (recv_packet) waiting for
    // the next audio packet, and joining would hang until the next
    // packet arrives — which on a paused / idle Sonos can be forever.
    // Process termination kills the thread cleanly; the kernel handle
    // is RAII-dropped via the SharedHandle Arc on exit.
    app.request_shutdown();
    shutdown_cleanup(&app);
    Ok(())
}

#[cfg(not(windows))]
fn run_gui_mode(_app: Arc<App>, _no_tray: bool) -> Result<()> {
    anyhow::bail!("GUI mode is Windows-only — pass --headless on this platform")
}

fn shutdown_cleanup(app: &Arc<App>) {
    info!("shutdown: tearing down active session");
    let final_session = app.session.lock().unwrap().take();
    if let Some(s) = final_session {
        s.stop();
    }
    info!("shutdown: complete");
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

    let advertise_ip = match cli.advertise_ip.as_deref() {
        Some(ip) => ip.to_string(),
        None => stream_to_speaker::app::default_advertise_ip()?,
    };

    let ssdp_iface = if cli.no_discovery {
        None
    } else {
        stream_to_speaker::app::pick_ssdp_iface(cli.advertise_ip.as_deref(), &cli.bind)
    };

    let discovery = if cli.no_discovery {
        None
    } else {
        let state = DiscoveryState::new();
        spawn_discovery(
            state.clone(),
            Duration::from_secs(cli.ssdp_interval * 60),
            ssdp_iface,
        );
        Some(state)
    };

    let airplay_discovery = if cli.no_airplay {
        None
    } else {
        let state = AirPlayDiscoveryState::new();
        if let Err(e) = spawn_airplay_discovery(state.clone(), ssdp_iface) {
            warn!("AirPlay discovery failed to start: {} (continuing without it)", e);
            None
        } else {
            Some(state)
        }
    };

    let stream_uri = format!("http://{}:{}/stream.raw", advertise_ip, cli.port);
    let callback_url = format!("http://{}:{}/gena", advertise_ip, cli.port);

    let config = AppConfig {
        stream_uri,
        callback_url,
        advertise_ip,
        bind: bind_socket,
        initial_buffer_ms: cli.initial_buffer_ms,
        silence_packets_threshold: cli.silence_packets_threshold,
        no_silence_injection: cli.no_silence_injection,
        no_discovery: cli.no_discovery,
        web_enabled: cli.web,
        ssdp_iface,
    };

    Ok(App::new(config, discovery, airplay_discovery))
}

// -----------------------------------------------------------------------------
// HTTP server setup
// -----------------------------------------------------------------------------

fn start_http(app: &Arc<App>) -> Result<u16> {
    let gena_callback = {
        let vsync = app.vsync.clone();
        let pusher = build_driver_volume_pusher();
        let pusher = pusher.map(Arc::new);
        Arc::new(move |path: &str, body: &str| {
            debug!("GENA NOTIFY on {}: {} bytes", path, body.len());
            if let Some(change) = parse_rendering_notify(body) {
                if let Some(v) = change.volume {
                    if let Some(level) = vsync.sonos_changed(v) {
                        // Set our node's dB to the linear position for this
                        // percent; Windows then shows the slider at the
                        // same %.
                        let mb = stream_to_speaker::volume_sync::sonos_to_millibels(level);
                        info!("speaker -> windows: volume {} (mb={})", level, mb);
                        if let Some(p) = pusher.as_ref() {
                            if let Err(e) = p.push(mb, false) {
                                warn!("failed to push volume to driver: {}", e);
                            }
                        }
                    }
                }
                if let Some(m) = change.mute {
                    info!("speaker -> driver: mute={}", m);
                    if let Some(p) = pusher.as_ref() {
                        if let Err(e) = p.push(0, m) {
                            warn!("failed to push mute to driver: {}", e);
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

    start_http_server(HttpServerConfig {
        bind: app.config.bind,
        hub: app.hub.clone(),
        gena_callback: Some(gena_callback),
        speaker_list: Some(speaker_list),
        speaker_select: Some(speaker_select),
        resync: Some(resync),
        latency_adjust: Some(latency_adjust),
        web_ui_enabled: Some(app.web_ui_enabled.clone()),
    })
}

// -----------------------------------------------------------------------------
// --list-speakers
// -----------------------------------------------------------------------------

fn cmd_list_speakers(cli: &Cli) -> Result<()> {
    if cli.no_discovery {
        return Err(anyhow!("--list-speakers and --no-discovery are mutually exclusive"));
    }
    let state = DiscoveryState::new();
    let ssdp_iface = stream_to_speaker::app::pick_ssdp_iface(cli.advertise_ip.as_deref(), &cli.bind);
    spawn_discovery(
        state.clone(),
        Duration::from_secs(cli.ssdp_interval * 60),
        ssdp_iface,
    );
    let renderers = picker::wait_for_first_discovery(&state, Duration::from_secs(5));
    let n = picker::print_speaker_list(&renderers);
    if n == 0 {
        eprintln!("(no speakers found within 5s — check that SSDP isn't blocked)");
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Source selection
// -----------------------------------------------------------------------------

fn build_source(cli: &Cli) -> Result<Box<dyn AudioSource>> {
    match cli.source {
        SourceKind::Sine => Ok(Box::new(SineSource::new())),
        SourceKind::WasapiLoopback => {
            #[cfg(windows)]
            {
                Ok(Box::new(WasapiLoopbackSource::new(cli.device.as_deref())?))
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("--source wasapi-loopback only works on Windows")
            }
        }
        SourceKind::Driver => {
            #[cfg(windows)]
            {
                use stream_to_speaker::ioctl_source::IoctlAudioSource;
                let src = IoctlAudioSource::open_audio_only()
                    .context("opening Stream-To-Speaker kernel driver (--source driver was specified)")?;
                Ok(Box::new(src))
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("--source driver only works on Windows")
            }
        }
        SourceKind::Auto => {
            #[cfg(windows)]
            {
                use stream_to_speaker::ioctl_source::IoctlAudioSource;
                match IoctlAudioSource::open_audio_only() {
                    Ok(s) => Ok(Box::new(s)),
                    Err(e) => {
                        info!("driver not present, falling back to WASAPI loopback: {}", e);
                        Ok(Box::new(WasapiLoopbackSource::new(cli.device.as_deref())?))
                    }
                }
            }
            #[cfg(not(windows))]
            {
                warn!("non-Windows host; using sine source for --source auto");
                Ok(Box::new(SineSource::new()))
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Driver event consumer + volume pusher
// -----------------------------------------------------------------------------

#[cfg(windows)]
fn spawn_driver_event_consumer(app: Arc<App>, cli: &Cli) {
    if cli.source == SourceKind::WasapiLoopback || cli.source == SourceKind::Sine {
        return;
    }
    use stream_to_speaker::ioctl_source::{DriverEvent, IoctlAudioSource};
    thread::Builder::new()
        .name("stream-to-speaker-driver-events".to_string())
        .spawn(move || {
            let src = match IoctlAudioSource::open() {
                Ok(s) => s,
                Err(e) => {
                    warn!("driver-event consumer: open failed: {}", e);
                    return;
                }
            };
            let rx = src.events();
            while let Ok(ev) = rx.recv() {
                if app.is_shutting_down() {
                    return;
                }
                match ev {
                    DriverEvent::VolumeChanged { level_millibels } => {
                        // Windows maps our hardware volume node's slider
                        // linearly across its dB range, so convert the dB
                        // back to a percent linearly to track it 1:1.
                        let level = stream_to_speaker::volume_sync::millibels_to_sonos(
                            level_millibels,
                        );
                        if let Some(level) = app.vsync.driver_changed(level) {
                            info!("windows -> speaker: volume {} (mb={})", level, level_millibels);
                            // Dispatch to whichever session kind is active;
                            // the ActiveSession enum hides SOAP-vs-RTSP so
                            // UPnP, AirPlay 1 and AirPlay 2 all get it.
                            let guard = app.session.lock().unwrap();
                            if let Some(s) = guard.as_ref() {
                                if let Err(e) = s.set_volume_pct(level) {
                                    warn!("set_volume failed: {}", e);
                                }
                            }
                        }
                    }
                    DriverEvent::MuteChanged { muted } => {
                        info!("driver -> speaker: mute={}", muted);
                        let guard = app.session.lock().unwrap();
                        if let Some(s) = guard.as_ref() {
                            if let Err(e) = s.set_mute(muted) {
                                warn!("set_mute failed: {}", e);
                            }
                        }
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
        })
        .ok();
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
        let s = self.src.lock().unwrap();
        s.push_volume(mb, muted)
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

fn install_signal_handler(app: Arc<App>) {
    if let Err(e) = ctrlc::set_handler(move || {
        if !app.is_shutting_down() {
            eprintln!("shutdown requested — cleaning up...");
            app.request_shutdown();
        }
    }) {
        warn!("could not install Ctrl-C handler: {}", e);
    }
}
