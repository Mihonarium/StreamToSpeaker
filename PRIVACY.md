# Privacy Policy

**Stream To Speaker collects nothing.** There are no servers, no accounts, no
analytics, no telemetry, and no crash reporting. Nothing about you or your
computer is sent to us, because there is nowhere for it to be sent.

Last updated: 7 August 2026.

## What leaves your computer

Only your audio, only to the speaker you pick, and only across your own
network:

- **Discovery.** The app broadcasts standard discovery messages (SSDP for
  UPnP/OpenHome speakers, mDNS for AirPlay) on your local network to find
  speakers, and listens for their replies.
- **Playback.** Once you select a speaker, the audio Windows is playing is
  streamed to that speaker's address on your local network, along with the
  control messages needed to start, stop and set volume.

None of this traffic leaves your network or passes through us.

## What is stored on your computer

- **Settings** — `%APPDATA%\Stream To Speaker\config.json`: your last chosen
  speaker, latency and reconnect preferences, and, if you use AirPlay
  speakers that require them, **speaker passwords and pairing keys**. These
  are stored in plain text in your user profile so the app can reconnect
  without asking again. Delete the file to clear them.
- **Log file** — diagnostic output, including speaker names and local IP
  addresses. It stays on your machine; it is only shared if you choose to
  attach it to a bug report.

Uninstalling removes the program. Delete the folder above to remove settings
and logs as well.

## The optional web interface

Stream To Speaker can expose a small web page and HTTP API for controlling it
from another device. **This is off unless you start the app with `--web`.**
When enabled it has no authentication and is reachable by anything on your
network, so enable it only on networks you trust — or bind it to your own
machine with `--bind 127.0.0.1`.

## Links to other sites

The app has links that open your browser: the project page on GitHub, and a
donation page on Buy Me a Coffee. Those sites are run by other companies and
have their own privacy policies. Nothing is sent to them unless you click.

## Children

The app is not directed at children and collects no personal information from
anyone.

## Changes

Any change to this policy will be committed to this file, so its full history
is public.

## Contact

Questions: open an issue at
<https://github.com/Mihonarium/StreamToSpeaker/issues>.
