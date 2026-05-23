//! Stream To Speaker — main binary entry point.
//!
//! Wires the audio source, silence detector, HTTP server, SSDP discovery,
//! UPnP control plane, GENA event subscription, interactive speaker
//! picker, and bidirectional volume sync into one running service. Audio
//! path runs on a dedicated MMCSS thread; HTTP and control planes each
//! get their own thread pools.
//!
//! When no `--player` is given and stdin is a terminal, the user gets a
//! numbered prompt listing every discovered speaker (swyh-rs-style, but
//! text-mode). In non-interactive mode (e.g. running as a Windows
//! service) we fall back to "first discovered". Speakers can also be
//! switched at runtime via `POST /api/select`.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use crossbeam_channel::{select, tick};
use log::{debug, error, info, warn};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use stream_to_speaker::audio_source::{AudioSource, PACKET_FLAG_STREAM_RESTART};
use stream_to_speaker::gena::{parse_rendering_notify, GenaManager};
use stream_to_speaker::http_server::{
    samples_to_l16_be_bytes, start_http_server, HttpServerConfig, LatencyAdjustCallback, PcmFrame,
    ResyncCallback, SpeakerInfo, SpeakerListCallback, SpeakerSelectCallback, StreamHub,
};
use stream_to_speaker::picker;
use stream_to_speaker::silence::{SilenceDetector, DEFAULT_QUIESCENT_AFTER_PACKETS};
use stream_to_speaker::sine_source::SineSource;
use stream_to_speaker::ssdp::{spawn_discovery, DiscoveryState, Renderer};
use stream_to_speaker::upnp;
use stream_to_speaker::volume_sync::VolumeSync;
use stream_to_speaker::PRODUCT_NAME;
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

    /// TCP port to serve `/stream.raw` on.
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

    /// Interval between SSDP re-discoveries (minutes).
    #[arg(long, default_value_t = 5)]
    ssdp_interval: u64,

    /// Initial buffer hint sent to the speaker in DIDL metadata (ms).
    #[arg(long, default_value_t = 50)]
    initial_buffer_ms: u32,

    /// Skip SSDP discovery; serve HTTP only.
    #[arg(long, default_value_t = false)]
    no_discovery: bool,

    /// Disable silence injection (will send literal zeros during silence).
    #[arg(long, default_value_t = false)]
    no_silence_injection: bool,

    /// Number of consecutive silent packets before entering quiescence.
    #[arg(long, default_value_t = DEFAULT_QUIESCENT_AFTER_PACKETS)]
    silence_packets_threshold: u32,

    /// Bind address for our HTTP server. The speaker must be able to
    /// reach us here. Default 0.0.0.0; consider setting to the LAN IP.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Public IP to advertise to the speaker in the stream URI. Required
    /// when bind is 0.0.0.0. Defaults to the first non-loopback IPv4.
    #[arg(long)]
    advertise_ip: Option<String>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info")]
    log_level: String,

    // ---- Latency / clock-drift compensation ------------------------------

    /// Wall-clock ms between silence packets during silence-injection
    /// mode. Default 10 = real-time (matches 441 frames @ 44.1 kHz).
    /// Set > 10 to send slower than real-time during silence, draining
    /// the speaker's prebuffer so post-pause latency is smaller. Values
    /// too high (e.g., > 30) risk underrun on Sonos.
    #[arg(long, default_value_t = 10)]
    silence_pace_ms: u64,

    /// Rate-fudge compensation in parts-per-million for hardware
    /// clock-skew between the Windows TSC and the speaker's audio
    /// crystal. Positive = over-produce (insert duplicated frames) to
    /// match a speaker whose crystal runs faster than the host's, i.e.
    /// the speaker is draining its buffer faster than we fill it. Try
    /// +50 to +200 if you see the Sonos buffer slowly running out;
    /// negative to drop frames if the buffer overflows. 0 disables.
    #[arg(long, default_value_t = 0)]
    rate_fudge_ppm: i32,

    /// Maximum frames added or dropped per packet when applying a
    /// runtime latency-adjust request (POST /api/latency/adjust). At
    /// 44.1 kHz, 4 frames is 0.09 ms — well below the audibility
    /// threshold per packet. A 50 ms adjust at step=4 spreads across
    /// ~550 audio packets (~1.2 s) so it's smooth; raise for a faster
    /// snap-to-target at the cost of a more audible click.
    #[arg(long, default_value_t = 4)]
    latency_adjust_step_frames: u32,
}

