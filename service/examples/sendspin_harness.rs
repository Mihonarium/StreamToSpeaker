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
        _ => {
            eprintln!("usage: sendspin_harness source|play|discover ...");
            std::process::exit(2);
        }
    }
}
