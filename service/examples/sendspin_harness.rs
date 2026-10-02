//! Cross-platform harness for the Sendspin roles without the Windows audio
//! path: a 440 Hz sine is fed through the same `StreamHub` the app uses.
//!
//! ```text
//! cargo run --example sendspin_harness -- source [--name NAME] [--port N] [--store FILE]
//!     Run the source client (Music Assistant "Sendspin Source" input).
//!     Prints the pairing token; shows the pairing code when one is offered.
//! cargo run --example sendspin_harness -- play ws://HOST:PORT/sendspin [--store FILE] [--seconds N]
//!     Act as the server and stream the sine to one player.
//! cargo run --example sendspin_harness -- discover [--seconds N]
//!     List Sendspin players found via mDNS.
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use stream_to_speaker::http_server::{samples_to_l16_be_bytes, PcmFrame, StreamHub};
use stream_to_speaker::sendspin::store::SendspinConfig;
use stream_to_speaker::sendspin::SendspinStore;

/// JSON-file-backed store so pairings survive harness restarts.
struct FileStore {
    path: Option<PathBuf>,
    cfg: Mutex<SendspinConfig>,
}

impl FileStore {
    fn open(path: Option<PathBuf>) -> Self {
        let cfg = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, cfg: Mutex::new(cfg) }
    }
}

impl SendspinStore for FileStore {
    fn snapshot(&self) -> SendspinConfig {
        self.cfg.lock().unwrap().clone()
    }

    fn update(&self, f: &mut dyn FnMut(&mut SendspinConfig) -> bool) {
        let mut g = self.cfg.lock().unwrap();
        if f(&mut g) {
            if let Some(p) = &self.path {
                let _ = std::fs::write(p, serde_json::to_string_pretty(&*g).unwrap());
            }
        }
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Publish a 440 Hz sine (-12 dBFS) as 10 ms PcmFrames, paced in real time.
fn spawn_sine(hub: Arc<StreamHub>) {
    std::thread::spawn(move || {
        let mut phase = 0f32;
        let step = 2.0 * std::f32::consts::PI * 440.0 / 44_100.0;
        let mut next = Instant::now();
        loop {
            let mut samples = Vec::with_capacity(882);
            for _ in 0..441 {
                let s = (phase.sin() * 8192.0) as i16;
                samples.push(s);
                samples.push(s);
                phase = (phase + step) % (2.0 * std::f32::consts::PI);
            }
            hub.publish(PcmFrame(Arc::new(samples_to_l16_be_bytes(&samples))));
            next += Duration::from_millis(10);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    });
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let store = Arc::new(FileStore::open(arg(&args, "--store").map(PathBuf::from)));
    let hub = StreamHub::new();
    match args.first().map(String::as_str) {
        Some("source") => {
            spawn_sine(hub.clone());
            let opts = stream_to_speaker::sendspin::source::SourceOptions {
                name: arg(&args, "--name").unwrap_or_else(|| "Harness Source".into()),
                port: arg(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8928),
                software_version: env!("CARGO_PKG_VERSION").into(),
            };
            let svc = stream_to_speaker::sendspin::source::SourceService::start(opts, hub, store).expect("start");
            println!("PORT {}", svc.port());
            println!("TOKEN {}", svc.pairing_token());
            let mut last_code = None;
            let mut last_state = None;
            loop {
                let code = svc.pairing_code();
                if code != last_code {
                    if let Some((c, server)) = &code {
                        println!("CODE {} {}", c, server);
                    }
                    last_code = code;
                }
                let st = svc.state();
                if Some(&st) != last_state.as_ref() {
                    println!("STATE {:?}", st);
                    last_state = Some(st);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Some("play") => {
            use stream_to_speaker::sendspin::server::{PairingInput, PlayerSession, PlayerSessionConfig, SendspinRenderer, StartError};
            let url = args.get(1).expect("url");
            let rest = url.strip_prefix("ws://").expect("ws:// url");
            let (hostport, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{}", p))).unwrap_or((rest, "/sendspin".into()));
            let addr: std::net::SocketAddr = hostport.parse().expect("ip:port");
            let renderer = SendspinRenderer {
                instance: "harness-target".into(),
                friendly_name: "target".into(),
                ip: addr.ip(),
                port: addr.port(),
                path,
            };
            spawn_sine(hub.clone());
            let pairing = if let Some(t) = arg(&args, "--token") {
                Some(PairingInput::Token(t))
            } else if let Some(code) = arg(&args, "--static-code") {
                Some(PairingInput::Code { dynamic: false, ask: Box::new(move || Some(code.clone())) })
            } else if args.iter().any(|a| a == "--dynamic-code") {
                Some(PairingInput::Code {
                    dynamic: true,
                    ask: Box::new(|| {
                        println!("ENTER-CODE");
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line).ok()?;
                        Some(line.trim().to_string())
                    }),
                })
            } else {
                None
            };
            let seconds: u64 = arg(&args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(10);
            let np: stream_to_speaker::sendspin::server::NowPlayingFn =
                Arc::new(|| Some(("Harness Tone".to_string(), "Stream To Speaker".to_string(), "Tests".to_string())));
            let cfg = PlayerSessionConfig {
                renderer,
                server_name: "Harness Server".into(),
                store,
                samples_rx: hub.subscribe(),
                extra_latency_ms: arg(&args, "--extra-ms").and_then(|s| s.parse().ok()).unwrap_or(0),
                pairing,
                now_playing: Some(np),
                connect_timeout: Duration::from_secs(5),
            };
            match PlayerSession::start(cfg) {
                Ok(s) => {
                    println!("PLAYING {:?} volume={:?}", s.format(), s.volume());
                    if let Some(v) = arg(&args, "--volume").and_then(|v| v.parse().ok()) {
                        s.set_volume_pct(v).expect("volume");
                        println!("VOLUME-SET {}", v);
                    }
                    let until = Instant::now() + Duration::from_secs(seconds);
                    while Instant::now() < until && !s.is_dead() {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    println!("STOP dead={}", s.is_dead());
                    s.stop();
                }
                Err(StartError::NeedsPairing { methods, lost_credential }) => {
                    println!("NEEDS-PAIRING {:?} lost={}", methods, lost_credential);
                    std::process::exit(3);
                }
                Err(e) => {
                    println!("ERROR {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some("discover") => {
            let st = stream_to_speaker::sendspin::server::SendspinDiscoveryState::new();
            stream_to_speaker::sendspin::server::spawn_discovery(st.clone()).expect("discovery");
            let seconds: u64 = arg(&args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(5);
            std::thread::sleep(Duration::from_secs(seconds));
            for r in st.renderers() {
                println!("FOUND {} {} {}", r.stable_id(), r.friendly_name, r.url());
            }
        }
        _ => {
            eprintln!("usage: sendspin_harness source|play|discover ...");
            std::process::exit(2);
        }
    }
}
