# Stream To Speaker

A Windows virtual audio device that streams whatever you're playing on your PC to a UPnP / OpenHome network speaker — Sonos, IKEA SYMFONISK, KEF, Denon HEOS, MoOde, Volumio, and anything else that speaks UPnP AVTransport or OpenHome.

The virtual device shows up as **"Stream To Speaker"** in Windows Sound Settings — pick it as the default output (or route a single app to it via Volume Mixer), and the audio flows through a small kernel driver into a user-mode Rust service that pushes raw PCM via HTTP to the speaker. Windows volume stays in sync with the speaker's hardware buttons in both directions.

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

# Build + run the service
cd ..\service
cargo build --release
.\target\release\stream-to-speaker.exe
```

Then open `http://localhost:5901/` for the speaker picker and live latency controls.

If you upgrade the driver later, `pnputil /add-driver` only stages the new binary. Either reboot, or in Device Manager → Stream To Speaker → right-click → Disable, then Enable. Confirm the new build is live by looking at the service log: `StreamToSpeaker driver opened (proto=1 build=N ...)` — the `build` number bumps on every shipped binary.

## Speaker selection

Three ways to pick a target, in increasing order of automation:

1. **Interactive prompt** (default in a terminal): run with no `--player` flag and you get a numbered list of discovered speakers.

   ```text
   Discovered speakers:
     [1]  Living Room (192.168.1.50)
     [2]  Kitchen     (192.168.1.51)
     [3]  Bedroom     (192.168.1.52)

   Pick a speaker [1-3] (Enter=first, r=refresh, q=skip):
   ```

2. **CLI flag**: `--player "Living Room"` (substring match) or `--player 192.168.1.50` (IP literal). Useful for shortcuts and scripts.

3. **Web UI / API**: navigate to `http://localhost:5901/` for a tiny HTML page that lists speakers and lets you switch with one click. Switching tears down the GENA subscription on the old speaker, sends `Stop` + `SetAVTransportURI` + `Play` on the new one. The HTTP audio stream stays up.

The list refreshes via SSDP every `--ssdp-interval` minutes (default 5). `--list-speakers` prints and exits, useful for confirming a name before scripting.

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
--source <auto|driver|wasapi-loopback|sine>
                          Audio input. Default 'auto' tries the kernel driver,
                          falls back to WASAPI loopback (cpal) if not present.

--player <name-or-ip>     Speaker target. Substring match against friendly
                          name, or an IPv4 literal. Omit for interactive picker.
--no-interactive          Skip the picker even in a TTY (for Windows-service mode).
--list-speakers           Print discovered speakers and exit.
--no-discovery            Skip SSDP entirely; only serve HTTP.

--port <N>                TCP port for /stream.raw and the web UI. Default 5901.
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

## Web UI / HTTP API

`http://<host>:5901/` serves a tiny status page with the speaker list, select buttons, latency-trim buttons, and a resync button. The same endpoints, for scripting:

| Endpoint                          | Method | Effect                                                         |
|-----------------------------------|--------|----------------------------------------------------------------|
| `/stream.raw`                     | GET    | Raw PCM stream the speaker pulls (audio/wav, no length).       |
| `/api/speakers`                   | GET    | JSON list of discovered speakers.                              |
| `/api/select`                     | POST   | `{"id": "<udn-or-ip>"}` to switch the active speaker.          |
| `/api/resync`                     | POST   | UPnP Stop + Play — drops Sonos's accumulated prebuffer.        |
| `/api/latency/adjust?ms=N`        | POST   | `N>0` drains N ms (lower latency); `N<0` pads (higher latency).|
| `/healthz`                        | GET    | Liveness probe — always returns `ok`.                          |

> ⚠️ The API endpoints have no authentication. On an untrusted LAN, bind to `127.0.0.1` (`--bind 127.0.0.1`) and configure your speaker(s) to reach the host via that IP, or put the service behind a reverse proxy with auth.

## Build prerequisites

- **Driver**: VS 2022 Build Tools (or EWDK) + Windows Driver Kit (WDK) 10.0.22621+. See `driver/README.md`.
- **Service**: Rust 1.74+ stable. `cargo build --release` from `service/`. Produces `target/release/stream-to-speaker.exe`.
- **Test signing** (development): Secure Boot off, `bcdedit /set testsigning on`, reboot. HVCI / Memory Integrity must be off in Windows Security → Device Security → Core Isolation.

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
        ├── main.rs                # wires everything, audio loop, runtime adjust
        ├── ioctl_source.rs        # driver IOCTL consumer (audio + events)
        ├── http_server.rs         # /stream.raw + /api/* + status page
        ├── ssdp.rs                # multicast discovery
        ├── upnp.rs                # SOAP control plane + DIDL metadata
        ├── gena.rs                # event subscription (volume from speaker)
        ├── volume_sync.rs         # bidirectional volume bridge
        ├── silence.rs             # silence detection + noise floor
        ├── wasapi_source.rs       # fallback loopback source
        ├── sine_source.rs         # test tone
        └── picker.rs              # interactive TTY picker
```

## Format support (v1)

L16 PCM, 44.1 kHz, stereo only. Wire MIME `audio/L16;rate=44100;channels=2` (we wrap a 44-byte RIFF/WAVE header in front of the PCM for the actual HTTP body so Sonos accepts it as `audio/wav`). Lowest-latency option with the broadest speaker compatibility. Other formats (24-bit, 48 kHz, FLAC) are deferred until someone wants them — the service architecture is set up to add them as alternative output encoders without touching the driver.

## License

MIT. Driver scaffold derives from Microsoft's sysvad sample (MIT); service borrows UPnP/SSDP patterns from [swyh-rs](https://github.com/dheijl/swyh-rs) (MIT). Thanks to both projects.
