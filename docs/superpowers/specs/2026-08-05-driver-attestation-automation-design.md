# Automated Microsoft driver attestation — design

Date: 2026-08-05. Status: approved.
Rendered version (with pipeline diagram): https://data.claude.ms/attest-design-x3kq/

## Goal

Replace the manual middle of the driver signing pipeline (browser submission at
the Partner Center dashboard, hand-attaching Microsoft's zip) with the Partner
Center Hardware API, so that the only human action between "push a driver
change" and "installers bundle a Microsoft-attested driver" is the existing
EV-signing approval click in the signing repo.

Builds on the PR #25 branch (`claude/driver-signing-release-ci-u51pj7`), which
already implements: unsigned driver build → submission CAB → `driver-v<ver>`
prerelease with `manifest.json` → EV-signing via the central signing repo →
(manual gap) → signature/INF verification → canonical `-Signed.zip` →
installer bundling by source hash.

## Decisions taken

- **Fully automatic** — no second approval gate. The EV-signing approval
  already vouches for the exact CAB bytes; the Entra app can only submit to
  our own Partner Center account.
- **Reconciler architecture** (chosen over a linear job appended to
  `driver-submission.yml` and over a cron poller): a new idempotent workflow
  advances durable state recorded in `manifest.json`; any failure is repaired
  by re-running.
- **Lands on the PR #25 branch** as new commits, after a verification pass
  over that branch's existing content (do not trust it blindly).

## New workflow: `driver-attest.yml`

Runner: `ubuntu-latest` (pure REST; Windows is only needed in verification,
which is a separate reusable workflow).

Triggers:
- `workflow_run` on "Driver submission" completing on `main` (fires after the
  EV-signed CAB has been swapped onto the release);
- `workflow_dispatch` with optional `tag` input — the universal retry.

Concurrency: group `driver-attest`, `cancel-in-progress: false`.

### Reconcile logic

Read `manifest.json` from the newest `driver-v*` release (or the explicit
`tag`), then advance whatever state it finds:

| Manifest state | Action |
| --- | --- |
| `attested: true` | Exit green — nothing to do. |
| `submission_cab_signed_sha256` is null | Exit with a clear message: EV signing hasn't completed; nothing submitted. |
| No `ms_submission_id` | Get token (client credentials). Create product + initial submission. **Persist `ms_product_id`/`ms_submission_id`/`ms_state: created` to the release manifest immediately, before any upload** — a crash after this point can never cause a duplicate Partner Center submission. |
| `ms_state: created` | Download the EV-signed CAB from the release, verify its SHA-256 equals `submission_cab_signed_sha256`, upload to the submission's SAS URL (`PUT` with `x-ms-blob-type: BlockBlob`), set `ms_state: uploaded`. |
| `ms_state: uploaded` | Commit the submission, set `ms_state: committed`. |
| `ms_state: committed` | Poll submission status every 30 s (job `timeout-minutes: 50`; attestation is typically ~10 min). On Microsoft failure: dump the submission's error payload to the step summary, set `ms_state: failed`, exit red. On poll timeout: exit red, state stays `committed` — re-run resumes polling. |
| Workflow done | Download the signed package via the submission's download links, attach to the release as `StreamToSpeaker-Driver-<ver>-Microsoft.zip`, set `ms_state: done`, invoke verification. |
| `ms_state: failed` | Exit red with the recorded error; a fresh driver build (new release) is the way forward, or manual dispatch after fixing account-level issues. |

The manifest is the single source of truth; the release asset list is derived,
never authoritative.

### Product settings

Mirror the existing "Stream to Speaker" product (id 14636198218617861):
`driverType: desktop`, attestation test harness, `deviceType:
internalExternal`, no test/flight signing, and the Windows 10 1809+ / Windows
11 x64 `requestedSignatures` list. Product name:
`StreamToSpeaker-Driver-<version>`. One product per driver version (the
Hardware API's initial-submission model).

### API flow (verified working 2026-08-05 from this account)

- Token: `POST https://login.microsoftonline.com/<tenant>/oauth2/token` with
  `grant_type=client_credentials`,
  `resource=https://manage.devcenter.microsoft.com` (v1 endpoint; tokens last
  1 h — fetch per run).
- Base: `https://manage.devcenter.microsoft.com/v2.0/my/hardware/products`.
- Create product → create submission (returns SAS upload URL) → blob `PUT` →
  commit → poll → download items of type signed package.

## Changes to existing files

- `driver-attested.yml`: verify + finalize logic extracted into a reusable
  workflow (`workflow_call` with a `tag` input). The `workflow_dispatch` entry
  point remains as the manual fallback (browser submission + hand-attached
  zip), running the identical checks. No verification is weakened: signtool
  `/kp` catalog + embedded checks, Microsoft Windows Hardware Compatibility
  Publisher chain requirement, byte-identical INF, DriverVer match.
- `manifest.json` schema additions: `ms_product_id`, `ms_submission_id`,
  `ms_state` (`created → uploaded → committed → done | failed`),
  `ms_zip_asset`.
- `docs/driver-signing.md`: rewritten around the automatic flow; the manual
  Partner Center steps shrink to a fallback appendix. Documents the secret
  expiry failure mode (`invalid_client`).
- `driver-submission.yml` release notes: runbook steps 2–5 collapse to "the
  Driver attest workflow takes it from here".

## Secrets (repository secrets, StreamToSpeaker)

`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` — the Entra app
associated with the Partner Center account (Manager role; token issuance and
product listing verified 2026-08-05). No environment gate.

## Phase 0: verify the PR #25 base

Before stacking new work, the PR #25 branch is treated as untrusted input:

1. Line-by-line review of all five changed areas (`driver-submission.yml`,
   `driver-attested.yml`, `.github/actions/request-signing/action.yml`,
   `build.yml` integration, `installer/StreamToSpeaker.iss`) for correctness,
   injection-safety of workflow inputs, and failure handling.
2. Cross-check its claims against Microsoft's attestation documentation: CAB
   layout (files under a subdirectory, none at root), PDB inclusion, "only
   the CAB needs the EV signature", INF returned untouched.
3. Exercise what can run without a driver push: workflow YAML validation, the
   CAB packing and manifest logic, the request-signing action's hash-pinning
   behavior.
4. Findings are fixed on the same branch before the new work lands.

## Testing

The Hardware API has no sandbox. Component-level: YAML validation and a
dry-run of the reconcile script's state transitions against fixture manifests.
Integration: the first real driver push is watched end to end (submission IDs,
poll transcript, and verification output all land in run summaries).

## Risks

- **Junk products**: products cannot be deleted via the API; a failed run can
  leave an empty product behind (harmless, invisible to users). The
  persist-IDs-first ordering caps it at one per driver version.
- **Secret expiry** (≤ 24 months): fails as `invalid_client` in this workflow;
  documented, with a rotation reminder.
- **Microsoft latency**: usually ~10 min, occasionally hours. Poll timeout
  leaves state resumable; re-run (manual dispatch) continues. No cron.
