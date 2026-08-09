# Stream To Speaker — technical documentation

How it works and how to build it. Install and use: [README](README.md).
Flags: [README](README.md#cli-flags). Driver signing pipeline:
[docs/driver-signing.md](docs/driver-signing.md).

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
│                            │   discovery + control plane
└──────────────┬─────────────┘
               │ UPnP: HTTP GET /stream.raw   AirPlay: RTSP + RTP
               ▼
┌────────────────────────────┐
│ Speaker                    │   ~100-300 ms prebuffer (speaker side)
└────────────────────────────┘
```

End-to-end latency on wired ethernet: ~150-300 ms, tuneable at runtime.

## Build prerequisites

- **Driver**: VS 2022 Build Tools (or EWDK) + WDK 10.0.22621+. See
  `driver/README.md`.
- **Service**: Rust 1.74+ stable.
- **Installer** (optional): [Inno Setup 6](https://jrsoftware.org/isdl.php).

## Building from source

A locally built driver is unsigned, so the machine needs test-signing mode
with Secure Boot and HVCI off. Released drivers are Microsoft-signed and
need none of this — [docs/driver-signing.md](docs/driver-signing.md) covers
the pipeline that produces them.

```powershell
bcdedit /set testsigning on
shutdown /r /t 0
# Secure Boot and Core isolation → Memory integrity must both be off.

cd driver
msbuild StreamToSpeaker.sln /p:Configuration=Release /p:Platform=x64
pnputil /add-driver StreamToSpeaker.inf /install

cd ..\service
cargo build --release
.\target\release\stream-to-speaker.exe
```

`pnputil /add-driver` only stages an upgrade: reboot, or disable and re-enable
the device in Device Manager. The service log line
`StreamToSpeaker driver opened (proto=1 build=N ...)` confirms which build is
live — `N` bumps on every shipped binary.

### Building the installer

```powershell
.\installer\build-installer.ps1                          # driver + service + installer
.\installer\build-installer.ps1 -SkipDriver              # re-package only
.\installer\build-installer.ps1 -Version 0.1.0-rc.1
```

Output: `installer\out\StreamToSpeakerSetup-<version>.exe`. It installs the
service to `Program Files`, stages the driver with `pnputil /add-driver
/install`, runs `scripts\Rename-Endpoint.ps1`, and optionally adds Start Menu,
desktop and autostart entries. Uninstall removes the driver via
`Uninstall-Driver.ps1` and deletes the install directory.

### Endpoint named "Internal AUX Jack — Stream To Speaker"

Windows caches an endpoint's display name in the registry on first enrolment
and keeps it across reinstalls, so upgrades over an older install show the
stale name. `scripts\Rename-Endpoint.ps1`, run elevated, overwrites it
(`-Name` sets a custom one). The installer does this automatically.

## Modes

| Flag | Behaviour |
|---|---|
| *(none)* | GUI window + tray. Closing the window minimises to tray. |
| `--no-tray` | GUI only. Closing the window exits. |
| `--headless` | No GUI; speaker via `--player` or the terminal picker. For service installs and SSH. |
| `--web` | Adds the HTTP/JSON API and web UI. Combines with any mode. |

`/stream.raw` is always served — the speaker pulls from it. The management
endpoints exist only under `--web`.

## Speaker selection

The GUI and tray lists update as discovery runs. `--player` takes a name
substring or an IPv4 literal; `--list-speakers` prints and exits. Without
`--player`, `--headless` on a TTY offers a numbered prompt. Under `--web`,
`POST /api/select` with `{"id": "<udn-or-ip>"}`. Discovery re-runs every
`--ssdp-interval` minutes.

### Sonos groups

Discovery queries the Sonos `ZoneGroupTopology` service (`GetZoneGroupState`,
answered by any player) and folds the zone-group state into the list
(`sonos.rs`): bonded invisible units (stereo-pair slaves, Subs, surrounds —
`Invisible="1"` members and `Satellite` children), BRIDGE/BOOST units
(`IsZoneBridge="1"`), and grouped non-coordinator members are hidden, and
each group appears as its coordinator, named "Living Room + Kitchen" (or
"+ N" beyond two). Members are hidden because `SetAVTransportURI` *is*
Sonos's group-membership mechanism — `x-rincon:<coordinator-UUID>` joins a
zone to a group, so pointing a member at a normal stream URI changes its
membership instead of coexisting with it (SoCo hard-errors client-side on
any transport call to a non-coordinator for the same reason). `--player`
also matches group member names. Selecting a speaker re-checks topology from
the device itself and redirects to its current coordinator, so a list stale
by up to one discovery interval can't break a group. Group sessions route
volume through `GroupRenderingControl` on the coordinator —
`SetGroupVolume`/`SetGroupMute` scale every member proportionally like the
Sonos app's group slider, and error 701 anywhere else — and subscribe GENA
to its flat `GroupVolume`/`GroupMute` events instead of `RenderingControl`'s
`LastChange`. Both ZoneGroupState shapes are handled (S2 wraps
`<ZoneGroups>` in `<ZoneGroupState>`; pre-10.1 S1 doesn't), and the SOAP
client de-chunks response bodies — Sonos chunks HTTP/1.1 responses, which
small replies survive by accident but a multi-KB `GetZoneGroupState` does
not.

## Latency control

End-to-end latency is `Windows engine + driver buffer + network + speaker
prebuffer`. The first three are O(1) ms; the speaker's prebuffer dominates.
Sonos prebuffers ~150-200 ms and drifts against the host clock.

The GUI and web UI expose:

- **−25 / −100 ms** — drain over ~0.5-2 s, dropping sub-millisecond amounts
  per packet, below the audibility floor.
- **+25 / +100 ms** — pad with duplicated frames, if you overshot.
- **Resync** — stop and restart the session: one brief glitch, minimal
  prebuffer afterwards.

```bash
curl -X POST "http://localhost:5901/api/latency/adjust?ms=100"   # trim 100 ms
curl -X POST "http://localhost:5901/api/latency/adjust?ms=-25"   # pad 25 ms
curl -X POST "http://localhost:5901/api/resync"
```

For *ongoing* drift (typically ±10-100 ppm), set `--rate-fudge-ppm` once at
startup: positive duplicates a frame every `1 000 000 / N` frames (speaker
crystal faster than the host), negative drops one. Start at 0 and tune until
the speaker's buffer level holds steady.

### Tuning notes

- `--initial-buffer-ms` is a prebuffer hint in the DIDL metadata; Sonos
  generally ignores it, so prefer the runtime knobs.
- `--silence-pace-ms` is wall-clock ms between silence packets while the
  audio engine is paused. Default 10 = real-time; higher under-produces,
  draining the speaker's prebuffer between tracks so post-pause latency is
  smaller. Useful range 12-25; above 30 risks underrun.
- `--latency-adjust-step-frames` caps frames added or dropped per packet
  while adjusting. Default 4 (≈0.09 ms per packet); higher is snappier at the
  cost of audible clicks.
- Silence injection (default on) replaces silent packets with ~|4|-peak white
  noise after 500 ms so the speaker doesn't treat the stream as dead.

## HTTP API

`--web` serves a status page at `http://<host>:5901/` plus:

| Endpoint | Method | Effect | Always on |
|---|---|---|---|
| `/stream.raw` | GET | Raw PCM the speaker pulls. | yes |
| `/api/speakers` | GET | Discovered speakers. | `--web` |
| `/api/select` | POST | `{"id": "<udn-or-ip>"}` — switch speaker. | `--web` |
| `/api/resync` | POST | Restart the session. | `--web` |
| `/api/latency/adjust?ms=N` | POST | `N>0` drains, `N<0` pads. | `--web` |
| `/healthz` | GET | Returns `ok`. | `--web` |

> ⚠️ The API has no authentication. Leave it off (the default), bind to
> `127.0.0.1`, or front it with a reverse proxy that authenticates.

## Continuous integration

`build.yml` builds driver + service + installer on every push, PR and tag.
Pushes to `main` also produce an installer bundling the Microsoft-signed
driver whenever one matches the current driver source.

A `v*` tag runs the release chain — build → sign the service binary → package
→ sign the installer — publishing signed binaries with checksums and, on
public repos, build-provenance attestations. Two approval clicks, both in the
signing repository.

`driver-submission.yml`, `driver-attest.yml` and `driver-attested.yml` handle
the driver's Microsoft attestation round-trip:
[docs/driver-signing.md](docs/driver-signing.md).

The WDK install and Rust dependencies are cached; a cold build is ~15-20 min,
warm ~5.

## Layout

```
StreamToSpeaker/
├── include/stream_to_speaker_ioctl.h   shared driver↔service ABI
├── driver/                             C++ kernel driver (PortCls / WaveRT)
├── installer/                          Inno Setup script + build-installer.ps1
├── scripts/                            endpoint rename, driver uninstall
├── docs/                               signing pipeline, specs, plans
└── service/src/
    ├── main.rs          CLI parsing, mode dispatch
    ├── app.rs           central Arc<App> state + actions
    ├── audio_loop.rs    audio loop, own thread
    ├── audio_source.rs  source abstraction
    ├── ioctl_source.rs  driver IOCTL consumer (audio + events)
    ├── wasapi_source.rs loopback fallback · sine_source.rs  test tone
    ├── gui.rs           eframe/egui window · tray.rs  tray icon + menu
    ├── picker.rs        terminal picker (--headless)
    ├── http_server.rs   /stream.raw + /api/*
    ├── ssdp.rs          discovery · upnp.rs  SOAP + DIDL · gena.rs  events
    ├── airplay/         RAOP + AirPlay 2: pairing, crypto, RTSP, PTP, AAC
    ├── volume_sync.rs   bidirectional volume bridge
    ├── silence.rs       silence detection + noise floor
    ├── now_playing.rs   track metadata · endpoint_name.rs  endpoint rename
    ├── user_config.rs   persisted preferences · qpc.rs  high-res clock
    └── lib.rs           library surface for tests
```

## Formats

UPnP/OpenHome: L16 PCM, 44.1 kHz, stereo — wire MIME
`audio/L16;rate=44100;channels=2`, wrapped in a 44-byte RIFF/WAVE header so
Sonos accepts it as `audio/wav`. Lowest latency, broadest compatibility.

AirPlay: ALAC for RAOP, and AAC-LC via Windows Media Foundation for AirPlay 2
buffered audio.
