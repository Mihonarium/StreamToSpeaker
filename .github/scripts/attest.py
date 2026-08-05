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
        req = urllib.request.Request(sas_url, data=data, method="PUT", headers={
            "x-ms-blob-type": "BlockBlob",
            "Content-Type": "application/octet-stream"})
        with urllib.request.urlopen(req, timeout=600) as r:
            if r.status not in (200, 201):
                raise RuntimeError(f"blob upload HTTP {r.status}")

    def download(self, url):
        # SAS download URL — plain GET, no auth header.
        with urllib.request.urlopen(url, timeout=600) as r:
            return r.read()
