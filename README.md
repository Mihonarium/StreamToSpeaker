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
   and run it. (That link always points at the newest signed installer; the
   [releases page](https://github.com/Mihonarium/StreamToSpeaker/releases/latest)
   has the versioned file, checksums and provenance bundles.)
2. That's it — the installer sets up the audio driver and the app. No
   test-signing mode, no Secure Boot changes: the driver is signed by
   Microsoft (attestation signing), so it loads on stock Windows.

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
- **The output device is named "Internal AUX Jack — …"** → an old cached name
  from a previous install; run `scripts/Rename-Endpoint.ps1` elevated (the
  installer normally does this for you).

## Verifying your download

Every released binary carries a GitHub build-provenance attestation: proof
that these exact bytes came out of this repository's workflow, at a specific
commit — not from someone's laptop. The code-signing certificate says *who*
published a file; the attestation says *what it was built from*.

```
gh attestation verify StreamToSpeakerSetup.exe -R Mihonarium/StreamToSpeaker
```

(The attestation binds to the file's *contents*, so it verifies whether you
downloaded the stable-named or the versioned copy.)

Verifying the installer covers everything inside it — service and driver
alike are part of the attested bytes. Each release also ships a Sigstore
bundle (`<file>.sigstore.json`) for offline verification with `--bundle`.
The kernel driver additionally carries Microsoft's attestation signature
(*Microsoft Windows Hardware Compatibility Publisher*), which is what lets
it load with Secure Boot on.

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
