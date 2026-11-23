//! Draws scenes of the app's UI to PNG files, without a window.
//!
//!     cargo run --example ui_preview -- --out DIR [--fonts DIR]
//!         [--theme light,dark] [--scale 100,150] [--time SECONDS] [--list]
//!         [SCENE | PREFIX* ...]
//!
//! Each picture is written as `DIR/<scene>.<theme>.<scale>.png`. `--fonts`
//! names a folder holding the fonts by their Windows file names
//! (`segoeui.ttf`, `seguisb.ttf`, …; see `ui::preview::fonts_from_dir`);
//! without it, text is drawn with egui's own fonts and is not the size it
//! has on Windows. Exits with status 1 if any scene fails, after drawing
//! the others.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use stream_to_speaker::ui::preview::{self, Picture, Scene, Settings};
use stream_to_speaker::ui::theme::Mode;

struct Args {
    out: Option<PathBuf>,
    fonts: Option<PathBuf>,
    modes: Vec<Mode>,
    scales: Vec<u32>,
    time: f64,
    list: bool,
    patterns: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        out: None,
        fonts: None,
        modes: vec![Mode::Light, Mode::Dark],
        scales: vec![100, 150],
        time: 0.25,
        list: false,
        patterns: vec![],
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--out" => a.out = Some(value("--out")?.into()),
            "--fonts" => a.fonts = Some(value("--fonts")?.into()),
            "--theme" => {
                a.modes = value("--theme")?
                    .split(',')
                    .map(|m| match m {
                        "light" => Ok(Mode::Light),
                        "dark" => Ok(Mode::Dark),
                        _ => Err(format!("--theme takes light, dark or both, not {m:?}")),
                    })
                    .collect::<Result<_, _>>()?
            }
            "--scale" => {
                a.scales = value("--scale")?
                    .split(',')
                    .map(|s| match s.trim_end_matches('%').parse::<u32>() {
                        Ok(n @ 50..=400) => Ok(n),
                        _ => Err(format!("--scale takes percentages such as 100,125,150,200, not {s:?}")),
                    })
                    .collect::<Result<_, _>>()?
            }
            "--time" => a.time = value("--time")?.parse().map_err(|_| "--time takes seconds".to_string())?,
            "--list" => a.list = true,
            "-h" | "--help" => return Err(String::new()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.patterns.push(s.to_string()),
        }
    }
    Ok(a)
}

fn write_png(path: &Path, p: &Picture) -> Result<(), String> {
    // Written beside the target and renamed, so a reader never sees half a
    // file, even with two runs writing to the same folder.
    let tmp = path.with_extension(format!("png.{}.tmp", std::process::id()));
    let file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), p.width, p.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(&p.rgba).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("ui_preview: {e}");
            }
            eprintln!("usage: ui_preview --out DIR [--fonts DIR] [--theme light,dark] [--scale 100,150] [--time S] [--list] [SCENE|PREFIX* ...]");
            return ExitCode::from(2);
        }
    };
    let all = stream_to_speaker::ui::scenes();
    let chosen = match preview::select(&all, &args.patterns) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ui_preview: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.list {
        for s in &chosen {
            println!("{}  {}×{}", s.name, s.size[0], s.size[1]);
        }
        return ExitCode::SUCCESS;
    }
    let Some(out) = args.out else {
        eprintln!("ui_preview: --out DIR is needed");
        return ExitCode::from(2);
    };
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("ui_preview: {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    let fonts = match &args.fonts {
        Some(dir) if dir.is_dir() => preview::fonts_from_dir(dir),
        Some(dir) => {
            eprintln!("ui_preview: --fonts {} is not a folder", dir.display());
            return ExitCode::FAILURE;
        }
        None => {
            eprintln!("ui_preview: no --fonts: drawing with egui's own fonts, not at Windows' text size");
            stream_to_speaker::fonts::definitions(&[], &mut |_| None).0
        }
    };

    let mut jobs: Vec<(&Scene, Mode, u32)> = Vec::new();
    for s in &chosen {
        for &m in &args.modes {
            for &sc in &args.scales {
                jobs.push((s, m, sc));
            }
        }
    }
    let next = Mutex::new(0usize);
    let failures = Mutex::new(Vec::<String>::new());
    let written = Mutex::new(0usize);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(4).min(jobs.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let job = {
                    let mut n = next.lock().unwrap();
                    let j = jobs.get(*n).copied();
                    *n += 1;
                    j
                };
                let Some((scene, mode, scale)) = job else { break };
                let settings = Settings { mode, scale: scale as f32 / 100.0, time: args.time };
                let name = format!("{}.{}.{}", scene.name, mode.name(), scale);
                match preview::render(scene, &fonts, settings).and_then(|p| write_png(&out.join(format!("{name}.png")), &p)) {
                    Ok(()) => *written.lock().unwrap() += 1,
                    Err(e) => failures.lock().unwrap().push(format!("{name}: {e}")),
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    println!("ui_preview: wrote {} pictures to {}", written.into_inner().unwrap(), out.display());
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        for f in &failures {
            eprintln!("ui_preview: FAILED {f}");
        }
        ExitCode::FAILURE
    }
}
