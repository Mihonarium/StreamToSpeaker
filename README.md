# Stream To Speaker

A Windows virtual audio device that streams to UPnP / OpenHome network speakers (Sonos, IKEA StreamToSpeaker, KEF, Denon Heos, MoOde, Volumio, and anything else that speaks UPnP AVTransport or OpenHome).

The virtual device shows up as **"Stream To Speaker"** in Windows Sound settings — selectable in the system tray, per-app routable. Audio flows through a small kernel driver into a user-mode Rust service that pushes raw PCM via HTTP to the speaker, with volume in Windows kept in sync with the speaker's hardware buttons.

> **Internal project name is still `symfonisk-bridge` in the source tree** — that's where it started. User-facing strings (binary, Sound-settings device name, web UI title) say "Stream To Speaker". The internal Rust crate name and most file names stay as-is for code stability.

## Architecture

```
┌──────────────────────────┐
│ Any Windows audio source │
│  (Spotify, browser, ...) │
└────────────┬─────────────┘
             │ PCM via WASAPI
             ▼
┌──────────────────────────┐
│ Windows audio engine     │   ~1.3 ms (fixed by OS)
└────────────┬─────────────┘
             │ WaveRT cyclic buffer
             ▼
┌──────────────────────────┐
│ Stream To Speaker driver │   2.66 ms buffer
│  (PortCls, WaveRT)       │
│  - exposes volume node   │
│  - IOCTL inverted call   │
└────────────┬─────────────┘
             │ IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET
             ▼
┌──────────────────────────┐
│ stream-to-speaker.exe    │   silence detect, encode (passthrough for L16)
│  (Rust, MMCSS Pro Audio) │   SSDP discovery, interactive picker
│                          │   /api/speakers, /api/select (runtime switch)
└────────────┬─────────────┘
             │ HTTP GET /stream.raw
             ▼
┌──────────────────────────┐
│ UPnP/OpenHome speaker    │   ~150-300 ms prebuffer (hard floor)
└──────────────────────────┘
```

End-to-end latency target: ~200 ms with L16 over wired ethernet.

## Speaker selection

Three ways to pick which speaker to stream to, in increasing order of automation:

1. **Interactive prompt** (default in a terminal): run with no `--player` flag and you get a numbered list of discovered speakers.

   ```text
   Discovered speakers:
     [1]  Living Room (192.168.1.50)
     [2]  Kitchen     (192.168.1.51)
     [3]  Bedroom     (192.168.1.52)

   Pick a speaker [1-3] (Enter=first, r=refresh, q=skip):
   ```

2. **CLI flag**: `--player "Living Room"` (substring match) or `--player 192.168.1.50` (IP literal). Useful for shortcuts and scripts.

3. **Web UI / API**: navigate to `http://localhost:5901/` for a tiny built-in HTML status page that lists speakers and lets you switch with one click. Same backend exposed at `GET /api/speakers` (JSON) and `POST /api/select` (`{"id": "<udn-or-ip>"}`).

   Switching at runtime: just `POST /api/select` — the service tears down the GENA subscription on the old speaker, sends UPnP Stop, then SetAVTransportURI + Play on the new one. The HTTP audio stream itself stays up; only the speaker change is propagated.

The list refreshes via SSDP every `--ssdp-interval` minutes (default 5). Newly-appeared speakers show up automatically.

`--list-speakers` prints the list and exits, useful for confirming a name before scripting.

## Layout

```
symfonisk-bridge/
├── include/
│   └── symfonisk_ioctl.h     # shared driver<->service ABI (single source of truth)
├── driver/                   # C++ kernel-mode driver (PortCls / WaveRT)
│   ├── README.md
│   ├── StreamToSpeaker.inf   # friendly name = "Stream To Speaker"
│   ├── *.cpp, *.h
│   └── ...
├── service/                  # Rust user-mode bridge
│   ├── Cargo.toml            # binary = stream-to-speaker
│   ├── README.md
│   └── src/
│       ├── main.rs           # wires everything + runtime switching
│       ├── picker.rs         # interactive TTY picker
│       ├── http_server.rs    # stream + /api/speakers + /api/select + /
│       ├── ssdp.rs           # multicast discovery
│       ├── upnp.rs           # SOAP control
│       ├── gena.rs           # event subscription
│       ├── volume_sync.rs    # bidirectional volume
│       ├── silence.rs        # silence detection + noise injection
│       ├── ioctl_source.rs   # real driver source
│       ├── wasapi_source.rs  # fallback loopback source (swyh-rs-style)
│       ├── sine_source.rs    # test tone
│       └── ...
└── docs/
```

## Format support (v1)

Only **L16 PCM, 44.1 kHz, stereo** in the first release. MIME type `audio/L16;rate=44100;channels=2`. Lowest-latency option, broadest compatibility.

Other formats (24-bit, 48 kHz, FLAC, WAV) are deferred until someone wants them. The service architecture supports adding them as alternative output encoders without touching the driver.

## Build prerequisites

- **Driver**: VS 2022 Build Tools (or EWDK) + Windows Driver Kit (WDK) 10.0.22621+. See `driver/README.md`.
- **Service**: Rust 1.74+ (stable). `cargo build --release` from `service/`. Produces `target/release/stream-to-speaker.exe`.
- **Test signing** (development): Secure Boot off, `bcdedit /set testsigning on`, reboot. HVCI / Memory Integrity must be off in Windows Security → Device Security → Core Isolation (and it can't run anyway without Secure Boot).

## Quick start

```powershell
# One-time
bcdedit /set testsigning on
shutdown /r /t 0

# Build + install driver
cd driver
msbuild StreamToSpeaker.sln /p:Configuration=Release /p:Platform=x64
pnputil /add-driver StreamToSpeaker.inf /install

# Build + run service (auto-detect driver, otherwise WASAPI loopback)
cd ..\service
cargo run --release
# Open http://localhost:5901/ to see the speaker list / switch.
```

## Future UI

The HTTP API endpoints under `/api/` are deliberately structured so a future GUI (system tray, separate desktop app, web app on another device) can drive everything without re-implementing discovery or UPnP. The current built-in HTML page at `/` is the minimum-viable picker; it'll grow into a richer status view over time.

## License

MIT. Driver scaffold derives from Microsoft's sysvad sample (MIT); service borrows from swyh-rs's UPnP/SSDP patterns (MIT).
