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

`pnputil /add-driver` only stages an upgrade, and the driver's control device
keeps the loaded image in memory until Windows restarts, so a new driver binary
only runs after a reboot (disabling and re-enabling the device is not enough).
The installer asks for a restart when an upgrade replaces the driver binary
that was installed before. The service log line
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
desktop and autostart entries. `Pre-Install.ps1` and `Rename-Endpoint.ps1`
log to `logs\install.log` in the install directory, not to
`%LOCALAPPDATA%`; `scripts\Diagnose.ps1` prints its tail. Uninstall removes the driver
via `Uninstall-Driver.ps1`, removes the test-signing certificate a test-signed
build imported into `LocalMachine\Root` and `TrustedPublisher`
(`Remove-TestCertificate.ps1`; an upgrade removes the replaced build's
certificate when it differs), and deletes the install directory.

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
| `--headless` | No GUI; speaker via `--player` or the terminal picker. For service installs and SSH; `--web-link` prints the web UI's link. |
| `--web` | Adds the web UI and HTTP/JSON API, behind a secret link ([HTTP API](#http-api)). Combines with any mode; with `--headless` the link is printed once at start, to a console that a service may not have. |
| `--web-link` | Prints the web UI's link and exits (making its key if there is none yet). Run it as the account the app runs under, with the app's `--port` and `--advertise-ip`. |

`/stream.raw` is always served — the speaker pulls from it: to this PC, and to
other devices while streaming is enabled (with privacy mode on, only to the
speaker being played on and its Sonos group). The management endpoints exist
only under `--web`.

## Speaker selection

The GUI and tray lists update as discovery runs. `--player` takes a name
substring or an IPv4 literal; `--list-speakers` prints and exits. Without
`--player`, `--headless` on a TTY offers a numbered prompt. Under `--web`,
`POST /api/select` with `{"id": "<udn-or-ip>"}` and the API key
([HTTP API](#http-api)). Discovery re-runs every `--ssdp-interval` minutes.

### Sonos groups

Discovery queries the Sonos `ZoneGroupTopology` service (`GetZoneGroupState`,
answered by any player) and folds the zone-group state into the list
(`sonos.rs`): bonded invisible units (stereo-pair slaves, Subs, surrounds —
`Invisible="1"` members and `Satellite` children), BRIDGE/BOOST units
(`IsZoneBridge="1"`), and grouped non-coordinator members are hidden, and
each group appears as its coordinator, named "Living Room + Kitchen" (or
"+ N" beyond two). One topology, from the first of up to three players on
this PC's networks that answers, says which units might be hidden, but a unit is hidden only on its own word:
each unit it would hide is first asked for its own topology, at its own
address, so a device answering discovery can't make other speakers
disappear. A unit that answers when asked is named from its own answer;
the other rows keep the first topology's labels. A unit that can't be
asked keeps its answer from an earlier scan, or stays listed. Members are
hidden because `SetAVTransportURI` *is* Sonos's group-membership mechanism —
`x-rincon:<coordinator-UUID>` joins a zone to a group, so pointing a
member at a normal stream URI changes its membership instead of coexisting
with it (SoCo hard-errors client-side on any transport call to a
non-coordinator for the same reason). `--player`
also matches group member names. Selecting a speaker re-checks topology from
the device itself and redirects to its current coordinator (for a speaker
discovery found, only one on this PC's networks), so a list stale by up to
one discovery interval can't break a group. Group sessions route
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
KEY=...   # the web UI link's key: see HTTP API, below
curl -X POST -H "Authorization: Bearer $KEY" "http://localhost:5901/api/latency/adjust?ms=100"   # trim 100 ms
curl -X POST -H "Authorization: Bearer $KEY" "http://localhost:5901/api/latency/adjust?ms=-25"   # pad 25 ms
curl -X POST -H "Authorization: Bearer $KEY" "http://localhost:5901/api/resync"
```

For *ongoing* drift (typically ±10-100 ppm), set `--rate-fudge-ppm` once at
startup: positive duplicates a frame every `1 000 000 / N` frames (speaker
crystal faster than the host), negative drops one. Start at 0 and tune until
the speaker's buffer level holds steady.

### AirPlay buffer

AirPlay delay is dominated by a buffer the *sender* asks for: each sync
packet tells the speaker "the frame written N samples ago is playing now",
so N is the receiver's buffer depth and most of the audible delay. iTunes
uses 2 s (88200 samples). **Advanced → AirPlay buffer (latency)** sets N in
milliseconds (`airplay_latency_ms` in config.json; default 2000, range
20–3000):

- The receiver's own `Audio-Latency` (stated on SETUP and/or RECORD) is a
  floor: the anchor is `max(configured, advertised)`, so a speaker that
  reports its minimum is never driven below it, and a big-DSP AVR asking
  for more than 2 s still gets it.
- AirPlay 2: the value drives the realtime stream's declared
  `latencyMin/Max` and its sync anchor. Below 1000 ms the buffered stream
  (which holds 1–2 s regardless) is skipped in favour of realtime.
- Realistic floors: a recent Apple TV accepts very small values on a good
  network; AirPort Express and most AirPlay speakers need ~100–350 ms. Any
  Wi-Fi hiccup longer than the buffer is a dropout, so lower it until
  playback stutters, then back off. Applies on the next connect.

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

## Update check

`src/update_check.rs`. Once a day (first ~20 s after launch, never on the
startup path) the app GETs
`https://api.github.com/repos/Mihonarium/StreamToSpeaker/releases/latest` over
WinHTTP — the OS HTTPS stack, so the system certificate store and proxy
apply and no TLS library is bundled. The request carries only a
`User-Agent` with the app version. If the release tag is newer (strict
semver) a banner offers the release page; nothing is downloaded or installed
automatically. "Skip this version" / "Later" and the on/off switch
(`check_for_updates`, Advanced) persist in `config.json`.

The version compared is the **release tag**, baked in by CI as
`STS_RELEASE_VERSION` on `v*` tag builds (`release_version()` /
`display_version()` in `lib.rs`) — not `CARGO_PKG_VERSION`, which is
versioned independently. A build without it is a dev build: the label shows
the crate version and update checks are off.

## HTTP API

`--web` (or **Enable web UI** in the window) turns on a status page and the
JSON API. Both need the web UI's link, `http://<host>:5901/r/<key>/`. The
window's *Web UI* card shows it (*Copy link*, *Open in browser*); a
`--headless --web` run prints it once to standard output when it starts, and
never writes it to the log. Headless and service installs get it with
`stream-to-speaker.exe --web-link`, which prints the link and exits: run it as
the account the app runs under (the key is in that account's `config.json`),
with the app's `--port` and `--advertise-ip` if it has them. `<key>` is 32 hex
digits from the system's random source, made the first time the web UI is
turned on (or `--web-link` runs) and kept in `config.json` (sealed with DPAPI,
like the other stored secrets). **Make a new link** replaces it; the old link
and key stop working at once. Anyone with the link can control the app on the
network.

| Endpoint | Method | Effect | Always on |
|---|---|---|---|
| `/stream.raw` | GET | Raw PCM the speaker pulls. | yes |
| `/r/<key>/` | GET | The status page (its script: `/r/<key>/app.js`). | `--web` |
| `/api/speakers` | GET | Discovered speakers. | `--web` |
| `/api/select` | POST | `{"id": "<udn-or-ip>"}` with `Content-Type: application/json` — switch speaker. | `--web` |
| `/api/resync` | POST | Restart the session. | `--web` |
| `/api/latency/adjust?ms=N` | POST | `N>0` drains, `N<0` pads. | `--web` |
| `/healthz` | GET | Returns `ok`. | yes |

Every `/api/*` request needs the key as a bearer token,
`Authorization: Bearer <key>`; without it, or with a wrong one, the answer is
`401` with a JSON `error`. Scripts send it like this:

```bash
KEY=...   # the part of the web UI's link after /r/ (32 hex digits), from the window or --web-link
curl -H "Authorization: Bearer $KEY" http://localhost:5901/api/speakers
curl -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"id":"uuid:RINCON_..."}' http://localhost:5901/api/select
```

`/`, `/app.js` and a link with a wrong key answer `401` with a page saying where to find the link. Pages under `/r/` are sent
with `Referrer-Policy: no-referrer` and `Cache-Control: no-store`, and the key
is replaced by `<key>` wherever a request path is logged.

Requests must also name this machine in `Host` (`localhost`, its computer
name, one of its IP addresses, or `--advertise-ip`), and the `POST` routes
refuse requests a browser marks as coming from another site (`Origin` /
`Sec-Fetch-Site`), even with the key, so a web page can't drive the API or
read the stream through DNS rebinding.

`/stream.raw` and the speakers' event callbacks take no key: speakers can't
send one. The stream follows the streaming switch and privacy mode instead.

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
