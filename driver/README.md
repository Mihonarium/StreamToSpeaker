# stream-to-speaker driver

A WaveRT virtual audio render endpoint that captures the PCM the
Windows audio engine writes to it and exposes that PCM to a user-mode
service over a StreamToSpeaker-specific IOCTL contract.

The driver presents itself in Windows Sound settings as **StreamToSpeaker**.
It does no audio processing of its own; volume / mute / stream state
events are forwarded out to the user-mode service which mirrors them
to the real Sonos speaker over UPnP.

## File layout

```
driver/
  StreamToSpeaker.sln              MSBuild solution (VS 2022)
  StreamToSpeaker.vcxproj          Project, WDM + KMDF disabled, x64 only
  StreamToSpeaker.vcxproj.filters  Source filter view
  StreamToSpeaker.inf              INF for installation
  driver.h                   Common declarations
  driver.cpp                 DriverEntry, dispatch glue, PnP
  adapter.cpp                PcAddAdapterDevice, StartDevice
  wave.h / wave.cpp          IMiniportWaveRT
  wavestream.h / wavestream.cpp
                             IMiniportWaveRTStream (capture DPC)
  topology.h / topology.cpp  IMiniportTopology + KS properties
  mintopo.h                  Topology pin/node indexes
  minwave.h                  Wave pin indexes + format
  ringbuffer.h / ringbuffer.cpp
                             SPSC byte ring buffer
  ioctl.h / ioctl.cpp        IRP_MJ_DEVICE_CONTROL dispatch +
                             pended-IRP queues
  debug.h                    DbgPrint verbosity wrappers
```

## Build prerequisites

- Windows 11 22H2 or later
- Visual Studio 2022 17.5+ with the *Desktop development with C++* workload
- **Windows Driver Kit (WDK) 10.0.22621.x** or newer
  (`winget install Microsoft.WindowsWDK`)
- The matching **Windows SDK** of the same build number
- Spectre-mitigated MSVC libraries (installed by default with VS 2022)

The project targets WDM with the WaveRT port driver. KMDF is **not**
linked in — the IOCTL queues are raw IRP lists guarded by spin locks.

## Building

From a Visual Studio 2022 *Developer Command Prompt*:

```cmd
cd driver
msbuild StreamToSpeaker.sln /p:Configuration=Debug /p:Platform=x64
```

Or from inside Visual Studio: open `StreamToSpeaker.sln`, set the
configuration to `Debug | x64`, Build > Build Solution.

Output is `x64\Debug\StreamToSpeaker.sys` and `StreamToSpeaker.cat`. The build
will self-sign the catalog with the WDK test cert.

## Installing (test machine)

```powershell
# One-time:
bcdedit /set testsigning on
# Reboot.

cd driver\x64\Debug
pnputil /add-driver StreamToSpeaker.inf /install
```

Open *Sound settings* and you should see **Stream To Speaker** in the
output dropdown. Select it; any app that plays audio now feeds the
driver. The user-mode `stream-to-speaker` service picks the PCM up
via the IOCTL.

To uninstall:

```powershell
pnputil /enum-drivers | findstr StreamToSpeaker
pnputil /delete-driver oemNN.inf /uninstall
```

(Replace `oemNN.inf` with whatever pnputil reported.)

## Architecture notes

- One device, one render endpoint, one stereo PCM format
  (44.1 kHz / 16 / stereo).
- Wave filter has a single sink pin (data IN from the engine) and a
  bridge pin (logical OUT to topology).
- Topology filter chain: `VOLUME -> MUTE -> DAC`.
- WaveRT cyclic buffer is allocated by the driver in non-paged pool
  on `AllocateAudioBuffer`. The Windows audio engine writes into it.
