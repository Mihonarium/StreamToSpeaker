#!/usr/bin/env python3
"""Reconcile the newest driver-v* release toward Microsoft attestation.

Driven by driver-attest.yml. Reads manifest.json from a driver-v* release,
advances whatever state it finds (create Partner Center product+submission,
upload the EV-signed CAB, commit, poll, fetch the signed zip), and records
every transition back onto the release before acting on it — so a re-run
always resumes cleanly and duplicate submissions are impossible.

Spec: docs/superpowers/specs/2026-08-05-driver-attestation-automation-design.md
Stdlib only; release asset I/O goes through the gh CLI.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
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


# Mirrors live product 14636198218617861 exactly (GET verified 2026-08-05);
# driverType is read-only/derived and not part of the create schema.
REQUESTED_SIGNATURES = [
    "WINDOWS_v100_X64_RS5_FULL", "WINDOWS_v100_X64_19H1_FULL",
    "WINDOWS_v100_X64_VB_FULL", "WINDOWS_v100_X64_CO_FULL",
    "WINDOWS_v100_X64_NI_FULL", "WINDOWS_v100_X64_GE_FULL",
    "WINDOWS_v100_X64_25H2_FULL", "WINDOWS_v100_X64_26H1_FULL",
]


def build_product_payload(version):
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


def find_download(sub, dtype):
    for item in ((sub.get("downloads") or {}).get("items") or []):
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
        # urlopen raises HTTPError on any non-2xx, so success needs no check.
        req = urllib.request.Request(sas_url, data=data, method="PUT", headers={
            "x-ms-blob-type": "BlockBlob",
            "Content-Type": "application/octet-stream"})
        with urllib.request.urlopen(req, timeout=600):
            pass

    def download(self, url):
        # SAS download URL — plain GET, no auth header.
        with urllib.request.urlopen(url, timeout=600) as r:
            return r.read()


class Release:
    """Asset I/O for one driver-v* release via the gh CLI."""

    def __init__(self, repo, tag):
        self.repo = repo
        self.tag = tag

    @staticmethod
    def resolve(repo, tag):
        """Return (tag, Release, manifest) for `tag`, or the newest driver-v*."""
        import re
        out = subprocess.run(
            ["gh", "api", f"repos/{repo}/releases?per_page=100"],
            check=True, capture_output=True, text=True).stdout
        rels = [r for r in json.loads(out) if r["tag_name"].startswith("driver-v")]
        if tag:
            rels = [r for r in rels if r["tag_name"] == tag]
            if not rels:
                sys.exit(f"::error::release {tag} not found")
        else:
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
        dest = f"_dl_{name}"
        subprocess.run(["gh", "release", "download", self.tag, "--repo", self.repo,
                        "--pattern", name, "--output", dest, "--clobber"], check=True)
        with open(dest, "rb") as f:
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
    version = m.get("driver_version") or tag.removeprefix("driver-v")
    start = now()
    cached_sub = None  # create-time submission JSON, holds a fresh SAS URL
    while True:
        act = next_action(m)
        if act in ("done", "wait-signing", "failed", "finalize"):
            return act
        if act == "create":
            product = api.create_product(build_product_payload(version))
            sub = api.create_submission(product["id"], f"attestation {version}")
            cached_sub = sub
            apply_created(m, str(product["id"]), str(sub["id"]))
            rel.save_manifest(m)  # persist IDs BEFORE any upload
        elif act == "upload":
            cab = rel.download_asset(m["submission_cab"])
            got = hashlib.sha256(cab).hexdigest()
            want = m["submission_cab_signed_sha256"].lower()
            if got != want:
                sys.exit(f"::error::EV-signed CAB sha256 {got} != manifest {want} - refusing to upload")
            sub = cached_sub or api.get_submission(m["ms_product_id"], m["ms_submission_id"])
            cached_sub = None
            url = find_download(sub, "initialPackage")
            if not url:
                # The create response can omit download links; a fresh GET
                # always carries them (verified live 2026-08-05).
                sub = api.get_submission(m["ms_product_id"], m["ms_submission_id"])
                url = find_download(sub, "initialPackage")
            if not url:
                sys.exit("::error::submission has no initialPackage upload URL")
            api.upload_blob(url, cab)
            apply_state(m, "uploaded")
            rel.save_manifest(m)
        elif act == "commit":
            # If a previous run committed but died before save_manifest, a
            # re-POST of commit is rejected by the API. Treat "already past
            # commit" as success so the universal-retry invariant holds.
            try:
                api.commit(m["ms_product_id"], m["ms_submission_id"])
            except RuntimeError as e:
                sub = api.get_submission(m["ms_product_id"], m["ms_submission_id"])
                wf_state = (sub.get("workflowStatus") or {}).get("state")
                if (sub.get("commitStatus") in ("commitPending", "commitComplete")
                        or wf_state in ("started", "completed")):
                    print(f"commit POST rejected but submission is already "
                          f"committed ({e}); continuing")
                else:
                    raise
            apply_state(m, "committed")
            rel.save_manifest(m)
        elif act == "poll":
            while True:
                sub = api.get_submission(m["ms_product_id"], m["ms_submission_id"])
                wf = sub.get("workflowStatus") or {}
                state = wf.get("state")
                if state == "failed" or sub.get("commitStatus") == "commitFailed":
                    print(f"::error::attestation failed at step "
                          f"{wf.get('currentStep')}: {wf.get('messages')}")
                    # downloads.items carries live SAS URLs (the
                    # initialPackage one is write-capable) — never put those
                    # in the summary; diagnostics live in workflowStatus.
                    redacted = {k: v for k, v in sub.items() if k != "downloads"}
                    summary("### Microsoft submission failure\n```json\n"
                            + json.dumps(redacted, indent=2) + "\n```")
                    apply_state(m, "failed")
                    rel.save_manifest(m)
                    break
                signed_url = find_download(sub, "signedPackage")
                if state == "completed" and signed_url:
                    zip_name = f"StreamToSpeaker-Driver-{version}-Microsoft.zip"
                    with open(zip_name, "wb") as f:
                        f.write(api.download(signed_url))
                    rel.upload_asset(zip_name)
                    apply_done(m, zip_name)
                    rel.save_manifest(m)
                    break
                if now() - start > deadline_s:
                    print(f"::error::poll deadline exceeded at step "
                          f"{wf.get('currentStep')} - state stays committed; re-run to resume")
                    return "poll-timeout"
                sleep(30)
        else:
            raise AssertionError(act)


def summary(text):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a") as f:
            f.write(text + "\n")


def main():
    repo = os.environ["GITHUB_REPOSITORY"]
    tag_in = os.environ.get("INPUT_TAG") or None
    tag, rel, m = Release.resolve(repo, tag_in)
    print(f"reconciling {tag}: ms_state={m.get('ms_state')} attested={m.get('attested')}")
    if next_action(m) in ("done", "wait-signing", "finalize", "failed"):
        outcome = next_action(m)  # terminal states need no API credentials
    else:
        token = get_token(os.environ["AZURE_TENANT_ID"],
                          os.environ["AZURE_CLIENT_ID"],
                          os.environ["AZURE_CLIENT_SECRET"])
        outcome = reconcile(HardwareApi(token), rel, m, tag)
    verify = outcome == "finalize"
    out_path = os.environ.get("GITHUB_OUTPUT")
    if out_path:
        with open(out_path, "a") as f:
            f.write(f"verify_tag={tag if verify else ''}\n")
            f.write(f"outcome={outcome}\n")
    summary(f"## Driver attest\n- release: `{tag}`\n- outcome: `{outcome}`\n"
            f"- product: `{m.get('ms_product_id')}` submission: `{m.get('ms_submission_id')}`")
    if outcome in ("failed", "poll-timeout"):
        return 1
    if outcome == "wait-signing":
        print("EV-signed CAB not on the release yet - nothing submitted.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
