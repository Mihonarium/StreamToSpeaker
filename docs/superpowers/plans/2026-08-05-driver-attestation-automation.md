# Automated Driver Attestation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the manual Partner Center browser step in the driver signing pipeline with an idempotent `driver-attest.yml` reconciler driven by the Hardware API, so one EV-approval click takes a driver push all the way to a Microsoft-attested, installer-bundled driver.

**Architecture:** A stdlib-only Python script (`.github/scripts/attest.py`) holds all reconcile logic (state machine over `manifest.json`, Hardware API client, release asset I/O via `gh`), unit-tested with `unittest`. A thin `driver-attest.yml` workflow runs it on `workflow_run` completion of "Driver submission" (+ manual dispatch as universal retry), then invokes the existing verification, refactored into a reusable workflow.

**Tech Stack:** GitHub Actions, Python 3 stdlib (urllib/json/subprocess), `gh` CLI (preinstalled on runners), Partner Center Hardware API v2, actionlint for workflow validation.

## Global Constraints

- Work happens in `/tmp/claude-1001/-home-dev/a5202ea2-f94d-4740-b394-575b6d19d801/scratchpad/sts-repo` on branch `pr25` (tracks PR #25 head `claude/driver-signing-release-ci-u51pj7`); push goes to that branch, NEVER to `main`.
- Git identity is already set repo-locally: `Mihonarium <ms@contact.ms>`. Commits end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- `attest.py` uses ONLY the Python standard library — no pip installs on the runner.
- API base: `https://manage.devcenter.microsoft.com/v2.0/my/hardware`. Token: v1 endpoint `https://login.microsoftonline.com/<tenant>/oauth2/token` with `resource=https://manage.devcenter.microsoft.com` (verified working 2026-08-05).
- Secrets (to be set on the repo in Task 8): `AZURE_TENANT_ID=4e42a1e3-2436-4a97-968f-612a22c608fc`, `AZURE_CLIENT_ID=28d798ca-3f21-4ee2-815e-19281d162b65`, `AZURE_CLIENT_SECRET=<from session>`.
- `requestedSignatures` mirrors the live "Stream to Speaker" product exactly: `WINDOWS_v100_X64_RS5_FULL, WINDOWS_v100_X64_19H1_FULL, WINDOWS_v100_X64_VB_FULL, WINDOWS_v100_X64_CO_FULL, WINDOWS_v100_X64_NI_FULL, WINDOWS_v100_X64_GE_FULL, WINDOWS_v100_X64_25H2_FULL, WINDOWS_v100_X64_26H1_FULL`.
- During development, NEVER POST to the Hardware API from this box (no sandbox exists); read-only GETs are allowed and encouraged.
- Manifest `ms_state` values: `created → uploaded → committed → done | failed` exactly as in the spec.

---

### Task 0: Verify the PR #25 base (Phase 0)

The branch is treated as untrusted input. Findings that require fixes become commits on this branch BEFORE later tasks.

**Files:**
- Read: `.github/workflows/driver-submission.yml`, `.github/workflows/driver-attested.yml`, `.github/actions/request-signing/action.yml`, `.github/scripts/Get-AttestedDriver.ps1`, `.github/workflows/build.yml`, `installer/StreamToSpeaker.iss`, `docs/driver-signing.md`
- Create: none (findings go into the final report to the user; fixes are commits)

- [ ] **Step 0.1: Static-validate all workflows with actionlint**

```bash
cd /tmp/claude-1001/-home-dev/a5202ea2-f94d-4740-b394-575b6d19d801/scratchpad
curl -fsSL https://github.com/rhysd/actionlint/releases/download/v1.7.7/actionlint_1.7.7_linux_amd64.tar.gz | tar xz actionlint
./actionlint -color sts-repo/.github/workflows/*.yml
```
Expected: no errors (warnings reviewed individually). Any error = finding.

- [ ] **Step 0.2: Review `request-signing/action.yml` line by line**

Checks: (a) it re-downloads the artifact URL and re-hashes before dispatching (the PR body claims this); (b) the dispatched digest is the locally computed one, not a caller input passed through; (c) polling matches the signing repo's run-name format `Sign sha256 <digest>`; (d) failure/timeout paths exit non-zero; (e) no secrets echoed.

- [ ] **Step 0.3: Review `driver-submission.yml` + `driver-attested.yml` against the already-read sources**

Checks: (a) `workflow_run`-relevant: the workflow `name:` is exactly `Driver submission` (the new trigger matches on this string); (b) manifest round-trips: fields written in submission are the ones read in attested (`submission_cab_signed_sha256` null vs `$null` JSON round-trip — PowerShell `ConvertTo-Json` renders `$null` as `null`, then `ConvertFrom-Json` gives `$null`; confirm the attest script's Python reads see JSON `null`); (c) `gh release upload --clobber` usage for manifest updates is atomic enough (single asset replace); (d) the stale-prerelease cleanup can't delete the release the attest reconciler is mid-flight on (it only deletes builds `< current` with `attested=false` and no in-flight marker — check interaction with `ms_state`, add guard if needed: a manifest with `ms_submission_id` set must NOT be auto-deleted); (e) injection-safety of inputs interpolated into PowerShell.

- [ ] **Step 0.4: Review `build.yml` integration + `installer/StreamToSpeaker.iss` diff vs main**

```bash
cd /tmp/claude-1001/-home-dev/a5202ea2-f94d-4740-b394-575b6d19d801/scratchpad/sts-repo
git diff origin/main...pr25 -- .github/workflows/build.yml installer/StreamToSpeaker.iss | head -400
```
Checks: source-hash pairing logic matches `driver-submission.yml`'s `hashFiles('driver/**','include/**')`; fallback to test-signed driver is clearly labelled; no cert import when attested.

- [ ] **Step 0.5: Cross-check attestation-doc claims**

Fetch and verify against `https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation` and `https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/create-a-new-hardware-submission-api`: (a) package files must sit in a subdirectory of the CAB; (b) the CAB (not the .sys) must carry the EV signature; (c) INF comes back untouched; (d) `.pdb` inclusion is recommended. Record any doc mismatch as a finding.

- [ ] **Step 0.6: Live-GET the existing product to lock the create-product payload**

```bash
# token as in the session; then:
curl -s -H "Authorization: Bearer $TOKEN" \
  "https://manage.devcenter.microsoft.com/v2.0/my/hardware/products/14636198218617861" | python3 -m json.tool
```
Record the full field set; adjust `build_product_payload` in Task 2 if the live product carries required fields beyond the ones listed there (e.g. `announcementDate`, `firmwareVersion`).

- [ ] **Step 0.7: Fix findings, commit each fix separately**

```bash
git add -p && git commit  # one commit per finding, message: "fix(pr25 review): <finding>"
```

### Task 1: `attest.py` skeleton + state machine (TDD)

**Files:**
- Create: `.github/scripts/attest.py`
- Test: `.github/scripts/test_attest.py`

**Interfaces:**
- Produces: `next_action(manifest: dict) -> str` returning one of `"done" | "wait-signing" | "create" | "upload" | "commit" | "poll" | "finalize" | "failed"`. Later tasks add `HardwareApi`, `build_product_payload`, `main` to the same module.

- [ ] **Step 1.1: Write the failing tests**

```python
# .github/scripts/test_attest.py
import unittest
import attest

def man(**kw):
    base = {"schema": 1, "driver_version": "1.0.0.42", "attested": False,
            "submission_cab_signed_sha256": "ab" * 32,
            "ms_product_id": None, "ms_submission_id": None, "ms_state": None}
    base.update(kw)
    return base

class NextAction(unittest.TestCase):
    def test_attested_is_done(self):
        self.assertEqual(attest.next_action(man(attested=True)), "done")
    def test_unsigned_cab_waits(self):
        self.assertEqual(attest.next_action(man(submission_cab_signed_sha256=None)), "wait-signing")
    def test_no_submission_creates(self):
        self.assertEqual(attest.next_action(man()), "create")
    def test_created_uploads(self):
        m = man(ms_product_id="1", ms_submission_id="2", ms_state="created")
        self.assertEqual(attest.next_action(m), "upload")
    def test_uploaded_commits(self):
        m = man(ms_product_id="1", ms_submission_id="2", ms_state="uploaded")
        self.assertEqual(attest.next_action(m), "commit")
    def test_committed_polls(self):
        m = man(ms_product_id="1", ms_submission_id="2", ms_state="committed")
        self.assertEqual(attest.next_action(m), "poll")
    def test_done_finalizes(self):
        m = man(ms_product_id="1", ms_submission_id="2", ms_state="done")
        self.assertEqual(attest.next_action(m), "finalize")
    def test_failed_is_failed(self):
        m = man(ms_product_id="1", ms_submission_id="2", ms_state="failed")
        self.assertEqual(attest.next_action(m), "failed")
    def test_unknown_state_raises(self):
        m = man(ms_product_id="1", ms_submission_id="2", ms_state="weird")
        with self.assertRaises(ValueError):
            attest.next_action(m)
    def test_missing_ms_keys_treated_as_absent(self):
        # manifests created by driver-submission.yml before this feature
        # have no ms_* keys at all
        m = man(); del m["ms_product_id"]; del m["ms_submission_id"]; del m["ms_state"]
        self.assertEqual(attest.next_action(m), "create")

if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 1.2: Run to verify failure**

Run: `cd .github/scripts && python3 -m unittest test_attest -v`
Expected: FAIL / ERROR — `attest` has no `next_action`.

- [ ] **Step 1.3: Implement `next_action`**

```python
# .github/scripts/attest.py
#!/usr/bin/env python3
"""Reconcile the newest driver-v* release toward Microsoft attestation.

State machine over manifest.json (see docs/superpowers/specs/
2026-08-05-driver-attestation-automation-design.md). Stdlib only.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "https://manage.devcenter.microsoft.com/v2.0/my/hardware"

def next_action(m):
    if m.get("attested"):
        return "done"
    if not m.get("submission_cab_signed_sha256"):
        return "wait-signing"
    if not m.get("ms_submission_id"):
        return "create"
    state = m.get("ms_state")
    if state in ("created", "uploaded", "committed", "done", "failed"):
        return {"created": "upload", "uploaded": "commit",
                "committed": "poll", "done": "finalize", "failed": "failed"}[state]
    raise ValueError(f"unknown ms_state {state!r}")
```

- [ ] **Step 1.4: Run tests, expect PASS**

- [ ] **Step 1.5: Commit** — `feat(attest): reconcile state machine`

### Task 2: Product payload + manifest mutation helpers (TDD)

**Files:**
- Modify: `.github/scripts/attest.py`
- Test: `.github/scripts/test_attest.py`

**Interfaces:**
- Produces: `build_product_payload(version: str) -> dict`; `apply_created(m, product_id, submission_id) -> dict`; `apply_state(m, state) -> dict`; `apply_done(m, zip_asset: str) -> dict` (all return the mutated manifest dict).

- [ ] **Step 2.1: Write failing tests**

```python
class Payload(unittest.TestCase):
    def test_product_payload(self):
        p = attest.build_product_payload("1.0.0.42")
        self.assertEqual(p["productName"], "StreamToSpeaker-Driver-1.0.0.42")
        self.assertEqual(p["testHarness"], "attestation")
        self.assertEqual(p["deviceType"], "internalExternal")
        self.assertFalse(p["isTestSign"]); self.assertFalse(p["isFlightSign"])
        self.assertIn("WINDOWS_v100_X64_RS5_FULL", p["requestedSignatures"])
        self.assertIn("WINDOWS_v100_X64_26H1_FULL", p["requestedSignatures"])
        self.assertEqual(len(p["requestedSignatures"]), 8)

class Mutations(unittest.TestCase):
    def test_apply_created_sets_ids_and_state(self):
        m = attest.apply_created(man(), "111", "222")
        self.assertEqual((m["ms_product_id"], m["ms_submission_id"], m["ms_state"]),
                         ("111", "222", "created"))
    def test_apply_done_records_zip(self):
        m = attest.apply_done(man(ms_state="committed"), "X-Microsoft.zip")
        self.assertEqual(m["ms_state"], "done")
        self.assertEqual(m["ms_zip_asset"], "X-Microsoft.zip")
```

- [ ] **Step 2.2: Run, expect FAIL; Step 2.3: implement**

```python
REQUESTED_SIGNATURES = [
    "WINDOWS_v100_X64_RS5_FULL", "WINDOWS_v100_X64_19H1_FULL",
    "WINDOWS_v100_X64_VB_FULL", "WINDOWS_v100_X64_CO_FULL",
    "WINDOWS_v100_X64_NI_FULL", "WINDOWS_v100_X64_GE_FULL",
    "WINDOWS_v100_X64_25H2_FULL", "WINDOWS_v100_X64_26H1_FULL",
]

def build_product_payload(version):
    # Field set mirrors live product 14636198218617861 (GET verified in
    # Task 0 step 0.6 — adjust here if that GET showed more required fields).
    return {
        "productName": f"StreamToSpeaker-Driver-{version}",
        "testHarness": "attestation",
        "deviceType": "internalExternal",
        "isTestSign": False,
        "isFlightSign": False,
        "deviceMetadataIds": [],
        "marketingNames": [],
        "selectedProductTypes": {},
        "additionalAttributes": {},
        "requestedSignatures": REQUESTED_SIGNATURES,
    }

def apply_created(m, product_id, submission_id):
    m["ms_product_id"] = product_id
    m["ms_submission_id"] = submission_id
    m["ms_state"] = "created"
    return m

def apply_state(m, state):
    m["ms_state"] = state
    return m

def apply_done(m, zip_asset):
    m["ms_state"] = "done"
    m["ms_zip_asset"] = zip_asset
    return m
```

- [ ] **Step 2.4: Run tests PASS; Step 2.5: Commit** — `feat(attest): product payload + manifest mutations`

### Task 3: Hardware API client

**Files:**
- Modify: `.github/scripts/attest.py`
- Test: `.github/scripts/test_attest.py`

**Interfaces:**
- Produces: `class HardwareApi` with `__init__(self, token)`, `request(self, method, path_or_url, body=None) -> dict|bytes`, `create_product(payload) -> dict`, `create_submission(product_id, name) -> dict`, `get_submission(product_id, submission_id) -> dict`, `commit(product_id, submission_id) -> dict`, `upload_blob(sas_url, data: bytes)`, `download(url) -> bytes`; module function `get_token(tenant, client_id, secret) -> str`; helper `find_download(submission: dict, dtype: str) -> str|None` (returns the URL of the downloads item with `"type" == dtype`, e.g. `initialPackage` / `signedPackage`).

- [ ] **Step 3.1: Failing tests for the pure helper**

```python
class FindDownload(unittest.TestCase):
    SUB = {"downloads": {"items": [
        {"type": "initialPackage", "url": "https://sas/init"},
        {"type": "signedPackage", "url": "https://sas/signed"}]}}
    def test_finds_signed(self):
        self.assertEqual(attest.find_download(self.SUB, "signedPackage"), "https://sas/signed")
    def test_missing_returns_none(self):
        self.assertIsNone(attest.find_download({"downloads": {"items": []}}, "signedPackage"))
    def test_no_downloads_key(self):
        self.assertIsNone(attest.find_download({}, "signedPackage"))
```

- [ ] **Step 3.2: Implement client (helper + I/O methods; I/O is exercised live in Task 7, not unit-mocked)**

```python
def find_download(sub, dtype):
    for item in (sub.get("downloads") or {}).get("items", []):
        if item.get("type") == dtype:
            return item.get("url")
    return None

def get_token(tenant, client_id, secret):
    body = urllib.parse.urlencode({
        "grant_type": "client_credentials", "client_id": client_id,
        "client_secret": secret,
        "resource": "https://manage.devcenter.microsoft.com"}).encode()
    req = urllib.request.Request(
        f"https://login.microsoftonline.com/{tenant}/oauth2/token", data=body)
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)["access_token"]

class HardwareApi:
    def __init__(self, token):
        self.token = token

    def request(self, method, path_or_url, body=None):
        url = path_or_url if path_or_url.startswith("http") else API + path_or_url
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/json",
            **({"Content-Type": "application/json"} if data else {})})
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                raw = r.read()
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace")[:2000]
            raise RuntimeError(f"{method} {url} -> HTTP {e.code}: {detail}") from e
        return json.loads(raw) if raw else {}

    def create_product(self, payload):
        return self.request("POST", "/products", payload)

    def create_submission(self, product_id, name):
        return self.request("POST", f"/products/{product_id}/submissions",
                            {"name": name, "type": "initial"})

    def get_submission(self, product_id, submission_id):
        return self.request("GET", f"/products/{product_id}/submissions/{submission_id}")

    def commit(self, product_id, submission_id):
        return self.request("POST",
                            f"/products/{product_id}/submissions/{submission_id}/commit")

    def upload_blob(self, sas_url, data):
        req = urllib.request.Request(sas_url, data=data, method="PUT", headers={
            "x-ms-blob-type": "BlockBlob",
            "Content-Type": "application/octet-stream"})
        with urllib.request.urlopen(req, timeout=600) as r:
            if r.status not in (200, 201):
                raise RuntimeError(f"blob upload HTTP {r.status}")

    def download(self, url):
        with urllib.request.urlopen(url, timeout=600) as r:
            return r.read()
```
(Requires `import urllib.parse` in the module header.)

- [ ] **Step 3.3: Tests PASS; commit** — `feat(attest): hardware API client`

### Task 4: Release I/O + main orchestration (TDD via fakes)

**Files:**
- Modify: `.github/scripts/attest.py`
- Test: `.github/scripts/test_attest.py`

**Interfaces:**
- Consumes: everything above.
- Produces: `class Release` (methods `load(tag: str|None) -> (tag, manifest)`, `save_manifest(manifest)`, `download_asset(name) -> bytes`, `upload_asset(path)`) implemented over the `gh` CLI; `reconcile(api, rel, manifest, tag, now=time.time, sleep=time.sleep, deadline_s=2700) -> str` returning the terminal action string; `main(argv) -> int` wiring env vars (`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `GH_TOKEN`, `INPUT_TAG`, `GITHUB_OUTPUT`, `GITHUB_STEP_SUMMARY`).

- [ ] **Step 4.1: Failing tests for `reconcile` with fakes**

```python
class FakeApi:
    def __init__(self, poll_results):
        self.calls = []; self.poll_results = list(poll_results)
    def create_product(self, payload):
        self.calls.append(("create_product", payload["productName"]))
        return {"id": 111}
    def create_submission(self, pid, name):
        self.calls.append(("create_submission", pid))
        return {"id": 222, "downloads": {"items": [
            {"type": "initialPackage", "url": "https://sas/up"}]}}
    def get_submission(self, pid, sid):
        return self.poll_results.pop(0)
    def commit(self, pid, sid):
        self.calls.append(("commit", pid, sid)); return {}
    def upload_blob(self, url, data):
        self.calls.append(("upload_blob", url, len(data)))
    def download(self, url):
        self.calls.append(("download", url)); return b"ZIPBYTES"

class FakeRelease:
    def __init__(self, manifest, cab=b"CABBYTES"):
        self.manifest = manifest; self.cab = cab; self.saved = []; self.uploaded = []
    def save_manifest(self, m):
        self.saved.append(json.loads(json.dumps(m)))
    def download_asset(self, name):
        return self.cab
    def upload_asset(self, path):
        self.uploaded.append(path)

DONE_SUB = {"commitStatus": "commitComplete",
            "workflowStatus": {"currentStep": "finalizeIngestion", "state": "completed"},
            "downloads": {"items": [{"type": "signedPackage", "url": "https://sas/dl"}]}}

class Reconcile(unittest.TestCase):
    def test_full_run_from_scratch(self):
        import hashlib
        m = man(submission_cab_signed_sha256=hashlib.sha256(b"CABBYTES").hexdigest())
        rel = FakeRelease(m)
        api = FakeApi([DONE_SUB])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "finalize")
        # IDs persisted BEFORE upload:
        self.assertEqual(rel.saved[0]["ms_state"], "created")
        self.assertTrue(any(c[0] == "upload_blob" for c in api.calls))
        self.assertEqual(rel.saved[-1]["ms_state"], "done")
    def test_sha_mismatch_refuses_upload(self):
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="created",
                submission_cab_signed_sha256="00" * 32)
        rel = FakeRelease(m)
        api = FakeApi([])
        with self.assertRaises(SystemExit):
            attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                             now=lambda: 0, sleep=lambda s: None)
        self.assertFalse(any(c[0] == "upload_blob" for c in api.calls))
    def test_microsoft_failure_marks_failed(self):
        import hashlib
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="committed",
                submission_cab_signed_sha256=hashlib.sha256(b"CABBYTES").hexdigest())
        failed_sub = {"commitStatus": "commitFailed",
                      "workflowStatus": {"currentStep": "driverValidation",
                                         "state": "failed",
                                         "messages": ["bad inf"]}}
        rel = FakeRelease(m); api = FakeApi([failed_sub])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "failed")
        self.assertEqual(rel.saved[-1]["ms_state"], "failed")
    def test_poll_timeout_keeps_state_committed(self):
        import hashlib, itertools
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="committed",
                submission_cab_signed_sha256=hashlib.sha256(b"CABBYTES").hexdigest())
        pending = {"commitStatus": "commitComplete",
                   "workflowStatus": {"currentStep": "sign", "state": "started"}}
        rel = FakeRelease(m)
        api = FakeApi([pending] * 200)
        clock = itertools.count(0, 60)  # each poll advances a minute
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: next(clock), sleep=lambda s: None,
                               deadline_s=300)
        self.assertEqual(out, "poll-timeout")
        self.assertEqual(m["ms_state"], "committed")
    def test_wait_signing_short_circuits(self):
        m = man(submission_cab_signed_sha256=None)
        out = attest.reconcile(FakeApi([]), FakeRelease(m), m, "driver-v1",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "wait-signing")
```

- [ ] **Step 4.2: Run, expect FAIL; Step 4.3: implement `Release`, `reconcile`, `main`**

```python
class Release:
    """Asset I/O for one driver-v* release via the gh CLI."""
    def __init__(self, repo, tag):
        self.repo = repo
        self.tag = tag

    def _gh(self, *args, **kw):
        return subprocess.run(["gh", *args], check=True,
                              capture_output=True, text=kw.pop("text", True))

    @staticmethod
    def resolve(repo, tag):
        """Return (tag, manifest) for `tag`, or the newest driver-v* release."""
        out = subprocess.run(
            ["gh", "api", f"repos/{repo}/releases?per_page=100"],
            check=True, capture_output=True, text=True).stdout
        rels = [r for r in json.loads(out)
                if r["tag_name"].startswith("driver-v")]
        if tag:
            rels = [r for r in rels if r["tag_name"] == tag]
            if not rels:
                sys.exit(f"::error::release {tag} not found")
        else:
            import re
            def build(r):
                mt = re.match(r"^driver-v\d+\.\d+\.\d+\.(\d+)$", r["tag_name"])
                return int(mt.group(1)) if mt else -1
            rels = sorted((r for r in rels if build(r) >= 0), key=build, reverse=True)
            if not rels:
                sys.exit("::error::no driver-v* releases found")
        rel = Release(repo, rels[0]["tag_name"])
        man = json.loads(rel.download_asset("manifest.json"))
        return rel.tag, rel, man

    def download_asset(self, name):
        subprocess.run(["gh", "release", "download", self.tag, "--repo", self.repo,
                        "--pattern", name, "--output", f"_dl_{name}", "--clobber"],
                       check=True)
        with open(f"_dl_{name}", "rb") as f:
            return f.read()

    def save_manifest(self, manifest):
        with open("manifest.json", "w") as f:
            json.dump(manifest, f, indent=2)
        self.upload_asset("manifest.json")

    def upload_asset(self, path):
        subprocess.run(["gh", "release", "upload", self.tag, path,
                        "--repo", self.repo, "--clobber"], check=True)


def reconcile(api, rel, m, tag, now=time.time, sleep=time.sleep, deadline_s=2700):
    import hashlib
    version = m.get("driver_version", tag.removeprefix("driver-v"))
    start = now()
    while True:
        act = next_action(m)
        if act in ("done", "wait-signing", "failed", "finalize"):
            return act
        if act == "create":
            product = api.create_product(build_product_payload(version))
            sub = api.create_submission(product["id"], f"attestation {version}")
            apply_created(m, str(product["id"]), str(sub["id"]))
            rel.save_manifest(m)          # persist BEFORE any upload
            m["_sub"] = sub               # cache SAS from creation (not persisted)
        elif act == "upload":
            cab = rel.download_asset(m["submission_cab"])
            got = hashlib.sha256(cab).hexdigest()
            want = m["submission_cab_signed_sha256"].lower()
            if got != want:
                sys.exit(f"::error::EV-signed CAB sha256 {got} != manifest {want}")
            sub = m.pop("_sub", None) or api.get_submission(
                m["ms_product_id"], m["ms_submission_id"])
            url = find_download(sub, "initialPackage")
            if not url:
                sys.exit("::error::submission has no initialPackage upload URL")
            api.upload_blob(url, cab)
            apply_state(m, "uploaded"); rel.save_manifest(m)
        elif act == "commit":
            api.commit(m["ms_product_id"], m["ms_submission_id"])
            apply_state(m, "committed"); rel.save_manifest(m)
        elif act == "poll":
            while True:
                sub = api.get_submission(m["ms_product_id"], m["ms_submission_id"])
                wf = sub.get("workflowStatus") or {}
                state = wf.get("state")
                if state == "failed" or sub.get("commitStatus") == "commitFailed":
                    print(f"::error::attestation failed at step "
                          f"{wf.get('currentStep')}: {wf.get('messages')}")
                    summary(json.dumps(sub, indent=2))
                    apply_state(m, "failed"); rel.save_manifest(m)
                    break
                if state == "completed" and find_download(sub, "signedPackage"):
                    data = api.download(find_download(sub, "signedPackage"))
                    zip_name = f"StreamToSpeaker-Driver-{version}-Microsoft.zip"
                    with open(zip_name, "wb") as f:
                        f.write(data)
                    rel.upload_asset(zip_name)
                    apply_done(m, zip_name); rel.save_manifest(m)
                    break
                if now() - start > deadline_s:
                    print(f"::error::poll deadline exceeded; state stays "
                          f"committed at step {wf.get('currentStep')} — re-run to resume")
                    return "poll-timeout"
                sleep(30)
        else:
            raise AssertionError(act)


def summary(text):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a") as f:
            f.write(text + "\n")


def main(argv=None):
    repo = os.environ["GITHUB_REPOSITORY"]
    tag_in = os.environ.get("INPUT_TAG") or None
    tag, rel, m = Release.resolve(repo, tag_in)
    print(f"reconciling {tag}: state={m.get('ms_state')} attested={m.get('attested')}")
    if next_action(m) in ("done", "wait-signing"):
        outcome = next_action(m)   # no API credentials needed
    else:
        token = get_token(os.environ["AZURE_TENANT_ID"],
                          os.environ["AZURE_CLIENT_ID"],
                          os.environ["AZURE_CLIENT_SECRET"])
        outcome = reconcile(HardwareApi(token), rel, m, tag)
    out_path = os.environ.get("GITHUB_OUTPUT")
    verify = outcome == "finalize"
    if out_path:
        with open(out_path, "a") as f:
            f.write(f"verify_tag={tag if verify else ''}\n")
            f.write(f"outcome={outcome}\n")
    summary(f"## Driver attest\n- release: `{tag}`\n- outcome: `{outcome}`\n"
            f"- product: `{m.get('ms_product_id')}` submission: "
            f"`{m.get('ms_submission_id')}`")
    if outcome in ("failed", "poll-timeout"):
        return 1
    if outcome == "wait-signing":
        print("EV-signed CAB not on the release yet — nothing submitted.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
```
Note: `reconcile`'s loop transitions `poll` → (`done`-via-`apply_done` → `finalize`) / (`failed`) on the next `next_action()` iteration; `"_sub"` is pop'd before any `save_manifest` after upload (`apply_state` mutates the same dict — ensure `_sub` is removed before serialization: `m.pop("_sub", None)` in the `upload` branch happens before `save_manifest`).

- [ ] **Step 4.4: Run full suite PASS**

Run: `cd .github/scripts && python3 -m unittest -v` — all tests green, including Task 1–3 suites.

- [ ] **Step 4.5: Commit** — `feat(attest): release I/O + reconcile orchestration`

### Task 5: `driver-attest.yml` workflow + reusable verification

**Files:**
- Create: `.github/workflows/driver-attest.yml`
- Modify: `.github/workflows/driver-attested.yml` (add `workflow_call`)

**Interfaces:**
- Consumes: `attest.py` env contract from Task 4; `driver-attested.yml`'s `inputs.tag`.
- Produces: workflow `Driver attest`; `driver-attested.yml` callable with `tag` (string, required for calls).

- [ ] **Step 5.1: Add `workflow_call` to `driver-attested.yml`**

In the `on:` block, alongside the existing `workflow_dispatch`:

```yaml
on:
  workflow_dispatch:
    inputs:
      tag:
        description: "driver-v* release tag carrying the Microsoft-signed zip (default: newest unattested one with a zip attached)"
        required: false
        default: ""
  workflow_call:
    inputs:
      tag:
        required: true
        type: string
```
`${{ inputs.tag }}` resolves identically in both trigger contexts — no other change needed.

- [ ] **Step 5.2: Create `driver-attest.yml`**

```yaml
name: Driver attest

# Reconciles the newest driver-v* release toward Microsoft attestation via
# the Partner Center Hardware API (spec: docs/superpowers/specs/
# 2026-08-05-driver-attestation-automation-design.md). Idempotent: re-running
# always resumes from the manifest's recorded state; workflow_dispatch is the
# universal retry. All logic lives in .github/scripts/attest.py.

run-name: "Driver attest · ${{ inputs.tag || 'newest driver release' }}"

on:
  workflow_run:
    workflows: ["Driver submission"]
    types: [completed]
  workflow_dispatch:
    inputs:
      tag:
        description: "driver-v* release tag (default: newest by build number)"
        required: false
        default: ""

permissions:
  contents: write

concurrency:
  group: driver-attest
  cancel-in-progress: false

jobs:
  reconcile:
    # On workflow_run, only act when Driver submission succeeded (its sign-cab
    # job completed → the EV-signed CAB is on the release). Manual dispatch
    # always runs.
    if: github.event_name == 'workflow_dispatch' || github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    timeout-minutes: 55
    outputs:
      verify_tag: ${{ steps.attest.outputs.verify_tag }}
    steps:
      - uses: actions/checkout@v4

      - name: Unit tests (guard)
        run: python3 -m unittest discover -s .github/scripts -v

      - name: Reconcile attestation state
        id: attest
        env:
          GH_TOKEN: ${{ github.token }}
          AZURE_TENANT_ID: ${{ secrets.AZURE_TENANT_ID }}
          AZURE_CLIENT_ID: ${{ secrets.AZURE_CLIENT_ID }}
          AZURE_CLIENT_SECRET: ${{ secrets.AZURE_CLIENT_SECRET }}
          INPUT_TAG: ${{ inputs.tag }}
        run: python3 .github/scripts/attest.py

  verify:
    needs: reconcile
    if: needs.reconcile.outputs.verify_tag != ''
    uses: ./.github/workflows/driver-attested.yml
    permissions:
      contents: write
    with:
      tag: ${{ needs.reconcile.outputs.verify_tag }}
```

- [ ] **Step 5.3: Validate with actionlint**

Run: `./actionlint -color sts-repo/.github/workflows/driver-attest.yml sts-repo/.github/workflows/driver-attested.yml`
Expected: clean.

- [ ] **Step 5.4: Commit** — `feat(ci): driver-attest reconciler workflow; make verification reusable`

### Task 6: Update runbook texts

**Files:**
- Modify: `.github/workflows/driver-submission.yml` (release-notes block, header comment)
- Modify: `docs/driver-signing.md`

- [ ] **Step 6.1: Collapse the manual runbook in `driver-submission.yml` release notes**

Replace steps 2–5 of the `$notes` array with:

```powershell
"1. Approve the EV-signing run for the submission CAB in the signing repo (its run name must show sha256 ``${{ steps.cab.outputs.sha256 }}``)."
"2. That's it — the **Driver attest** workflow submits the signed CAB to Microsoft via the Hardware API, polls, attaches the returned zip here, verifies signatures, and marks the driver attested. If it goes red, re-run it from the Actions tab (it resumes where it left off). Manual fallback: docs/driver-signing.md."
```
Also update the workflow's header comment (`# A human then submits...` paragraph) to describe the automatic flow.

- [ ] **Step 6.2: Rewrite `docs/driver-signing.md`**

Structure: (1) pipeline overview with the one human click; (2) what Driver attest does + how to retry (`workflow_dispatch`, optional `tag`); (3) one-time setup — the three `AZURE_*` repo secrets, what the Entra app is, Partner Center association, secret expiry (`invalid_client` = rotate in Entra → update repo secret); (4) manual fallback appendix (old browser flow + hand-attached zip + Driver attested dispatch); (5) failure modes table (`wait-signing`, `failed` + Microsoft's payload in the summary, `poll-timeout`).

- [ ] **Step 6.3: Commit** — `docs: driver signing runbook for automated attestation`

### Task 7: Live read-only integration check (from this box, not CI)

**Files:** none (verification only)

- [ ] **Step 7.1: Token + product GET through the new code path**

```bash
cd sts-repo/.github/scripts && python3 - <<'EOF'
import os, attest
tok = attest.get_token(os.environ["AZURE_TENANT_ID"], os.environ["AZURE_CLIENT_ID"],
                       os.environ["AZURE_CLIENT_SECRET"])
api = attest.HardwareApi(tok)
prods = api.request("GET", "/products")
print([p["productName"] for p in prods["value"]])
sub_link = [l for p in prods["value"] for l in p["links"] if l["rel"] == "get_submissions"]
print("OK:", len(prods["value"]), "products")
EOF
```
Expected: prints `['Stream to Speaker']` (or more) — proves the exact code CI will run authenticates and parses. NO POSTs.

- [ ] **Step 7.2: `Release.resolve` dry-run against the real repo**

Run `attest.py` pieces with `GITHUB_REPOSITORY=Mihonarium/StreamToSpeaker` — if no `driver-v*` release exists yet, expect the clean `::error::no driver-v* releases found` exit (that IS the pass condition pre-merge).

### Task 8: Secrets, push, PR update

- [ ] **Step 8.1: Set the three repo secrets**

```bash
gh secret set AZURE_TENANT_ID -R Mihonarium/StreamToSpeaker -b "4e42a1e3-2436-4a97-968f-612a22c608fc"
gh secret set AZURE_CLIENT_ID -R Mihonarium/StreamToSpeaker -b "28d798ca-3f21-4ee2-815e-19281d162b65"
gh secret set AZURE_CLIENT_SECRET -R Mihonarium/StreamToSpeaker -b "<the key from this session>"
gh secret list -R Mihonarium/StreamToSpeaker
```

- [ ] **Step 8.2: Push the branch**

```bash
git push origin pr25:claude/driver-signing-release-ci-u51pj7
```

- [ ] **Step 8.3: Update PR #25 body** — add an "Automated attestation" section describing the reconciler, the removed manual steps, and the new secrets; note the spec/plan docs.

- [ ] **Step 8.4: Request code review** (superpowers:requesting-code-review) on the full branch diff; fix findings; re-push.

### Task 9: Report

- [ ] **Step 9.1:** Summarize to the user: Phase 0 findings (fixed/deferred), what was built, test results, secrets set, what the first real driver push will exercise, and the reminder that the pasted client secret should be rotated once the first end-to-end run is green.