- A periodic kernel timer at 2 ms (configurable via
  `STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS`) drives a DPC that:
  1. Measures elapsed QPC since the previous tick.
  2. Computes frames produced (rate * elapsed).
  3. Copies those frames out of the cyclic buffer into a kernel ring
     buffer.
  4. Drains any pended `IOCTL_STREAM_TO_SPEAKER_GET_AUDIO_PACKET` IRPs.
- The ring buffer is 32 KB (~186 ms at our format). Overrun policy is
  drop-oldest; we never block the audio engine.
- The driver does NOT apply volume to the PCM. The user-mode service
  forwards volume changes to the Sonos. The KS volume/mute setters
  fire `STREAM_TO_SPEAKER_CONTROL_EVENT` records into the event IOCTL queue.

## Troubleshooting

### Driver doesn't load

- Verify test signing is on:
  `bcdedit /enum {current} | findstr testsigning`
- Verify HVCI is off: *Windows Security > Device Security > Core
  Isolation > Memory Integrity = Off*.
- Check the Setup log: `%windir%\inf\setupapi.dev.log`.
- Inspect the System event log for *Service Control Manager* failures
  matching "StreamToSpeaker".

### Driver loads, no audio device appears

- Check `%windir%\inf\setupapi.dev.log` for INF parser errors.
- Confirm `KSCATEGORY_AUDIO` interface registration in the
  `[StreamToSpeaker_Inst.NT.Interfaces]` section is intact.
- Re-run `pnputil /add-driver StreamToSpeaker.inf /install` with elevated
  PowerShell; trailing whitespace or CRLF issues in the INF can cause
  silent failure.

### User-mode service can't find the device

- `Get-PnpDevice -Class Media | ?{ $_.FriendlyName -match 'Stream To Speaker' }`
  should list one device.
- Confirm `\\.\StreamToSpeaker` exists:
  `Get-ChildItem \\.\GLOBALROOT\??\StreamToSpeaker` (Windows syntax is
  awkward; alternatively use `[System.IO.File]::Exists('\\.\StreamToSpeaker')`).
- Run the service as Administrator the first time so it has rights to
  open the device handle.

### Glitchy audio / dropouts

- Increase `STREAM_TO_SPEAKER_NOTIFICATION_INTERVAL_MS` in `driver.h` to 5 or
  10 ms; rebuild. This trades latency for resilience.
- Increase `STREAM_TO_SPEAKER_RING_BYTES` to 64 KB.
- Confirm the user-mode service has MMCSS *Pro Audio* characteristics
  applied (it does this by default but a debugger attach can disable
  the privilege).
- Check `KeQuerySystemTimePrecise` jitter via WPA / xperf — a large
  DPC backlog elsewhere in the system will show up here first.

## Known limitations (v1)

- Single format only (44.1 kHz / 16-bit / stereo). Other formats are
  resampled by the Windows audio engine. 48 kHz / 24-bit support is
  tracked but not implemented.
- No offload mode, no spatial audio, no DRM (CopyProtect).
- No capture endpoint — render only.
- DataRangeIntersection ignores the client format on `MyDataRange[0]`
  and always returns our fixed format. The engine handles mismatch
  via its own resampler.
- Position tracking is software-derived from QueryPerformanceCounter
  rather than a real hardware position register. This is fine for a
  virtual device but means the engine cannot use polled HW-register
  position reads.

## Code style / conventions

- `_KERNEL_MODE` is defined for all sources.
- Pool tag is `'STSt'` (`STREAM_TO_SPEAKER_POOL_TAG`).
- DbgPrint level `DPFLTR_IHVAUDIO_ID` so messages show up alongside
  audio-stack logs in WPA.
- Pseudocode for any spinlocked region:
  ```
  KeAcquireSpinLock(&lock, &irql);
  // ...short critical section...
  KeReleaseSpinLock(&lock, irql);
  ```
  Critical sections do not call into the audio stack or allocate
  pageable memory.

## License

MIT. Significant portions derived from Microsoft's sysvad sample
(MIT), also MIT-licensed.