fn main() {
    let cli = Cli::parse();

    let mut builder = env_logger::Builder::from_default_env();
    builder
        .filter_level(cli.log_level.parse().unwrap_or(log::LevelFilter::Info))
        .format_timestamp_millis()
        .init();

    if let Err(e) = run(cli) {
        error!("fatal: {:#}", e);
        std::process::exit(1);
    }
}

// -----------------------------------------------------------------------------
// Renderer session — the active speaker + its GENA subscription.
// -----------------------------------------------------------------------------

/// What we keep track of for a single active speaker.
struct RendererSession {
    renderer: Renderer,
    gena: Arc<GenaManager>,
    /// The stream URI we're advertising to this speaker — same for every
    /// speaker on a given run, but cached here for `start`/`stop` symmetry.
    #[allow(dead_code)]
    stream_uri: String,
}

type SharedSession = Arc<Mutex<Option<RendererSession>>>;

fn start_session(
    renderer: Renderer,
    stream_uri: &str,
    didl: &str,
    callback_url: &str,
) -> Result<RendererSession> {
    info!(
        "targeting speaker: {} ({})",
        renderer.friendly_name, renderer.ip
    );
    // Stop first to clear any "Transport Locked" (Sonos error 705) state
    // left over from a previous session — swyh-rs does this. Best-effort:
    // a "not playing" Stop on Sonos returns 701, which we ignore.
    let _ = upnp::stop(&renderer.av_transport_control_url);
    // Tiny pause before SetAVTransportURI — swyh-rs uses 100 ms; Sonos's
    // AVTransport state machine sometimes rejects back-to-back commands.
    std::thread::sleep(std::time::Duration::from_millis(100));
    upnp::set_av_transport_uri(&renderer.av_transport_control_url, stream_uri, didl)
        .context("SetAVTransportURI")?;
    std::thread::sleep(std::time::Duration::from_millis(100));
    upnp::play(&renderer.av_transport_control_url).context("Play")?;

    let gena = GenaManager::new(callback_url.to_string());
    match gena.subscribe(&renderer.rendering_control_event_url) {
        Ok(_) => {
            gena.clone().spawn_renewer();
        }
        Err(e) => {
            warn!(
                "GENA subscribe failed (continuing without volume sync from speaker): {}",
                e
            );
        }
    }

    Ok(RendererSession {
        renderer,
        gena,
        stream_uri: stream_uri.to_string(),
    })
}

fn stop_session(session: &RendererSession) {
    // Best-effort tear-down; we don't fail switching if the old speaker
    // is already unreachable.
    session.gena.unsubscribe();
    if let Err(e) = upnp::stop(&session.renderer.av_transport_control_url) {
        debug!("UPnP Stop on {} failed: {}", session.renderer.friendly_name, e);
    }
}

// -----------------------------------------------------------------------------
// run()
// -----------------------------------------------------------------------------

