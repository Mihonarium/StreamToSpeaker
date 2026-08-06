# License for released binaries

**Copyright © 2026 Stream To Speaker contributors. All rights reserved.**

This notice covers the **compiled, signed artifacts published on this
repository's releases** — the installer (`StreamToSpeakerSetup-*.exe`), the
service executable (`stream-to-speaker-*.exe`), and the driver packages
(`StreamToSpeaker-Driver-*-Signed.zip`, `*-Submission.cab`), including every
file inside them.

The **source code** is separately licensed under the Mozilla Public License
2.0 — see [`LICENSE`](LICENSE). Nothing here limits or alters your rights to
the source code under that license: you may read it, modify it, build your own
binaries from it, and distribute those, subject only to the MPL.

## Why these are licensed differently

The published binaries carry two signatures that the source code cannot give
you: an extended-validation code-signing certificate identifying us as the
publisher, and — for the kernel driver — a Microsoft attestation signature
obtained through the Windows Hardware Developer Program, which is what allows
the driver to load on stock Windows with Secure Boot enabled. Those signatures
represent an identity and a paid, audited process tied to us specifically.
Redistributing the signed binaries would put our identity behind software we
did not build and cannot vouch for, so the binaries themselves are not
redistributable even though the code behind them is open.

## What you may do

- Download, install and use the binaries, for any purpose, personal or
  commercial, on any number of machines.
- Verify them — check the code signature, the Microsoft signature, and the
  GitHub build-provenance attestation (see the README).
- Link to the official release page.

## What you may not do

- Redistribute, mirror, re-host, bundle, or otherwise distribute the signed
  binaries, in whole or in part, including inside another installer, package,
  or product.
- Modify the binaries, or strip, replace, or reuse any signature or catalog
  file from them.
- Use the driver binary or its catalog with software other than the
  Stream To Speaker service it was built for.
- Use the name "Stream To Speaker", or our logos, to identify your own builds
  or products. Trademark rights are reserved and are not licensed by the MPL.

**Want to distribute a build?** Build it from source under the MPL and sign it
yourself. If you want to ship *our* signed binaries — bundled with a product,
mirrored in a package manager, whatever — just ask; permission is often
granted and always cheap to request.

---

*Plain-language summary, not a substitute for the text above: the code is
open, our signatures are not.*
