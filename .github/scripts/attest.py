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
