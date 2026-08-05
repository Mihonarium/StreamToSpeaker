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


if __name__ == "__main__":
    unittest.main()
