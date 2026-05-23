# Stream To Speaker (service)

User-mode Rust service that takes PCM audio from either:

- the **Stream To Speaker virtual audio driver** (via `DeviceIoControl` over the IOCTL contract in `../include/stream_to_speaker_ioctl.h`), or
- **WASAPI loopback** (mirrors whatever Windows is currently playing, through `cpal`), or
- a built-in **440 Hz sine** for end-to-end testing

…and streams it as raw L16 PCM (44.1 kHz, stereo, big-endian per RFC 3551) over HTTP to a UPnP/OpenHome network speaker. SSDP picks the speaker up, UPnP/SOAP starts playback, and a GENA subscription keeps Windows mixer volume in sync with the speaker hardware buttons.

> The Rust **library crate** is still named `stream_to_speaker` internally — the **binary** is `stream-to-speaker`. Internal struct names follow the library convention; user-visible strings say "Stream To Speaker".

## Build

```powershell
# Stable Rust 1.74+
cd service
cargo build --release
```

The binary lands at `target/release/stream-to-speaker.exe`.

## Run

```powershell
# Interactive picker: lists discovered speakers and prompts.
stream-to-speaker

# Auto-pick first speaker (skip prompt) — useful in scripts and services
stream-to-speaker --no-interactive

# Target a specific speaker by name or IP
stream-to-speaker --player "Living Room"
stream-to-speaker --player 192.168.1.50

# Print discovered speakers and exit
stream-to-speaker --list-speakers

# Force the kernel driver (will error if not installed)
stream-to-speaker --source driver

# Loopback fallback explicitly, on a specific WASAPI output device
stream-to-speaker --source wasapi-loopback --device "Speakers"

# Just play a 440 Hz tone, useful for testing the network leg
stream-to-speaker --source sine

# Serve-only mode (no SSDP / UPnP). Point a speaker at our URL yourself.
stream-to-speaker --no-discovery --port 5901
```

## HTTP endpoints

| Method | Path                 | Purpose                                                      |
| ------ | -------------------- | ------------------------------------------------------------ |
| GET    | `/`                  | Tiny built-in HTML page: speaker list + one-click switching. |
| GET    | `/stream.raw`        | Endless chunked L16 PCM stream consumed by the speaker.      |
| GET    | `/healthz`           | Returns "ok" — for monitoring.                               |
| GET    | `/api/speakers`      | JSON list of discovered speakers (with active marker).       |
| POST   | `/api/select`        | Body `{"id": "<udn-or-ip>"}` — switch the active speaker.    |
| NOTIFY | `/gena`              | GENA event callback target for UPnP RenderingControl.        |

The `/api/*` endpoints are the integration surface for any future tray icon or richer UI.

## Known limitations

- L16 only on the wire. FLAC, WAV, 24-bit, 48 kHz: future work.
- Single active speaker at a time. Multi-room mirroring would need either Sonos groups (out of scope) or fanning the HTTP stream to multiple renderers.
- Tray icon / proper GUI: not yet — the `/api/*` endpoints are the integration point when it ships.
