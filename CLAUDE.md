# CLAUDE.md — instructions for future Claude Code sessions

## Operating principles

1. **Research, don't speculate.** When the user reports a problem you don't fully understand (especially Windows / driver / OS-level), reach for `WebSearch` and `WebFetch` *before* proposing fixes. The user has explicitly called out cases where speculation wasted iteration cycles — examples: the "Internal AUX Jack" label, the "Allow apps and Windows to use this device" toggle (which turned out to be the documented `PKEY_AudioDevice_EnableEndpointByDefault` behaviour, not anything I'd guessed). If the user says "are you sure?", that's a signal to actually look it up.

2. **Verify with cargo check before pushing.** This is a Linux container; build the service with `cd service && cargo check && cargo check --target x86_64-pc-windows-gnu` after any service edit. The Windows cross-check catches GUI / tray / IOCTL issues that the Linux check skips via `cfg(windows)`. Driver edits can't be syntax-checked here (need WDK) — read carefully and trust the user / CI.

3. **PR-per-merge workflow.** This session pushes to a single fixed branch (`claude/stream-speaker-audio-delivery-Utgpx`), and the user merges each PR as a unit. After every push, check `mcp__github__list_pull_requests` for an open PR matching the branch; if none (previous one merged), open a new one via `mcp__github__create_pull_request` with a focused title describing the new commits. Don't let the branch accumulate too many merged-then-orphaned commits without a fresh PR.

4. **Don't claim "fixed" without verifying.** When a fix lands and the user reports it still doesn't work, the first move is `WebSearch` for what *actually* triggers the symptom — not another guess.

## Project: Stream To Speaker

Windows virtual audio device that streams system audio to network speakers — UPnP/OpenHome (Sonos primarily) **and** AirPlay (RAOP / AirPlay 1, plus AirPlay 2 / HomeKit for HomePod). Two parts:

- **`driver/`** — C++ kernel-mode WaveRT/PortCls driver. Single render endpoint, fixed L16 44.1 kHz stereo. Inverted-call IOCTL pattern delivers audio frames to user mode. Includes a separate non-PnP control device for the IOCTLs.
- **`service/`** — Rust user-mode bridge. Default mode is GUI (egui window + tray icon); `--headless` is the CLI mode; `--web` enables the HTTP/JSON API. Talks to the driver via IOCTLs. Discovers speakers via SSDP (UPnP) and mDNS (`_raop._tcp` / `_airplay._tcp`); controls UPnP via SOAP and AirPlay via RTSP; streams PCM either by serving HTTP `audio/wav` (UPnP pull) or pushing RTP/UDP (AirPlay, ALAC). AirPlay code lives in `service/src/airplay/`.

Shared ABI lives in `include/stream_to_speaker_ioctl.h`. Any change to the on-the-wire layout has to be mirrored in `service/src/ioctl_source.rs`.

## Driver versioning

INF DriverVer format: `MAJOR.MINOR.BUILD.REVISION`. Two pieces, two sources:

### Auto: revision (`REVISION` = git commit count)

Bumped on every CI run and every `build-installer.ps1` run, no manual step. Drives:

1. **INF `DriverVer`** (e.g. `1.0.0.42`) — what Windows uses for PnP version comparison and what Device Manager → Properties → Driver shows. From `StreamToSpeaker.vcxproj`: `<TimeStamp>$(DriverVersionPrefix).$(DriverBuildNumber)</TimeStamp>`, with `DriverBuildNumber` injected via `/p:DriverBuildNumber=N`.
2. **`STREAM_TO_SPEAKER_DRIVER_BUILD` in `driver/driver.h`** — runtime identifier, returned via `IOCTL_STREAM_TO_SPEAKER_GET_VERSION`, logged by the service (`StreamToSpeaker driver opened (proto=1 build=N ...)`).

Both get the same N every build, so `1.0.0.42` in Device Manager == `build=42` in the service log == same `.sys` binary. Override with `-DriverBuild N` on the local script for a reproducible value.

**CI driver cache (build.yml):** the kernel-mode driver is cached and only rebuilt when its *source* changes (cache keyed on `hashFiles('driver/**','include/**')`); a hit skips the WDK, the signing-cert generation, and the MSBuild. So `N` is no longer bumped on *every* CI run — it's the commit count at the **last commit that changed the driver**, stamped into the `.sys`/`.inf` when they were built. Still strictly monotonic across driver changes (PnP upgrades unaffected), and `build=N` still uniquely identifies the `.sys` bits — it just stops advancing on service-only commits. The cache stores the `.cer` *alongside* the `.sys`/`.cat`, so the cert the installer ships always matches the cached binaries (a freshly-generated cert per run would not). `build-installer.ps1` (local) is unchanged — still bumps every run. Bump the `v1` tag in the cache key to force a rebuild after changing build flags without touching driver source.

### Manual: prefix (`MAJOR.MINOR.BUILD`)

`DriverVersionPrefix` in `driver/StreamToSpeaker.vcxproj` — defaults to `1.0.0`. **Bump it by hand when the change is significant.** Semver-ish discipline:

- **MAJOR (`1.0.0 → 2.0.0`)** — breaking changes that aren't safe to roll back without uninstalling first. IOCTL ABI changes (modifying `stream_to_speaker_ioctl.h`), changing the device hardware-ID, changing PortCls→WaveRT to a different audio class, etc.
- **MINOR (`1.0.0 → 1.1.0`)** — new shipped features. Adding AirPlay support, adding alternative codecs, adding a new control device interface, adding a new PKEY in the INF.
- **BUILD (`1.0.0 → 1.0.1`)** — meaningful bug fixes worth flagging in the version string. The kind of thing a user grepping `1.0.X` would care about. (Note: this is the *third* INF field — not the auto-bumped revision.)

**Also bump `service/Cargo.toml`'s `version =` to match the prefix** when bumping MAJOR or MINOR — the service version shows up in `--version`, in `User-Agent` headers we send to speakers, and is what GitHub Releases use as the tag. (Patch-level disagreements between service and driver are fine; what matters is that the *prefix* tells the same story.)

Don't bump the prefix for routine fixes — the auto-bumped revision is the right granularity for "the bits changed, no semantic difference." Save prefix bumps for "the user should know something changed."

## Install / upgrade flow

End users install via `StreamToSpeakerSetup-<version>.exe` (produced by CI). The installer:

1. Imports our test-signing cert to TrustedPublisher + Root
2. **Cleans up any prior install** — `Pre-Install.ps1` removes the live device (`devcon remove`), unstages the old driver (`pnputil /delete-driver`), wipes cached `HKLM\…\MMDevices\Audio\Render\<id>` entries that would otherwise keep the old INF properties live
3. `pnputil /add-driver` stages the new driver
4. `devcon install` creates the root-enumerated device — `pnputil /install` alone does not for root-enumerated devices
5. `Rename-Endpoint.ps1` overwrites the cached friendly name + flips `DeviceState = ACTIVE` and restarts `AudioEndpointBuilder` so changes take effect without sign-out

User-machine prerequisites that aren't automatable:
- Test-signing mode (`bcdedit /set testsigning on` + reboot) — we ship a test-signed driver, not WHQL
- Secure Boot off, HVCI / Memory Integrity off — required for test-signed drivers to load
- Windows 10 1809+ (the INF targets `NTamd64.10.0...17763`)
- Click "Allow" once in Sound Settings for the per-device privacy gate (Windows 11 22H2+ only) — addressed by `PKEY_AudioDevice_EnableEndpointByDefault` in the INF, but pre-existing endpoints created without that property still need the manual click

## Key knowledge — Windows-audio specifics learned the hard way

| Symptom | Real cause | Fix |
|---|---|---|
| Device label is "Internal AUX Jack" | Windows derives the prefix from `KSNODETYPE_LINE_CONNECTOR` association | Use `KSNODETYPE_SPEAKER` in INF + `PKEY_Device_FriendlyName` to override cached name |
| New install shows old endpoint name / state | `HKLM\…\MMDevices\Audio\Render\<id>` is cached per endpoint ID and survives reinstalls | `Reset-Install.ps1` wipes the cached entries; the installer now runs the cleanup automatically |
| Device installed but no Sound Settings endpoint | `pnputil /add-driver /install` doesn't create root-enumerated devices | Use `devcon install <inf> Root\<HardwareId>` after pnputil |
| "Allow apps and Windows to use this device" toggle | Endpoint builder creates certain KSNODETYPE / form-factor combinations as disabled+hidden by default | `PKEY_AudioDevice_EnableEndpointByDefault = 0x101` (FLAG_ENABLE \| FLOW_MASK_RENDER) in INF |
| OEM APO (Dolby Atmos etc.) attaches and adds latency | FormFactor=1 (Speakers) is the default match for OEM APOs | `PKEY_AudioEndpoint_Disable_SysFx = 1` in INF |
| BSOD 0x9F sub-code 3 | A device our driver owns failed to complete an `IRP_MJ_POWER` in time | Don't just override `MJ_DEVICE_CONTROL` / `CREATE` / `CLOSE`; also own `MJ_POWER` and route by device — our control device needs to complete with `STATUS_SUCCESS` + `PoStartNextPowerIrp`, audio FDO forwards to PortCls |
| Driver loaded but `build=N` shows old N in service log | `pnputil /add-driver` only stages; running kernel keeps old `.sys` | Device Manager → device → Disable/Enable, or reboot |

## Debugging recipes

- **BSOD**: minidumps at `C:\Windows\Minidump\*.dmp`. Parse the header in Python:
  ```python
  with open(dump, 'rb') as f: h = f.read(0x400)
  bugcheck = struct.unpack_from('<I', h, 0x38)[0]
  params = [struct.unpack_from('<Q', h, 0x40+i*8)[0] for i in range(4)]
  ```
- **What's in the driver store**: `pnputil /enum-drivers | Select-String "StreamToSpeaker" -Context 1,8`
- **Is the device alive**: `devcon status Root\StreamToSpeaker`
- **Why did PnP refuse**: `Get-Content C:\Windows\INF\setupapi.dev.log -Tail 200 | Select-String StreamToSpeaker -Context 2,5`
- **Audio engine state**: `mmsys.cpl` shows all endpoints incl. disabled / hidden ones
- **Driver DPC running?**: kernel `DBG_INFO` in `wavestream.cpp:DoCopyToRing` logs every ~1 s; capture with DebugView (run as admin, enable Kernel capture)

## CI workflow

`.github/workflows/build.yml` runs on `windows-latest` for every push, PR, and tag:

1. Install Inno Setup (chocolatey)
2. Restore / install WDK (cached — see workflow for keys)
3. Generate self-signed code-signing cert, import to local trust stores
4. **Bump driver build to git commit count**
5. Build driver with `SignMode=TestSign`
6. Build service (`cargo build --release`)
7. Stage artifacts into `installer/staging/` (sys, inf, cat, devcon.exe, cert)
8. Run `ISCC.exe` to produce `installer/out/StreamToSpeakerSetup-<version>.exe`
9. Upload `.exe` + raw binaries as artifacts
10. On `v*` tag push: attach `.exe` to GitHub Release

WDK install (~5 min cold) is cached; warm cache restores in ~30 s.

## Future work — explicitly deferred

The user mentioned these but asked NOT to start them now. Pick up when they say:

- **AirPlay — implemented; remaining work + caveats.** The AirPlay sender lives in `service/src/airplay/` and is wired into discovery + the picker (each device is classified `RaopLegacy` vs `AirPlay2` via the `_airplay._tcp` `features`/`ft` bits; HomePods — `model AudioAccessory*` — always route to AirPlay 2). Two paths:
    - **AirPlay 1 / RAOP** — unencrypted (`et=0`) or Apple-RSA (`et=1`): AirPort Express, shairport-sync, Sonos/Apple-TV in legacy mode. `rtsp.rs` / `rtp.rs` / `alac.rs` / `crypto.rs` / `timing.rs` / `session.rs`. (This was already working before; left intact.)
    - **AirPlay 2 / HomeKit transient pairing** — HomePod + AP2-only receivers: `tlv8.rs` + `srp.rs` + `pairing.rs` + `ap2_crypto.rs` + `ap2_rtsp.rs` + `ap2_session.rs`. SRP-6a (3072-bit / SHA-512, PIN `3939`, flag `0x10`) → HKDF-SHA512 → ChaCha20-Poly1305 encrypted RTSP + per-packet audio. **No FairPlay/DRM blob needed** — transient pairing alone is sufficient (confirmed against OwnTone). Audio is the same uncompressed-ALAC frame as RAOP, ChaCha-sealed (`shk` = pairing secret[0..32]).

  **Verified vs. not:** the crypto core is unit-tested (SRP against the RFC 5054 vector; TLV8/HKDF/channel-cipher/audio-seal round-trips); the PTP packet codec + offset math and the resend buffer are unit-tested too. The *live* pairing + stream — and **especially the PTP handshake** — have **not** been validated against a real HomePod; that needs the user's hardware. Status of the formerly-deferred items:
    - **Timing-mode routing (field-tested): bit 41 (`SupportsPTP`) → PTP, else NTP.** Two field experiments on a current-firmware SYMFONISK: (1) PTP without Signaling — full bring-up accepted (SETUP/SETPEERS/RECORD 200, audio + 0xD7 sync flowing) but the receiver never sent one PTP packet back → no lock → silent; (2) NTP — `SETUP(stream)` *stalls* (current Sonos fw treats NTP as vestigially as RAOP; NTP-mode stream SETUP had returned 200 on this device only in older sessions). Conclusion: PTP is required for bit-41 receivers, and the missing engagement trigger was libairptp's **Signaling** (now implemented — see below). The PTP master logs every first inbound message type and warns after 5 s of receiver silence, so the next log always shows *how* a receiver engages. ⚠️ Do NOT send unsolicited datagrams between the two SETUPs (a "firewall priming" packet to the receiver's timing port was the prime suspect for stalling `SETUP(stream)`; removed). A session that fails mid-bring-up MUST still TEARDOWN — `Ap2Rtsp` now does it on Drop once paired — otherwise the receiver holds the half-open session and retries time out at pairing for tens of seconds.
    - **PTP timing — IMPLEMENTED (`ap2_ptp.rs`), sender-as-grandmaster, full libairptp parity.** Receivers we route to PTP negotiate `timingProtocol: PTP`. **The sender is the PTP grandmaster; the receiver follows our clock** (NOT the other way round — an earlier follower design left the receiver with no timeline: it decrypted audio but never scheduled it → "playing" but silent). Implementation mirrors OwnTone's `libairptp` exactly: unicast Announce (1 s, port 320, **with PATH_TRACE TLV** carrying the clock id) + two-step Sync (125 ms, port 319) + Follow_Up (320) + Delay_Resp (logInterval −3) on the receiver's Delay_Req + **Apple-proprietary Signaling every 1 s** (org-extension TLVs, Apple OUI `00:0d:93`, subtypes 1/22B and 5/32B, payloads starting `00 00 03 01`, targetPortIdentity zeros, logInterval −128) — iOS senders emit these and receivers appear to gate engagement on them; flags `UNICAST|TIMESCALE(|TWO_STEP)`; Announce grandmaster fields priority1/2=128, clockClass=0x06, accuracy=0x21, variance=0x436A, timeSource=0x20; sourcePortIdentity port 0x8005. The clock served is **monotonic** (Instant since session start), and the `0xD4` sync packet is stamped from the *same clock + NTP 1900-epoch delta* (OwnTone's consistency contract). SETUP carries `timingPeerInfo` AND `timingPeerList` with `ID` (UUID string), `ClockID` (int64), `DeviceType:0`, `SupportsClockPortMatchingOverride:false`, `Addresses`; `SETPEERS` lists receiver first, then sender. Non-PTP receivers keep the NTP path. Other AP2 session-bring-up requirements learned from hardware: the **event channel TCP connection** (to the first SETUP's `eventPort`) must be open or RECORD times out, and each realtime audio packet's on-wire layout is `[RTP header][ciphertext][16-byte tag][8-byte nonce]` (missing nonce suffix = every packet fails auth = silence). Refs: OwnTone `airplay.c` + `owntone/libairptp` (`ptp_msg_handle.c`, `ptp_definitions.h`).
    - **Retransmit/resend — IMPLEMENTED (both paths).** `ResendBuffer` (~4 s ring) + `spawn_resend_responder` in `timing.rs`: listens on the control socket for `0x80 0xD5` resend requests and re-sends matching packets wrapped in `0x80 0xD6`. Wired into RAOP (`rtp.rs`/`session.rs`) and AP2 (`ap2_session.rs`); the control socket is shared with the sync sender via `try_clone`.
    - **auth-setup — IMPLEMENTED (RAOP, `rtsp.rs`).** An ANNOUNCE `403` triggers the MFi `/auth-setup` X25519 handshake (`0x01` unencrypted selector) and one retry. **403-gated**, so it cannot regress receivers that already work (OwnTone disables auth-setup by default; the 403 trigger is our safety valve). Adds the `x25519-dalek` dep.
    - **Persistent pairing — STILL DEFERRED.** Only transient pairing is implemented. Full pair-setup (M1–M6, Ed25519) + pair-verify (X25519) is not — but the devices that would need it (Apple TV with PIN) also expose legacy RAOP, so they work today.
    - **Buffered audio — IMPLEMENTED (type 103/ALAC over TCP), UNVERIFIED vs hardware.** This is the stream kind iOS actually uses (feature bit 40) — and after full realtime+PTP parity still played silent on the Sonos, the hypothesis is that current Sonos fw only truly *plays* buffered. Receivers with bit 40 + PTP now get: `SETUP` type 103 (same dict as realtime, `ct=2`/ALAC first — the openairplay reference receiver accepts buffered-ALAC; if Sonos rejects it the SETUP fails *visibly* and we fall back to realtime, and the next step would be an AAC-LC encoder, mind fdk-aac licensing), one **TCP** connection to the returned dataPort carrying `[u16 BE length incl. itself][RTP header][sealed payload|tag|nonce]` frames (same ChaCha seal as realtime), **`SETRATEANCHORTIME`** after RECORD (`rate:1`, `rtpTime`, `networkTimeSecs`+`networkTimeFrac` (64-bit binary fraction — semantics verified against shairport-sync's handler), `networkTimeTimelineID` = our PTP clock id, anchor = now+500 ms), no 0xD4/0xD7 sync and no resend under buffered. Also added the **`POST /feedback` keepalive every 2 s** (both stream kinds) that iOS senders emit. Refs: openairplay airplay2-receiver (`ap2/connections/audio.py`, `ap2-receiver.py`), shairport-sync `rtsp.c`, openairplay-spec. Remaining if buffered-ALAC is refused or silent: AAC-LC encode; and `FLUSHBUFFERED` on stop/seek (currently TEARDOWN only).

  **References used (authoritative):** OwnTone `src/outputs/{raop,airplay}.c` + `src/pair_ap/`, the openairplay + emanuelcozzi.net specs, pyatv, RFC 5054 / RFC 3526. **Do not anchor on `imadal1n/airsink`** — zero stars, has compatibility issues; it was only a hypothesis source and every load-bearing constant was re-verified against OwnTone/pair_ap.

- **Lower-latency formats / alternative encodings**. Currently L16 PCM 44.1 kHz stereo. Options to add: 24-bit / 96 kHz for hi-fi; FLAC (compressed, lossless, lower bandwidth — most UPnP speakers support `audio/flac`); Opus (lossy, ultra-low-latency, used in WebRTC; less broadly supported by consumer speakers); AAC (broad compatibility, less optimal for low latency). Architecture-wise: the driver only knows L16 44.1; alternative encodings would be in the service's HTTP output path (encode at packet boundary, ship in the speaker's preferred MIME). Negotiation could be via DIDL `protocolInfo` listing multiple `<res>` lines.

- **License + donations for EV code signing**. We currently ship a test-signed driver; users have to flip `bcdedit /set testsigning on` and reboot, which is a non-trivial UX cliff and rules out anyone with Secure Boot / HVCI / Memory Integrity on. WHQL-signed kernel drivers need an EV (Extended Validation) code-signing certificate — ~$400-$600/year from a CA, plus hardware token shipping, plus a Microsoft Partner Center account, plus going through the HLK / WHQL submission flow. Pre-work: (a) pick a license for the repo (likely something permissive like MIT/Apache-2.0 so contributors / package maintainers can ship it; check whether any of our existing crate deps would force GPL/copyleft); (b) add a "support" / "donate" link to the README + GUI (GitHub Sponsors or Open Collective — covers ongoing cert renewal); (c) figure out who'd own the cert legally (individual cert vs. an org). Once funded, the migration is: get the cert → switch the CI sign step from self-signed to EV → submit to Partner Center → ship a properly-signed driver and drop the `testsigning` requirement.

## What I should NOT do

- Don't `process::exit` from anywhere except `main`. Use `app.request_shutdown()` and let the loop unwind.
- Don't add Power IRP completions without `PoStartNextPowerIrp` even though it's deprecated — older WHQL testers complain.
- Don't commit `service/target/` (covered by `.gitignore` now; broke the repo once).
- Don't push `windows_subsystem = "console"` for the GUI build — the brief console flash is a real UX bug. `"windows"` + `AttachConsole(ATTACH_PARENT_PROCESS)` for `--headless` is the right pattern.
- Don't ignore the user when they push back. They know what they observe; if my diagnosis doesn't match their experience, my diagnosis is probably wrong.

## Conventions for future work

- **App / tray icons must have an active and a non-active variant.** When we eventually replace the procedurally-drawn tray icon with proper artwork (or add an app icon), ship at minimum two states: one for "streaming active" (full color / accent fill / animated sound waves) and one for "idle / no speaker / disabled" (desaturated or outlined). The tray icon is the only persistent UI signal when the window is hidden; a single static icon hides whether the app is actually doing its job. Same applies if we ever ship a taskbar icon. Sizes: 16×16, 24×24, 32×32, 48×48, 256×256 (Windows tray scales down a single 32×32 fine, but the 16×16 master should be hand-tuned — auto-downscale loses detail).
