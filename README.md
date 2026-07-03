# Stream To Speaker

A Windows virtual audio device that streams whatever you're playing on your PC to a network speaker — over **UPnP / OpenHome** (Sonos, IKEA SYMFONISK, KEF, Denon HEOS, MoOde, Volumio, and anything else that speaks UPnP AVTransport or OpenHome) or **AirPlay** (AirPort Express, shairport-sync, and AirPlay 2 devices such as HomePod via HomeKit transient pairing). Discovered speakers from both protocols appear in one list.

The virtual device shows up as **"Stream To Speaker"** in Windows Sound Settings — pick it as the default output (or route a single app to it via Volume Mixer), and the audio flows through a small kernel driver into a user-mode Rust service that pushes raw PCM via HTTP to the speaker. Windows volume stays in sync with the speaker's hardware buttons in both directions.

Ships with a native GUI window plus a system-tray icon for quick controls — speaker picker, drain/pad latency buttons, enable/disable toggle, hard-resync. The headless CLI service is still there behind `--headless` for Windows-service installs.

## Architecture

```
┌────────────────────────────┐
│ Any Windows audio source   │
│  (Spotify, browser, ...)   │
└──────────────┬─────────────┘
               │ PCM via WASAPI
               ▼
┌────────────────────────────┐
│ Windows audio engine       │   ~1-3 ms
└──────────────┬─────────────┘
               │ WaveRT cyclic buffer + event notification
               ▼
┌────────────────────────────┐
│ Stream To Speaker driver   │   2 ms DPC period
│  (PortCls / WaveRT)        │   - exposes volume node
│                            │   - IOCTL inverted-call to user mode
└──────────────┬─────────────┘
               │ IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET
               ▼
┌────────────────────────────┐
│ stream-to-speaker.exe      │   silence detect, noise floor,
│  (Rust, MMCSS Pro Audio)   │   rate-fudge, runtime latency adjust,
│                            │   SSDP discovery + UPnP control plane
└──────────────┬─────────────┘
               │ HTTP GET /stream.raw  (audio/wav, fake content-length)
               ▼
┌────────────────────────────┐
│ UPnP/OpenHome speaker      │   ~100-300 ms prebuffer (speaker side)
└────────────────────────────┘
```

End-to-end latency target on wired ethernet: ~150-300 ms, tuneable at runtime (see below).

## Quick start

```powershell
# One-time: enable test-signed drivers (driver isn't WHQL-signed yet)
bcdedit /set testsigning on
shutdown /r /t 0

# After reboot, verify Secure Boot is OFF and HVCI / Memory Integrity is OFF
# (Windows Security → Device Security → Core isolation → off)

# Build + install the driver
cd driver
msbuild StreamToSpeaker.sln /p:Configuration=Release /p:Platform=x64
pnputil /add-driver StreamToSpeaker.inf /install
# Driver is now installed. Confirm in Device Manager → Sound, video and game controllers.
# You should also see "Stream To Speaker" in Sound Settings.

# Build + run
cd ..\service
cargo build --release
.\target\release\stream-to-speaker.exe
```

Default behaviour: opens the GUI window and adds a system-tray icon. Pick a speaker in the list — audio starts flowing immediately. Closing the window minimises to tray; only the tray menu's **Quit** actually exits.

If you upgrade the driver later, `pnputil /add-driver` only stages the new binary. Either reboot, or in Device Manager → Stream To Speaker → right-click → Disable, then Enable. Confirm the new build is live by looking at the service log: `StreamToSpeaker driver opened (proto=1 build=N ...)` — the `build` number bumps on every shipped binary.

### "Internal AUX Jack — Stream To Speaker" instead of just "Stream To Speaker"

Windows caches the user-visible endpoint name in the registry the first time an endpoint is enrolled, and the cache survives reinstalls. Fresh installs of the current driver get "Stream To Speaker" from the INF; upgrades over a pre-existing install keep the old cached name until you overwrite it (same registry slot the Sound Settings "Rename" button uses).

A one-line PowerShell fix is in `scripts/Rename-Endpoint.ps1`:

```powershell
# Run elevated (modifies HKLM):
.\scripts\Rename-Endpoint.ps1
# Or with a custom display name:
.\scripts\Rename-Endpoint.ps1 -Name "Sonos Picture Frame"
```

