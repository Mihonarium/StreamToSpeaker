# Microsoft-attested driver package

The kernel driver as signed by Microsoft via Partner Center attestation.
These exact bytes ship in release installers; they load on stock Windows
(testsigning OFF, Secure Boot ON) with no certificate imports.

**Do not edit these files.** To update after a driver-source change:
build via CI, run `installer/Make-SubmissionCab.ps1` on the
`binaries-<version>` artifact, submit the CAB at
https://partner.microsoft.com/dashboard/hardware (attestation, x64
signatures for Win10 1809+ / Win11), download the signed package, and
replace these three files (the returned `streamtospeaker.cat` is renamed
`StreamToSpeaker.cat` to match the INF's CatalogFile line).

CI stages this package for the installer whenever `StreamToSpeaker.inf`
here is byte-identical to the freshly built driver's INF (the INF embeds
DriverVer, which only changes when driver source is rebuilt — so
equality proves the built driver is the attested driver). On mismatch,
tag builds fail; dev builds fall back to the test-signed driver + cert.

## Provenance

- Submitted: 2026-08-04, CAB `StreamToSpeaker-1.1.0.1.cab` (sys + inf + pdb),
  EV-signed by High Expected Value LTD (Certum)
- Partner Center submission package: `Signed_1152921505701579794`
- DriverVer: 1.1.0.1 (from the `binaries-0.0.1` CI artifact, tag v0.0.1)
- Catalog signer: Microsoft Windows Hardware Compatibility Publisher
  (← Microsoft Windows Third Party Component CA 2014 ← Microsoft Root CA 2010)
- The .sys keeps the CI test-cert signature at index 0 with Microsoft's
  appended after it; PnP validates via the Microsoft-signed catalog.
- INF sha256: c6dbdf015e306a135e36510da9a3987cbdae137e51e38cdb3e795aa3d81fee4e
