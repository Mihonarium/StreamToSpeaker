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
