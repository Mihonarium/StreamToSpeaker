import hashlib
import itertools
import json
import unittest

import attest


def man(**kw):
    base = {"schema": 1, "driver_version": "1.0.0.42", "attested": False,
            "submission_cab": "StreamToSpeaker-Driver-1.0.0.42-Submission.cab",
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
        m = man()
        del m["ms_product_id"]; del m["ms_submission_id"]; del m["ms_state"]
        self.assertEqual(attest.next_action(m), "create")


class Payload(unittest.TestCase):
    def test_product_payload(self):
        p = attest.build_product_payload("1.0.0.42")
        self.assertEqual(p["productName"], "StreamToSpeaker-Driver-1.0.0.42")
        self.assertEqual(p["testHarness"], "attestation")
        self.assertEqual(p["deviceType"], "internalExternal")
        self.assertFalse(p["isTestSign"])
        self.assertFalse(p["isFlightSign"])
        self.assertIn("WINDOWS_v100_X64_RS5_FULL", p["requestedSignatures"])
        self.assertIn("WINDOWS_v100_X64_26H1_FULL", p["requestedSignatures"])
        self.assertEqual(len(p["requestedSignatures"]), 8)


class Mutations(unittest.TestCase):
    def test_apply_created_sets_ids_and_state(self):
        m = attest.apply_created(man(), "111", "222")
        self.assertEqual((m["ms_product_id"], m["ms_submission_id"], m["ms_state"]),
                         ("111", "222", "created"))

    def test_apply_state(self):
        m = attest.apply_state(man(ms_state="created"), "uploaded")
        self.assertEqual(m["ms_state"], "uploaded")

    def test_apply_done_records_zip(self):
        m = attest.apply_done(man(ms_state="committed"), "X-Microsoft.zip")
        self.assertEqual(m["ms_state"], "done")
        self.assertEqual(m["ms_zip_asset"], "X-Microsoft.zip")


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

    def test_null_downloads(self):
        self.assertIsNone(attest.find_download({"downloads": None}, "signedPackage"))


class FakeApi:
    def __init__(self, poll_results):
        self.calls = []
        self.poll_results = list(poll_results)

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
        self.calls.append(("commit", pid, sid))
        return {}

    def upload_blob(self, url, data):
        self.calls.append(("upload_blob", url, len(data)))

    def download(self, url):
        self.calls.append(("download", url))
        return b"ZIPBYTES"


class FakeRelease:
    def __init__(self, manifest, cab=b"CABBYTES"):
        self.manifest = manifest
        self.cab = cab
        self.saved = []
        self.uploaded = []

    def save_manifest(self, m):
        self.saved.append(json.loads(json.dumps(m)))

    def download_asset(self, name):
        return self.cab

    def upload_asset(self, path):
        self.uploaded.append(path)


CAB_SHA = hashlib.sha256(b"CABBYTES").hexdigest()

DONE_SUB = {"commitStatus": "commitComplete",
            "workflowStatus": {"currentStep": "finalizeIngestion", "state": "completed"},
            "downloads": {"items": [{"type": "signedPackage", "url": "https://sas/dl"}]}}


class Reconcile(unittest.TestCase):
    def test_full_run_from_scratch(self):
        m = man(submission_cab_signed_sha256=CAB_SHA)
        rel = FakeRelease(m)
        api = FakeApi([DONE_SUB])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "finalize")
        # IDs persisted BEFORE upload:
        self.assertEqual(rel.saved[0]["ms_state"], "created")
        self.assertTrue(any(c[0] == "upload_blob" for c in api.calls))
        self.assertEqual(rel.saved[-1]["ms_state"], "done")
        # the cached create-time SAS URL must never leak into the manifest
        for saved in rel.saved:
            self.assertNotIn("_sub", saved)
        self.assertIn("StreamToSpeaker-Driver-1.0.0.42-Microsoft.zip", rel.uploaded)

    def test_sha_mismatch_refuses_upload(self):
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="created",
                submission_cab_signed_sha256="00" * 32)
        rel = FakeRelease(m)
        api = FakeApi([])
        with self.assertRaises(SystemExit):
            attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                             now=lambda: 0, sleep=lambda s: None)
        self.assertFalse(any(c[0] == "upload_blob" for c in api.calls))

    def test_resume_from_created_fetches_fresh_sas(self):
        # a re-run after a crash has no cached _sub; must GET the submission
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="created",
                submission_cab_signed_sha256=CAB_SHA)
        rel = FakeRelease(m)
        api = FakeApi([
            {"downloads": {"items": [{"type": "initialPackage", "url": "https://sas/fresh"}]}},
            DONE_SUB,
        ])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "finalize")
        self.assertIn(("upload_blob", "https://sas/fresh", len(b"CABBYTES")), api.calls)

    def test_microsoft_failure_marks_failed(self):
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="committed",
                submission_cab_signed_sha256=CAB_SHA)
        failed_sub = {"commitStatus": "commitFailed",
                      "workflowStatus": {"currentStep": "driverValidation",
                                         "state": "failed",
                                         "messages": ["bad inf"]}}
        rel = FakeRelease(m)
        api = FakeApi([failed_sub])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "failed")
        self.assertEqual(rel.saved[-1]["ms_state"], "failed")

    def test_poll_timeout_keeps_state_committed(self):
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="committed",
                submission_cab_signed_sha256=CAB_SHA)
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


if __name__ == "__main__":
    unittest.main()