fn run(cli: Cli) -> Result<()> {
    info!("{} v{}", PRODUCT_NAME, env!("CARGO_PKG_VERSION"));

    // 1. Special-case: --list-speakers prints and exits, no service needed.
    if cli.list_speakers {
        return cmd_list_speakers(&cli);
    }

    // 2. Audio source.
    let mut source: Box<dyn AudioSource> = build_source(&cli)?;
    info!("audio source: {}", source.name());

    // 3. HTTP server + stream hub.
    let hub = StreamHub::new();
    let bind_addr: IpAddr = cli
        .bind
        .parse()
        .with_context(|| format!("parsing --bind {}", cli.bind))?;
    let bind_socket = SocketAddr::new(bind_addr, cli.port);

    // 4. SSDP discovery.
    let discovery = if cli.no_discovery {
        None
    } else {
        let state = DiscoveryState::new();
        // Reuse advertise_ip / bind for the multicast egress interface.
        // Falls back to OS-pick when neither is a specific IPv4.
        let ssdp_iface = pick_ssdp_iface(&cli);
        spawn_discovery(state.clone(), Duration::from_secs(cli.ssdp_interval * 60), ssdp_iface);
        Some(state)
    };

    // 5. Volume sync (shared between driver-event handler and GENA notify).
    let vsync = Arc::new(VolumeSync::new());

    // 6. Shared "current speaker" state used by /api/select and by the
    //    driver-event consumer (which needs to know where to send volume
    //    changes to).
    let session: SharedSession = Arc::new(Mutex::new(None));

    // 6b. Latency-adjust counter. Positive value = we owe Sonos N fewer
    // frames (drop them) → reduces accumulated latency. Negative = we
    // owe Sonos N more frames (duplicate them) → increases latency
    // (used to back off if a drain has gone too far). The audio loop
    // applies up to --latency-adjust-step-frames per packet so the
    // adjustment is spread out and the artifact is below audibility.
    let drain_frames = Arc::new(std::sync::atomic::AtomicI64::new(0));

    // 7. Stream URI & DIDL.
    let advertise_ip = match cli.advertise_ip.as_deref() {
        Some(ip) => ip.to_string(),
        None => default_advertise_ip()?,
    };

    // 8. GENA notify callback.
    let vsync_for_cb = vsync.clone();
    let driver_volume_pusher = build_driver_volume_pusher(&cli)?;
    let gena_callback: stream_to_speaker::http_server::GenaNotifyCallback =
        Arc::new(move |path: &str, body: &str| {
            debug!("GENA NOTIFY on {}: {} bytes", path, body.len());
            if let Some(change) = parse_rendering_notify(body) {
                if let Some(v) = change.volume {
                    if let Some(mb) = vsync_for_cb.sonos_changed(v) {
                        info!("speaker -> driver: volume {} (mb={})", v, mb);
                        if let Some(p) = driver_volume_pusher.as_ref() {
                            if let Err(e) = p.push(mb, false) {
                                warn!("failed to push volume to driver: {}", e);
                            }
                        }
                    }
                }
                if let Some(m) = change.mute {
                    info!("speaker -> driver: mute={}", m);
                    if let Some(p) = driver_volume_pusher.as_ref() {
                        if let Err(e) = p.push(0, m) {
                            warn!("failed to push mute to driver: {}", e);
                        }
                    }
                }
            }
        });

    // 9. Speaker list & select callbacks (only useful when discovery is on).
    let speaker_list: Option<SpeakerListCallback> = discovery.as_ref().map(|d| {
        let d = d.clone();
        let session_for_list = session.clone();
        Arc::new(move || {
            let active_id = session_for_list
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.renderer.stable_id());
            d.renderers()
                .into_iter()
                .map(|r| {
                    let id = r.stable_id();
                    let active = active_id.as_deref() == Some(id.as_str());
                    SpeakerInfo {
                        id,
                        friendly_name: r.friendly_name,
                        ip: r.ip.to_string(),
                        active,
                    }
                })
                .collect()
        }) as Arc<dyn Fn() -> Vec<SpeakerInfo> + Send + Sync>
    });

    // Need a couple of values inside the select callback. Build them up
    // front and clone into the closure.
    let stream_uri_template = (advertise_ip.clone(), cli.port);
    let didl_template = (cli.initial_buffer_ms,);

    let speaker_select: Option<SpeakerSelectCallback> =
        discovery.as_ref().map(|d| {
            let discovery = d.clone();
            let session = session.clone();
            let (adv_ip, port) = stream_uri_template.clone();
            let (buffer_ms,) = didl_template;
            Arc::new(move |id: &str| -> Result<(), String> {
                let Some(new_r) = discovery.find_by_id(id) else {
                    return Err(format!("no speaker with id {:?}", id));
                };
                let stream_uri = format!("http://{}:{}/stream.raw", adv_ip, port);
                let didl =
                    upnp::didl_lite_metadata(&stream_uri, PRODUCT_NAME, buffer_ms);
                let callback_url = format!("http://{}:{}/gena", adv_ip, port);
                let new_session = start_session(new_r, &stream_uri, &didl, &callback_url)
                    .map_err(|e| format!("{:#}", e))?;
                let mut guard = session.lock().unwrap();
                if let Some(old) = guard.take() {
                    drop(guard); // release before potentially-slow Stop
                    stop_session(&old);
                    guard = session.lock().unwrap();
                }
                *guard = Some(new_session);
                Ok(())
            }) as Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>
        });

    // 9b. Resync callback: stop + play on the active speaker. Forces
    // Sonos to discard its current prebuffer; on the next Play it picks
    // a fresh (typically smaller) prebuffer, trimming accumulated latency.
    let resync: Option<ResyncCallback> = {
        let session_for_resync = session.clone();
        Some(Arc::new(move || -> Result<(), String> {
            let renderer = {
                let guard = session_for_resync.lock().unwrap();
                guard.as_ref().map(|s| s.renderer.clone())
            };
            let Some(r) = renderer else {
                return Err("no active speaker".to_string());
            };
            info!("resync: UPnP Stop + Play on {}", r.friendly_name);
            if let Err(e) = upnp::stop(&r.av_transport_control_url) {
                // Sonos 701 ("not playing") is fine; warn on anything else.
                debug!("resync stop: {}", e);
            }
            std::thread::sleep(Duration::from_millis(100));
            upnp::play(&r.av_transport_control_url)
                .map_err(|e| format!("play failed: {:#}", e))
        }) as Arc<dyn Fn() -> Result<(), String> + Send + Sync>)
    };

    // 9c. Latency-adjust callback. ms > 0 = trim Sonos's latency by N ms
    // (drop frames over time); ms < 0 = pad by N ms (duplicate frames).
    let latency_adjust: Option<LatencyAdjustCallback> = {
        let drain = drain_frames.clone();
        Some(Arc::new(move |ms: i32| -> i64 {
            // 44.1 frames per ms at 44.1 kHz. Use i64 math for the cast.
            let frames = (ms as i64) * (stream_to_speaker::WIRE_SAMPLE_RATE as i64) / 1000;
            let new_val = drain.fetch_add(frames, std::sync::atomic::Ordering::AcqRel) + frames;
            info!(
                "latency adjust: {:+} ms ({:+} frames) → pending {} frames ({} ms)",
                ms,
                frames,
                new_val,
                new_val / 44, // approx ms
            );
            new_val
        }) as Arc<dyn Fn(i32) -> i64 + Send + Sync>)
    };

    let actual_port = start_http_server(HttpServerConfig {
        bind: bind_socket,
        hub: hub.clone(),
        gena_callback: Some(gena_callback),
        speaker_list: speaker_list.clone(),
        speaker_select: speaker_select.clone(),
        resync,
        latency_adjust,
    })?;

    let stream_uri = format!("http://{}:{}/stream.raw", advertise_ip, actual_port);
    info!("stream URI: {}", stream_uri);
    info!("web UI: http://{}:{}/", advertise_ip, actual_port);

    // 10. Resolve initial speaker.
    if let Some(state) = discovery.as_ref() {
        let r = picker::resolve(state, cli.player.as_deref(), !cli.no_interactive)?;
        if let Some(renderer) = r {
            let didl = upnp::didl_lite_metadata(&stream_uri, PRODUCT_NAME, cli.initial_buffer_ms);
            let callback_url = format!("http://{}:{}/gena", advertise_ip, actual_port);
            match start_session(renderer, &stream_uri, &didl, &callback_url) {
                Ok(s) => {
                    *session.lock().unwrap() = Some(s);
                }
                Err(e) => {
                    warn!("starting initial session failed: {:#}", e);
                }
            }
        } else if !cli.no_discovery {
            warn!(
                "no speaker selected; serving stream at {}/stream.raw — use the web UI at http://{}:{}/ or POST /api/select to pick one",
                stream_uri, advertise_ip, actual_port
            );
        }
    }

    // 11. Driver-event consumer.
    // Tracks whether the Windows audio engine is in KSSTATE_RUN. When
    // not, the audio loop emits paced silence directly instead of
    // blocking on recv_packet — keeps the Sonos HTTP stream alive.
    let stream_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
    #[cfg(windows)]
    spawn_driver_event_consumer(&cli, vsync.clone(), session.clone(), stream_active.clone());

    // 11b. Ctrl-C / SIGTERM handler. Sets the shutdown flag; the audio
    // loop polls it once per iteration and breaks. On the way out we
    // send a UPnP Stop to the active speaker so Sonos doesn't keep
    // "Stream To Speaker" stuck as the source.
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let shutdown_handler = shutdown.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            // Idempotent: a second Ctrl-C just confirms the first.
            if !shutdown_handler.swap(true, std::sync::atomic::Ordering::SeqCst) {
                eprintln!("shutdown requested — sending UPnP Stop and cleaning up...");
            }
        }) {
            warn!("could not install Ctrl-C handler: {} (use Task Manager / kill to stop)", e);
        }
    }

    // 12. Audio loop.
    let mut silence = SilenceDetector::new(cli.silence_packets_threshold, !cli.no_silence_injection);
    let mut last_packet_log = std::time::Instant::now();
    let mut packet_count: u64 = 0;
    // Silence-injection pacing. The packet is always 10 ms (441 frames)
    // of audio; silence_pace_ms controls the wall-clock interval between
    // them. Default 10 = real-time. >10 sends slower than real-time, so
    // Sonos's buffer drains during pauses → smaller post-pause latency.
    const SILENCE_FRAMES: usize = stream_to_speaker::WIRE_SAMPLE_RATE as usize / 100;
    let silence_period = Duration::from_millis(cli.silence_pace_ms.max(1));
    if cli.silence_pace_ms != 10 {
        info!(
            "silence-injection pacing: {} ms wall-clock per 10 ms silence packet ({})",
            cli.silence_pace_ms,
            match cli.silence_pace_ms.cmp(&10) {
                std::cmp::Ordering::Greater => "drains speaker prebuffer between sessions",
                std::cmp::Ordering::Less => "WARNING: faster than real-time, will overflow",
                std::cmp::Ordering::Equal => "real-time",
            }
        );
    }
    let mut next_silence_deadline = std::time::Instant::now();
    // Holds the error that caused the audio loop to exit (if any) so we
    // can still run the UPnP-Stop cleanup before returning it.
    let mut loop_error: Option<anyhow::Error> = None;
    // Rate-fudge state: accumulate fractional-frame compensation. Each
    // real packet adds `pkt_frames * rate_fudge_ppm / 1e6` to the
    // accumulator; once it crosses 1.0 we duplicate / drop a frame to
    // emit the correction. Linear interpolation would be cleaner but a
    // single sample at hundreds-of-ppm is below the audibility threshold.
    let fudge_ppm = cli.rate_fudge_ppm;
    let mut fudge_accum: f64 = 0.0;
    if fudge_ppm != 0 {
        info!(
            "rate-fudge compensation: {:+} ppm ({} crystal-skew)",
            fudge_ppm,
            if fudge_ppm > 0 { "over-produce (duplicate frames)" } else { "under-produce (drop frames)" }
        );
    }

    loop {
        if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let active = stream_active.load(std::sync::atomic::Ordering::Acquire);
        let mut pkt = if active {
            match source.recv_packet() {
                Ok(p) => p,
                Err(e) => {
                    if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    error!("audio source error: {} — exiting", e);
                    loop_error = Some(e);
                    break;
                }
            }
        } else {
            // Stream is stopped. Emit paced silence so the Sonos HTTP
            // connection doesn't time out. Use a deadline so we stay
            // close to real-time even if a previous iteration overran.
            let now = std::time::Instant::now();
            if next_silence_deadline > now {
                std::thread::sleep(next_silence_deadline - now);
            }
            next_silence_deadline = next_silence_deadline.max(now) + silence_period;
            stream_to_speaker::audio_source::AudioPacket::silence(SILENCE_FRAMES)
        };

        if pkt.flags & PACKET_FLAG_STREAM_RESTART != 0 {
            info!("stream restart from source (new session)");
        }

        if pkt.sample_rate != stream_to_speaker::WIRE_SAMPLE_RATE
            || pkt.channels != stream_to_speaker::WIRE_CHANNELS
        {
            warn!(
                "unexpected source format: rate={} ch={}; expected {}/{}",
                pkt.sample_rate,
                pkt.channels,
                stream_to_speaker::WIRE_SAMPLE_RATE,
                stream_to_speaker::WIRE_CHANNELS
            );
            continue;
        }

        // The IOCTL source signals "stream stop drain" by returning an
        // all-zeros 10 ms packet (see ioctl_source.rs). Switch to the
        // silence-injection branch immediately so we don't re-issue a
        // blocking IOCTL on the next iteration — saves one round-trip
        // worth of latency before silence starts flowing to Sonos.
        if active && pkt.samples.iter().all(|s| *s == 0)
            && pkt.flags & stream_to_speaker::audio_source::PACKET_FLAG_HINT_SILENT != 0
        {
            stream_active.store(false, std::sync::atomic::Ordering::Release);
            next_silence_deadline = std::time::Instant::now() + silence_period;
            debug!("audio: stream-stop drain received; emitting silence until StreamStart");
        }

        silence.process(&mut pkt);

        // Apply rate-fudge: insert / drop a frame when the accumulator
        // crosses ±1.0. Channels=2, so each frame is a pair of i16s.
        if fudge_ppm != 0 && !pkt.samples.is_empty() {
            let frames_in_pkt = pkt.samples.len() / pkt.channels as usize;
            fudge_accum += (frames_in_pkt as f64) * (fudge_ppm as f64) * 1e-6;
            if fudge_accum >= 1.0 {
                fudge_accum -= 1.0;
                // Duplicate the final frame. One extra frame at 100 ppm
                // is a click every ~10 s — well below the noise floor.
                let ch = pkt.channels as usize;
                let n = pkt.samples.len();
                if n >= ch {
                    let mut last = Vec::with_capacity(ch);
                    last.extend_from_slice(&pkt.samples[n - ch..]);
                    pkt.samples.extend_from_slice(&last);
                }
            } else if fudge_accum <= -1.0 {
                fudge_accum += 1.0;
                // Drop the final frame.
                let ch = pkt.channels as usize;
                let n = pkt.samples.len();
                if n >= ch {
                    pkt.samples.truncate(n - ch);
                }
            }
        }

        // Apply pending latency adjust (drain_frames): at most
        // latency_adjust_step_frames per packet, signed.
        if !pkt.samples.is_empty() {
            let pending = drain_frames.load(std::sync::atomic::Ordering::Acquire);
            if pending != 0 {
                let ch = pkt.channels as usize;
                let max_step = cli.latency_adjust_step_frames.max(1) as i64;
                let step = pending.signum() * pending.abs().min(max_step);
                if step > 0 {
                    // Drop `step` frames from the end of this packet.
                    let drop_samples = (step as usize) * ch;
                    let new_len = pkt.samples.len().saturating_sub(drop_samples);
                    pkt.samples.truncate(new_len);
                    drain_frames.fetch_sub(step, std::sync::atomic::Ordering::AcqRel);
                } else if step < 0 && pkt.samples.len() >= ch {
                    // Insert |step| duplicates of the last frame.
                    let last_frame_start = pkt.samples.len() - ch;
                    let last_frame: Vec<i16> = pkt.samples[last_frame_start..].to_vec();
                    for _ in 0..(-step) {
                        pkt.samples.extend_from_slice(&last_frame);
                    }
                    drain_frames.fetch_sub(step, std::sync::atomic::Ordering::AcqRel);
                }
            }
        }

        let bytes = samples_to_l16_be_bytes(&pkt.samples);
        hub.publish(PcmFrame(Arc::new(bytes)));

        packet_count += 1;
        if last_packet_log.elapsed() >= Duration::from_secs(10) {
            debug!(
                "{} packets streamed, {} subscriber(s)",
                packet_count,
                hub.subscriber_count()
            );
            last_packet_log = std::time::Instant::now();
        }

        // Defensive: keep references alive for cfg(not(windows)) build.
        let _ = (vsync.clone(), &cli);
        std::hint::spin_loop();
    }

    // Graceful shutdown: send UPnP Stop on the active speaker (so Sonos
    // doesn't keep showing us as the source with a stale source name)
    // and tear down GENA. Best-effort — network or speaker errors here
    // are logged but don't fail the exit.
    info!("shutdown: cleaning up");
    let final_session = session.lock().unwrap().take();
    if let Some(s) = final_session {
        stop_session(&s);
    }
    info!("shutdown: complete");
    match loop_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// -----------------------------------------------------------------------------
