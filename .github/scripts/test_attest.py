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


if __name__ == "__main__":
    unittest.main()