The same logic should be a post-install step in any installer that ships this project.

## Modes

By default the binary launches as a GUI app with a system-tray icon. Other modes via flags:

| Flag         | Behaviour                                                              |
|--------------|------------------------------------------------------------------------|
| *(none)*     | Native GUI window + system tray. Closing the window minimises to tray. |
| `--no-tray`  | GUI window only, no tray icon. Closing the window exits the process.   |
| `--headless` | No GUI. Pick a speaker via `--player` or the interactive terminal picker. Used for Windows-service installs and SSH sessions. |
| `--web`      | Enables the HTTP/JSON API and the web UI at `http://<host>:<port>/`. Off by default — the management endpoints are not exposed on the LAN unless you pass this. Can be combined with any mode. |

The audio stream itself (`/stream.raw`) is always served — the speaker pulls from it — but `/`, `/api/speakers`, `/api/select`, `/api/latency/adjust`, `/api/resync` only exist when `--web` is on.

## Speaker selection

Four ways to pick a target:

1. **GUI**: radio-button list in the main window, updates automatically as SSDP sees new devices.

2. **Tray**: left-click the tray icon to open the window, then pick from the list.

3. **CLI flag**: `--player "Living Room"` (substring match) or `--player 192.168.1.50` (IP literal). Works in any mode.

4. **Web API** (when `--web` is on): click in the web UI at `http://<host>:<port>/`, or `POST /api/select` with `{"id": "<udn-or-ip>"}`.

In `--headless` without `--player`, on a TTY, you get an interactive numbered prompt (useful over SSH). `--list-speakers` prints discovered devices and exits.

The list refreshes via SSDP every `--ssdp-interval` minutes (default 5).

## Latency control

End-to-end latency = `Windows engine pipeline + driver buffer + your network + speaker prebuffer`. The first three are O(1) ms; the speaker's prebuffer is the bulk of it. Sonos, for instance, prebuffers ~150-200 ms by default and the buffer drifts with the speaker's own crystal vs. the host's TSC.

The web UI at `http://localhost:5901/` has buttons to nudge it:

- `−25 ms` / `−100 ms`: drain that much audio over ~0.5-2 s (drops samples gradually — sub-millisecond per packet, below the audibility floor). Reduces latency.
- `+25 ms` / `+100 ms`: pad with duplicated frames. Increases latency (use if you went too far and Sonos is glitching).
- `resync`: hard UPnP Stop + Play. Brief audio glitch but Sonos starts fresh with a minimal prebuffer.

Same controls via API for scripting:

```bash
curl -X POST "http://localhost:5901/api/latency/adjust?ms=100"   # trim 100 ms
curl -X POST "http://localhost:5901/api/latency/adjust?ms=-25"   # pad 25 ms
curl -X POST "http://localhost:5901/api/resync"                  # hard reset
```

For *ongoing* drift between the Windows clock and the speaker's audio crystal (typically ±10-100 ppm), set `--rate-fudge-ppm <N>` once at startup:

- Positive (e.g. `+50` to `+200`) duplicates a frame every `1 000 000 / N` frames produced — compensates for a speaker whose crystal runs faster than the host.
- Negative drops a frame at the same rate — for the opposite case (rare).
- Start with `0`, watch the buffer for a few minutes, and tune. The right value is whatever keeps Sonos's buffer level stable instead of slowly draining or growing.

## CLI flags