// --list-speakers
// -----------------------------------------------------------------------------

fn cmd_list_speakers(cli: &Cli) -> Result<()> {
    if cli.no_discovery {
        return Err(anyhow!("--list-speakers and --no-discovery are mutually exclusive"));
    }
    let state = DiscoveryState::new();
    let ssdp_iface = pick_ssdp_iface(cli);
    spawn_discovery(state.clone(), Duration::from_secs(cli.ssdp_interval * 60), ssdp_iface);
    // Wait up to 5s for the first sweep.
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
                        info!(
                            "driver not present, falling back to WASAPI loopback: {}",
                            e
                        );
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
// Driver event consumer (volume change from Windows mixer -> speaker)
// -----------------------------------------------------------------------------

#[cfg(windows)]
fn spawn_driver_event_consumer(
    cli: &Cli,
    vsync: Arc<VolumeSync>,
    session: SharedSession,
    stream_active: Arc<std::sync::atomic::AtomicBool>,
) {
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
                    warn!("driver-event consumer: second handle failed: {}", e);
                    return;
                }
            };
            let rx = src.events();
            while let Ok(ev) = rx.recv() {
                match ev {
                    DriverEvent::VolumeChanged { level_millibels } => {
                        if let Some(level) = vsync.driver_changed(level_millibels) {
                            info!("driver -> speaker: volume {} (mb={})", level, level_millibels);
                            if let Some(r) = current_renderer(&session) {
                                if let Err(e) = upnp::set_volume(&r.rendering_control_control_url, level) {
                                    warn!("upnp set_volume failed: {}", e);
                                }
                            }
                        }
                    }
                    DriverEvent::MuteChanged { muted } => {
                        info!("driver -> speaker: mute={}", muted);
                        if let Some(r) = current_renderer(&session) {
                            if let Err(e) = upnp::set_mute(&r.rendering_control_control_url, muted) {
                                warn!("upnp set_mute failed: {}", e);
                            }
                        }
                    }
                    DriverEvent::StreamStart => {
                        info!("driver: stream start");
                        stream_active.store(true, std::sync::atomic::Ordering::Release);
                    }
                    DriverEvent::StreamStop => {
                        info!("driver: stream stop");
                        stream_active.store(false, std::sync::atomic::Ordering::Release);
                    }
                    DriverEvent::FormatChange { sample_rate, bits_per_sample, channels } => {
                        info!("driver: format change to {}/{}-bit/{}ch", sample_rate, bits_per_sample, channels);
                    }
                }
            }
        })
        .ok();
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn spawn_driver_event_consumer(
    _cli: &Cli,
    _vsync: Arc<VolumeSync>,
    _session: SharedSession,
    _stream_active: Arc<std::sync::atomic::AtomicBool>,
) {}

