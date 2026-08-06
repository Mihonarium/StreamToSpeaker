# Stream To Speaker

[![Build installer](https://github.com/Mihonarium/StreamToSpeaker/actions/workflows/build.yml/badge.svg)](https://github.com/Mihonarium/StreamToSpeaker/actions/workflows/build.yml)
[![Source: MPL-2.0](https://img.shields.io/badge/source-MPL--2.0-blue)](LICENSE)
[![Binaries: all rights reserved](https://img.shields.io/badge/binaries-all%20rights%20reserved-orange)](LICENSE-BINARIES.md)

Play your PC's sound on real speakers anywhere in the house.

Stream To Speaker adds a virtual audio device to Windows and streams whatever
plays through it to a network speaker — **UPnP / OpenHome** (Sonos, IKEA
SYMFONISK, KEF, Denon HEOS, MoOde, Volumio, and anything else that speaks UPnP
AVTransport or OpenHome) or **AirPlay** (AirPort Express, shairport-sync, and
AirPlay 2 devices such as HomePod). Speakers from both protocols appear in one
list. Windows volume stays in sync with the speaker's own buttons, in both
directions.

## Install

1. **[⬇ Download Stream To Speaker](https://github.com/Mihonarium/StreamToSpeaker/releases/latest/download/StreamToSpeakerSetup.exe)**
   and run it.
2. That's it. No test-signing mode, no Secure Boot changes — the driver is
   Microsoft-signed.

**Requirements:** Windows 10 1809+ or Windows 11, 64-bit, and a speaker on
your network that speaks UPnP/OpenHome or AirPlay. (Windows Server is not
supported — attestation-signed drivers don't load there.)

## Use

1. Set **Stream To Speaker** as your Windows audio output (taskbar speaker
   icon → output picker) — or route just one app to it via the Volume Mixer.
2. Pick a speaker from the list in the app window. Audio starts immediately.
3. If sound and video drift apart, use the latency buttons (**−25 / −100 ms**
   drain, **+25 / +100 ms** pad) or press **Resync** for a fresh start.

Closing the window minimises to the tray; the tray menu has quick controls
and **Quit**. Keyboard shortcuts: `Ctrl+E` enable/disable, `Ctrl+R` rescan,
`Ctrl+Shift+R` resync.

### If something misbehaves

- **It says streaming, but there's no sound** → **Audio not working? Resync**
  in the status panel (or `Ctrl+Shift+R`).
- **No speakers in the list** → press **↻ Rescan**; check the speaker is on
  the same network/VLAN as the PC.

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

## Verifying your download

Every released binary carries a GitHub build-provenance attestation: proof
that these exact bytes came out of this repository's workflow, at a specific
commit — not from someone's laptop. The code-signing certificate says *who*
published a file; the attestation says *what it was built from*.

```
gh attestation verify StreamToSpeakerSetup.exe -R Mihonarium/StreamToSpeaker
```

Verifying the installer covers everything inside it. Each release also ships
a Sigstore bundle (`<file>.sigstore.json`) for offline verification with
`--bundle`. The driver additionally carries Microsoft's attestation
signature, which is what lets it load with Secure Boot on.

## For developers

Architecture, building from source, CLI flags, the HTTP API, latency
internals, CI, and the repo layout live in **[TECHNICAL.md](TECHNICAL.md)**.
The driver signing/attestation pipeline is documented in
[docs/driver-signing.md](docs/driver-signing.md).

## License

**Source: [MPL-2.0](LICENSE)** — use it, fork it, ship it inside proprietary
software if you like; changes to *these files* come back to the commons.

**Released binaries: [all rights reserved](LICENSE-BINARIES.md)** — install
and use them freely, but don't redistribute or modify them. They carry our
code-signing certificate and the driver's Microsoft attestation signature,
which stand for our identity and a paid, audited process; the code behind
them is open, our signatures are not. Want to ship a build? Build from source
and sign it yourself, or ask us.

Driver scaffold derives from Microsoft's sysvad sample (MIT); service borrows
UPnP/SSDP patterns from [swyh-rs](https://github.com/dheijl/swyh-rs) (MIT).
Thanks to both projects — their notices are retained in the files concerned.