```
--headless                Disable the GUI; run as a CLI service.
--no-tray                 GUI window only, no system-tray icon.
--web                     Enable the HTTP/JSON API + web UI at the bound port.
                          Off by default — when off, only /stream.raw is served.

--source <auto|driver|wasapi-loopback|sine>
                          Audio input. Default 'auto' tries the kernel driver,
                          falls back to WASAPI loopback (cpal) if not present.

--player <name-or-ip>     Speaker target. Substring match against friendly
                          name, or an IPv4 literal. Omit to use the GUI picker
                          (or the interactive terminal picker in --headless).
--no-interactive          Skip the terminal picker even in a TTY (Windows-service mode).
--list-speakers           Print discovered speakers and exit.
--no-discovery            Skip SSDP entirely; only serve HTTP.

--port <N>                TCP port for /stream.raw and (if --web) the API. Default 5901.
--bind <ip>               HTTP bind address. Default 0.0.0.0.
--advertise-ip <ip>       What IP to put in the stream URI we send to the
                          speaker. Defaults to the first non-loopback IPv4.

--initial-buffer-ms <N>   DIDL prebuffer hint sent to the speaker (default 50).
                          Sonos generally ignores this; tune via the runtime
                          adjust knobs instead.

--silence-pace-ms <N>     Wall-clock ms between silence packets while the
                          Windows audio engine is paused. Default 10
                          (= real-time). Higher = under-produces during
                          silence, draining the speaker's prebuffer between
                          tracks so post-pause latency is smaller.
                          Useful range 12-25; >30 risks underrun.

--rate-fudge-ppm <N>      Steady-state clock-skew compensation. See above.
                          Default 0 (no compensation).

--latency-adjust-step-frames <N>
                          Maximum frames added/dropped per audio packet when
                          servicing a /api/latency/adjust request. Default 4
                          (≈ 0.09 ms per packet). Higher = snappier adjusts
                          at the cost of more audible clicks.

--no-silence-injection    Don't replace silent packets with a low-noise floor.
                          Default: inject ~|4|-peak white noise after 500 ms
                          of silence so Sonos doesn't decide the stream died
                          and disconnect.
--silence-packets-threshold <N>
                          Consecutive silent packets before quiescence kicks in.

--ssdp-interval <N>       Minutes between SSDP re-discoveries. Default 5.
--log-level <level>       error / warn / info / debug / trace. Default info.
```

## Web UI / HTTP API (opt-in)

Pass `--web` to enable. Then `http://<host>:5901/` serves a tiny status page with the speaker list, select buttons, latency-trim buttons, and a resync button. The same endpoints, for scripting:

| Endpoint                          | Method | Effect                                                         | Always on |
|-----------------------------------|--------|----------------------------------------------------------------|-----------|
| `/stream.raw`                     | GET    | Raw PCM stream the speaker pulls (audio/wav, no length).       | yes       |
| `/api/speakers`                   | GET    | JSON list of discovered speakers.                              | `--web`   |
| `/api/select`                     | POST   | `{"id": "<udn-or-ip>"}` to switch the active speaker.          | `--web`   |
| `/api/resync`                     | POST   | UPnP Stop + Play — drops Sonos's accumulated prebuffer.        | `--web`   |
| `/api/latency/adjust?ms=N`        | POST   | `N>0` drains N ms (lower latency); `N<0` pads (higher latency).| `--web`   |
| `/healthz`                        | GET    | Liveness probe — always returns `ok`.                          | `--web`   |

> ⚠️ The API endpoints have no authentication. On an untrusted LAN, either keep them disabled (the default) and use the GUI/tray, or bind to `127.0.0.1` (`--bind 127.0.0.1`), or put it behind a reverse proxy with auth.

## Build prerequisites

