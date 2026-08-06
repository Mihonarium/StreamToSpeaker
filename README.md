# Stream To Speaker

[![Build installer](https://github.com/Mihonarium/StreamToSpeaker/actions/workflows/build.yml/badge.svg)](https://github.com/Mihonarium/StreamToSpeaker/actions/workflows/build.yml)
[![Source: MPL-2.0](https://img.shields.io/badge/source-MPL--2.0-blue)](LICENSE)
[![Binaries: all rights reserved](https://img.shields.io/badge/binaries-all%20rights%20reserved-orange)](LICENSE-BINARIES.md)

Play your PC's sound on real speakers anywhere in the house.

Stream To Speaker adds a virtual audio output to Windows and streams it to
network speakers: **UPnP / OpenHome** (Sonos, IKEA SYMFONISK, KEF, Denon
HEOS, MoOde, Volumio, …) and **AirPlay** (HomePod, AirPort Express,
shairport-sync, …). Windows volume and the speaker's own buttons stay in
sync.

## Install

**[⬇ Download](https://github.com/Mihonarium/StreamToSpeaker/releases/latest/download/StreamToSpeakerSetup.exe)**
and run it. Needs Windows 10 1809+ / Windows 11, 64-bit (not Windows Server).

## Use

1. Set **Stream To Speaker** as the Windows audio output — or route a single
   app to it in the Volume Mixer.
2. Pick a speaker in the app.

If sound lags behind video, the **−25 / −100 ms** buttons drain the delay;
**Resync** (`Ctrl+Shift+R`) restarts the stream fresh. No sound at all →
**Audio not working? Resync**. Speaker missing from the list → **↻ Rescan**,
and check it's on the same network as the PC.

Closing the window minimises to the tray; quit from the tray menu.

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

## Verifying a download

```
gh attestation verify StreamToSpeakerSetup.exe -R Mihonarium/StreamToSpeaker
```

proves the file was built by this repository's CI from a specific commit.
Offline: `--bundle <file>.sigstore.json`, shipped with every release.

## Developers

Architecture, building from source, HTTP API: [TECHNICAL.md](TECHNICAL.md).
Driver signing pipeline: [docs/driver-signing.md](docs/driver-signing.md).

## License

Source [MPL-2.0](LICENSE). Released binaries: free to install and use,
no redistribution or modification — [LICENSE-BINARIES.md](LICENSE-BINARIES.md).

Driver scaffold derives from Microsoft's sysvad sample (MIT); UPnP/SSDP
patterns from [swyh-rs](https://github.com/dheijl/swyh-rs) (MIT).