fn current_renderer(session: &SharedSession) -> Option<Renderer> {
    session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.renderer.clone())
}

// -----------------------------------------------------------------------------
// Driver volume pusher
// -----------------------------------------------------------------------------

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

fn build_driver_volume_pusher(cli: &Cli) -> Result<Option<Arc<dyn DriverVolumePush>>> {
    if cli.source == SourceKind::WasapiLoopback || cli.source == SourceKind::Sine {
        return Ok(None);
    }
    #[cfg(windows)]
    {
        use stream_to_speaker::ioctl_source::IoctlAudioSource;
        match IoctlAudioSource::open_audio_only() {
            Ok(s) => Ok(Some(Arc::new(IoctlPusher {
                src: std::sync::Mutex::new(s),
            }))),
            Err(e) => {
                if cli.source == SourceKind::Driver {
                    return Err(e);
                }
                debug!("no driver for volume pusher: {}", e);
                Ok(None)
            }
        }
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Resolve the IPv4 interface to use for SSDP multicast egress. Prefers
/// `--advertise-ip`, then `--bind` if it's a specific IPv4 (not 0.0.0.0),
/// otherwise falls back to the route-derived default via the UDP-connect
/// trick. Returns None to let the OS pick if even that fails.

/// Resolve the IPv4 interface to use for SSDP multicast egress. Prefers
/// `--advertise-ip`, then `--bind` if it's a specific IPv4 (not 0.0.0.0),
/// otherwise falls back to the route-derived default via the UDP-connect
/// trick. Returns None to let the OS pick if even that fails.
fn pick_ssdp_iface(cli: &Cli) -> Option<Ipv4Addr> {
    if let Some(s) = cli.advertise_ip.as_deref() {
        if let Ok(IpAddr::V4(v4)) = s.parse() {
            return Some(v4);
        }
    }
    if let Ok(IpAddr::V4(v4)) = cli.bind.parse::<IpAddr>() {
        if !v4.is_unspecified() {
            return Some(v4);
        }
    }
    if let Ok(s) = default_advertise_ip() {
        if let Ok(IpAddr::V4(v4)) = s.parse() {
            return Some(v4);
        }
    }
    None
}

fn default_advertise_ip() -> Result<String> {
    use std::net::UdpSocket;
    let s = UdpSocket::bind("0.0.0.0:0").context("binding udp for ip discovery")?;
    if s.connect("8.8.8.8:53").is_ok() {
        if let Ok(addr) = s.local_addr() {
            return Ok(addr.ip().to_string());
        }
    }
    Err(anyhow!("could not determine local IP; pass --advertise-ip"))
}

#[allow(dead_code)]
fn _selector_warmup() {
    let (_t, r) = crossbeam_channel::bounded::<()>(0);
    let ticker = tick(Duration::from_secs(1));
    let _ = select! {
        recv(r) -> _ => 0,
        recv(ticker) -> _ => 0,
    };
}