- **Driver**: VS 2022 Build Tools (or EWDK) + Windows Driver Kit (WDK) 10.0.22621+. See `driver/README.md`.
- **Service**: Rust 1.74+ stable. `cargo build --release` from `service/`. Produces `target/release/stream-to-speaker.exe`.
- **Installer** (optional): [Inno Setup 6](https://jrsoftware.org/isdl.php) — `choco install innosetup` works too.
- **Test signing** (development): Secure Boot off, `bcdedit /set testsigning on`, reboot. HVCI / Memory Integrity must be off in Windows Security → Device Security → Core Isolation.

## Building the installer

A single-file installer (`StreamToSpeakerSetup-<version>.exe`) ties together the driver, the service binary, the endpoint-rename script, and Start Menu / autostart entries.

```powershell
# Builds driver + service + installer in one shot. Output:
# installer\out\StreamToSpeakerSetup-<version>.exe
.\installer\build-installer.ps1

# Or skip individual steps while iterating:
.\installer\build-installer.ps1 -SkipDriver        # only re-package
.\installer\build-installer.ps1 -SkipDriver -SkipService
.\installer\build-installer.ps1 -Version 0.1.0-rc.1
```

Per-step source: driver via msbuild, service via cargo, installer via Inno Setup's `ISCC.exe`.

### What the installer does on a target machine

1. Copies `stream-to-speaker.exe` to `Program Files\Stream To Speaker`.
2. Drops `StreamToSpeaker.sys`/`.inf`/`.cat` into a `driver\` subdirectory.
3. Runs `pnputil /add-driver /install` — stages the driver and (on Win10 1809+) creates the Root-enumerated device.
4. Runs `scripts\Rename-Endpoint.ps1` to overwrite the cached "Internal AUX Jack" string in the registry.
5. Optionally adds a Start Menu shortcut, a desktop shortcut, and an autostart entry (per-user `Run` key).
6. Offers to launch the app on exit.

Uninstall (via Control Panel → Apps): kills any running `stream-to-speaker.exe`, removes the driver via `Uninstall-Driver.ps1` (looks up the `oemNN.inf` assigned name in the driver store), deletes the install directory.

### Caveat: driver signing

The unsigned driver produced by `cargo`/`msbuild` won't load on a normal Windows machine. For development, the target needs test-signing mode + Secure Boot off + HVCI off (see Build prerequisites above). For a shippable installer that doesn't need test-signing, the `.sys` has to be signed with an EV code-signing certificate and WHQL-attested through the Microsoft Hardware Dev Center portal — out of scope for this repo right now.

## Continuous integration

The `.github/workflows/build.yml` workflow builds driver + service + installer on every push, PR, and tag, on `windows-latest` runners. Artifacts:

- **`StreamToSpeakerSetup-<version>.exe`** — the installer
- **`binaries-<version>`** — raw `.sys`/`.inf`/`.cat` + `stream-to-speaker.exe`

On a tag push (`v1.2.3`), the installer is also attached to a GitHub Release with auto-generated notes.

The workflow installs the WDK from Microsoft's redistributable URL (~5 min cold cache) and Inno Setup via chocolatey. Cargo is cached by `Cargo.lock` hash. Full cold build takes ~15-20 minutes; warm cache cuts it to ~5.

## Layout

```
StreamToSpeaker/
├── include/
│   └── stream_to_speaker_ioctl.h  # shared driver<->service ABI (single source of truth)
├── driver/                        # C++ kernel-mode driver (PortCls / WaveRT)
│   ├── README.md
│   ├── StreamToSpeaker.inf        # device install — names it "Stream To Speaker"
│   ├── *.cpp, *.h
│   └── ...
└── service/                       # Rust user-mode bridge
    ├── Cargo.toml                 # binary = stream-to-speaker
    ├── README.md
    └── src/
        ├── main.rs                # CLI parsing, mode dispatch (GUI / headless / web)
        ├── app.rs                 # central Arc<App> state + action methods
        ├── audio_loop.rs          # extracted audio loop, runs on its own thread
        ├── gui.rs                 # eframe + egui native window
        ├── tray.rs                # system-tray icon + menu
        ├── ioctl_source.rs        # driver IOCTL consumer (audio + events)
        ├── http_server.rs         # /stream.raw (always) + /api/* (opt-in)
        ├── ssdp.rs                # multicast discovery
        ├── upnp.rs                # SOAP control plane + DIDL metadata
        ├── gena.rs                # event subscription (volume from speaker)
        ├── volume_sync.rs         # bidirectional volume bridge
        ├── silence.rs             # silence detection + noise floor
        ├── wasapi_source.rs       # fallback loopback source
        ├── sine_source.rs         # test tone
        └── picker.rs              # interactive TTY picker (used in --headless)
```

## Format support (v1)

L16 PCM, 44.1 kHz, stereo only. Wire MIME `audio/L16;rate=44100;channels=2` (we wrap a 44-byte RIFF/WAVE header in front of the PCM for the actual HTTP body so Sonos accepts it as `audio/wav`). Lowest-latency option with the broadest speaker compatibility. Other formats (24-bit, 48 kHz, FLAC) are deferred until someone wants them — the service architecture is set up to add them as alternative output encoders without touching the driver.

## License

MIT. Driver scaffold derives from Microsoft's sysvad sample (MIT); service borrows UPnP/SSDP patterns from [swyh-rs](https://github.com/dheijl/swyh-rs) (MIT). Thanks to both projects.
