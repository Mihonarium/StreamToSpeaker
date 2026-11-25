import hashlib
import io
import itertools
import json
import os
import tempfile
import unittest
import unittest.mock

import attest
import driver_release
import fake_gh


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
    # reconcile() writes the downloaded zip into cwd — keep test runs out
    # of the source tree.
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self._old_cwd = os.getcwd()
        os.chdir(self._tmp.name)

    def tearDown(self):
        os.chdir(self._old_cwd)
        self._tmp.cleanup()

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

    def test_commit_rejection_recovers_when_already_committed(self):
        # commit POST succeeded on a previous run that died before
        # save_manifest; the retry's POST is rejected but the submission is
        # already past commit — must proceed, not go permanently red.
        class RejectingApi(FakeApi):
            def commit(self, pid, sid):
                raise RuntimeError("POST /commit -> HTTP 400: already committed")
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="uploaded",
                submission_cab_signed_sha256=CAB_SHA)
        rel = FakeRelease(m)
        api = RejectingApi([
            {"commitStatus": "commitComplete",
             "workflowStatus": {"currentStep": "sign", "state": "started"}},
            DONE_SUB,
        ])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "finalize")
        self.assertIn("committed", [s["ms_state"] for s in rel.saved])

    def test_commit_rejection_reraises_when_not_committed(self):
        class RejectingApi(FakeApi):
            def commit(self, pid, sid):
                raise RuntimeError("POST /commit -> HTTP 500: server error")
        m = man(ms_product_id="111", ms_submission_id="222", ms_state="uploaded",
                submission_cab_signed_sha256=CAB_SHA)
        api = RejectingApi([
            {"commitStatus": "commitPayloadUploading",
             "workflowStatus": {"currentStep": "packageUpload", "state": "notStarted"}},
        ])
        with self.assertRaises(RuntimeError):
            attest.reconcile(api, FakeRelease(m), m, "driver-v1.0.0.42",
                             now=lambda: 0, sleep=lambda s: None)

    def test_create_response_without_url_falls_back_to_get(self):
        class NoUrlApi(FakeApi):
            def create_submission(self, pid, name):
                self.calls.append(("create_submission", pid))
                return {"id": 222}  # no downloads in the create response
        m = man(submission_cab_signed_sha256=CAB_SHA)
        rel = FakeRelease(m)
        api = NoUrlApi([
            {"downloads": {"items": [{"type": "initialPackage", "url": "https://sas/fresh"}]}},
            DONE_SUB,
        ])
        out = attest.reconcile(api, rel, m, "driver-v1.0.0.42",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "finalize")
        self.assertIn(("upload_blob", "https://sas/fresh", len(b"CABBYTES")), api.calls)

    def test_wait_signing_short_circuits(self):
        m = man(submission_cab_signed_sha256=None)
        out = attest.reconcile(FakeApi([]), FakeRelease(m), m, "driver-v1",
                               now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "wait-signing")


REPO = "owner/repo"
COMMIT = "c0ffee" + "0" * 34
CERTIFIED_211 = {"schema": 1, "driver_version": "1.1.1.211", "driver_build": 211, "commit": "e" * 40,
                 "source_hash": "5" * 64, "certified": True, "attested": False, "submission_cab": None,
                 "signed_zip": "StreamToSpeaker-Driver-1.1.1.211-Signed.zip", "signed_zip_sha256": "6" * 64,
                 "ms_product_id": "1", "ms_submission_id": "2", "ms_state": "done"}


def unsigned(version, commit=COMMIT):
    return man(driver_version=version, driver_build=int(version.rsplit(".", 1)[1]), commit=commit,
               submission_cab=f"StreamToSpeaker-Driver-{version}-Submission.cab",
               submission_cab_signed_sha256=None)


class InTemporaryDirectory(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self._old_cwd = os.getcwd()
        os.chdir(self._tmp.name)

    def tearDown(self):
        os.chdir(self._old_cwd)
        self._tmp.cleanup()


class ChoosingTheRelease(InTemporaryDirectory):
    """Which driver-v* release a run without a `tag` reconciles."""

    def resolve(self, fake, tag=None, **kw):
        with unittest.mock.patch("subprocess.run", fake_gh.subprocess_run(fake)):
            return attest.Release.resolve(REPO, tag, **kw)

    def test_without_a_tag_the_newest_unfinished_release_is_chosen_not_the_highest_build(self):
        # Build numbers restarted with the public history: a new driver
        # numbers below the certified 211. The run must not pick 211
        # (certified: "done") and end green without submitting anything.
        fake = fake_gh.FakeReleases(REPO)
        fake.add_release("driver-v1.1.1.211", {"manifest.json": CERTIFIED_211}, created_at="2026-10-12T00:00:00Z")
        fake.add_release("driver-v1.1.1.140", {"manifest.json": unsigned("1.1.1.140")},
                         created_at="2026-12-01T00:00:00Z")
        tag, _rel, m = self.resolve(fake)
        self.assertEqual(tag, "driver-v1.1.1.140")
        self.assertEqual(attest.next_action(m), "wait-signing")

    def test_a_named_tag_beyond_the_first_page_of_releases_is_found(self):
        fake = fake_gh.FakeReleases(REPO)
        fake.add_release("driver-v1.1.0.204", {"manifest.json": unsigned("1.1.0.204")},
                         created_at="2026-01-01T00:00:00Z")
        for i in range(120):
            fake.add_release(f"v0.{i}.0", prerelease=False, created_at=f"2026-06-01T00:{i // 60:02d}:{i % 60:02d}Z")
        tag, _rel, _m = self.resolve(fake, "driver-v1.1.0.204")
        self.assertEqual(tag, "driver-v1.1.0.204")


class ReconcilingARelease(InTemporaryDirectory):
    """reconcile() with the real Release against a fake of the release."""

    def fake(self):
        fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
        m = dict(unsigned("1.1.2.140"), submission_cab_signed_sha256=CAB_SHA)
        fake.add_release("driver-v1.1.2.140", {"manifest.json": m, m["submission_cab"]: b"CABBYTES"})
        return fake

    def test_every_transition_reaches_the_release(self):
        fake = self.fake()
        tag, rel, m = attest.Release.resolve(REPO, None, run=fake)
        out = attest.reconcile(FakeApi([DONE_SUB]), rel, m, tag, now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(out, "finalize")
        final = fake.manifest(tag)
        zip_name = "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip"
        self.assertEqual((final["ms_product_id"], final["ms_state"], final["ms_zip_asset"]), ("111", "done", zip_name))
        self.assertEqual(fake.files(tag)[zip_name], b"ZIPBYTES")

    def test_a_finished_release_is_never_written(self):
        fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
        fake.add_release("driver-v1.1.1.211", {"manifest.json": CERTIFIED_211})
        tag, rel, m = attest.Release.resolve(REPO, "driver-v1.1.1.211", run=fake)
        self.assertEqual(attest.reconcile(FakeApi([]), rel, m, tag, now=lambda: 0, sleep=lambda s: None), "done")
        with self.assertRaisesRegex(driver_release.ReleaseError, "never changed"):
            rel.save_manifest(attest.apply_created(dict(m), "1", "2"))
        path = fake_gh.write(self._tmp.name, "StreamToSpeaker-Driver-1.1.1.211-Microsoft.zip", b"other bytes")
        with self.assertRaisesRegex(driver_release.ReleaseError, "never changed"):
            rel.upload_asset(path)
        self.assertEqual(fake.writes(), [])

    def test_the_release_of_the_commit_that_finished_driver_submission_is_taken(self):
        fake = self.fake()
        fake.add_release("driver-v1.1.2.150", {"manifest.json": unsigned("1.1.2.150", commit="b" * 40)},
                         created_at="2099-01-01T00:00:00Z")
        self.assertEqual(attest.Release.resolve(REPO, None, run=fake)[0], "driver-v1.1.2.150")
        self.assertEqual(attest.Release.resolve(REPO, None, commit=COMMIT, run=fake)[0], "driver-v1.1.2.140")
        with self.assertRaises(SystemExit):
            attest.Release.resolve(REPO, None, commit="d" * 40, run=fake)


class TwoRunsOnOneRelease(InTemporaryDirectory):
    def test_the_reconciler_does_not_write_over_a_release_finished_meanwhile(self):
        # Driver attest polls; the zip is attached by hand and Driver
        # attested finishes the release (docs, "Manual fallback"). The
        # reconciler's next save must not take attested back.
        fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
        m = dict(unsigned("1.1.2.140"), submission_cab_signed_sha256=CAB_SHA, ms_product_id="7",
                 ms_submission_id="8", ms_state="committed")
        fake.add_release("driver-v1.1.2.140", {"manifest.json": m, m["submission_cab"]: b"CABBYTES"})
        tag, rel, man = attest.Release.resolve(REPO, "driver-v1.1.2.140", run=fake)
        finished = dict(m, attested=True, signed_zip="StreamToSpeaker-Driver-1.1.2.140-Signed.zip",
                        signed_zip_sha256="6" * 64)
        r = fake.release(tag)
        fake.add_asset(r, "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip", b"ms zip")
        fake.add_asset(r, "StreamToSpeaker-Driver-1.1.2.140-Signed.zip", b"zip")
        r["assets"] = [a for a in r["assets"] if a["name"] != "manifest.json"]
        fake.add_asset(r, "manifest.json", finished)
        attest.apply_done(man, "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip")
        with self.assertRaises(driver_release.ReleaseError):
            rel.save_manifest(man)
        self.assertIs(fake.manifest(tag)["attested"], True)

    def test_ids_recorded_before_a_stop_are_kept(self):
        # The run stops after manifest.json.pending (with the new ids) is up
        # and before the old manifest is deleted: the next run must resume
        # that submission, not create a second one.
        fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
        m = dict(unsigned("1.1.2.140"), submission_cab_signed_sha256=CAB_SHA)
        fake.add_release("driver-v1.1.2.140", {"manifest.json": m, m["submission_cab"]: b"CABBYTES"})
        tag, rel, man = attest.Release.resolve(REPO, "driver-v1.1.2.140", run=fake)
        self.assertEqual(attest.next_action(man), "create")
        attest.apply_created(man, "111", "222")
        fake.fail_when(lambda a: a[:3] == ("api", "--method", "DELETE"), fake_gh.Stop())
        with self.assertRaises(fake_gh.Stop):
            rel.save_manifest(man)
        tag, rel2, man2 = attest.Release.resolve(REPO, "driver-v1.1.2.140", run=fake)
        self.assertEqual(attest.next_action(man2), "upload")
        self.assertEqual(man2["ms_submission_id"], "222")


ROUND = "driver-v1.1.2.140"
ROUND_ZIP = "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip"
# What Partner Center answers at every step of these runs: the upload URL
# and the finished submission with its signed package.
ANY_STEP = {"commitStatus": "commitComplete",
            "workflowStatus": {"currentStep": "finalizeIngestion", "state": "completed"},
            "downloads": {"items": [{"type": "initialPackage", "url": "https://sas/up"},
                                    {"type": "signedPackage", "url": "https://sas/dl"}]}}


# fail_when predicates: the two calls that follow the upload of
# manifest.json.pending when the manifest is replaced.
def deleting(call):
    return call[:3] == ("api", "--method", "DELETE") and "/releases/assets/" in call[3]


def renaming(call):
    return call[:3] == ("api", "--method", "PATCH") and "/releases/assets/" in call[3]


def nth(predicate, n):
    """A fail_when predicate: the n-th call (from 0) that `predicate` matches."""
    seen = itertools.count()
    return lambda call: predicate(call) and next(seen) == n


def signed_round(**kw):
    """A fake with an attestation round whose CAB is EV-signed; `kw`: what
    its manifest records beyond that."""
    fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
    m = dict(unsigned("1.1.2.140"), submission_cab_signed_sha256=CAB_SHA, **kw)
    fake.add_release(ROUND, {"manifest.json": m, m["submission_cab"]: b"CABBYTES"})
    return fake


def reconcile_round(fake, api):
    """One Driver attest run on ROUND."""
    tag, rel, m = attest.Release.resolve(REPO, ROUND, run=fake)
    return attest.reconcile(api, rel, m, tag, now=lambda: 0, sleep=lambda s: None)


class AStoppedSaveIsFinishedByTheNextRun(InTemporaryDirectory):
    """Wherever a save of the manifest stops, the next Driver attest run
    finishes that write first and goes on from what it recorded."""

    def test_every_save_stopped_at_either_point(self):
        for save, state in enumerate(("created", "uploaded", "committed", "done")):
            for stopped_at in (deleting, renaming):
                with self.subTest(state=state, stopped_at=stopped_at.__name__):
                    fake = signed_round()
                    fake.fail_when(nth(stopped_at, save), fake_gh.Stop())
                    with self.assertRaises(fake_gh.Stop):
                        reconcile_round(fake, FakeApi([ANY_STEP] * 4))
                    self.assertEqual(json.loads(fake.files(ROUND)["manifest.json.pending"])["ms_state"], state)
                    again = FakeApi([ANY_STEP] * 4)
                    self.assertEqual(reconcile_round(fake, again), "finalize")
                    self.assertNotIn("create_product", [c[0] for c in again.calls])
                    final = fake.manifest(ROUND)
                    self.assertEqual((final["ms_product_id"], final["ms_submission_id"], final["ms_state"],
                                      final["ms_zip_asset"]), ("111", "222", "done", ROUND_ZIP))
                    self.assertNotIn("manifest.json.pending", fake.files(ROUND))

    def test_the_first_save_on_a_manifest_without_partner_center_fields(self):
        # Driver submission's manifest names no ms_* field at all; the
        # first save adds them at its end.
        for stopped_at in (deleting, renaming):
            with self.subTest(stopped_at=stopped_at.__name__):
                fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
                m = {k: v for k, v in unsigned("1.1.2.140").items() if not k.startswith("ms_")}
                m["submission_cab_signed_sha256"] = CAB_SHA
                fake.add_release(ROUND, {"manifest.json": m, m["submission_cab"]: b"CABBYTES"})
                fake.fail_when(stopped_at, fake_gh.Stop())
                with self.assertRaises(fake_gh.Stop):
                    reconcile_round(fake, FakeApi([ANY_STEP] * 4))
                again = FakeApi([ANY_STEP] * 4)
                self.assertEqual(reconcile_round(fake, again), "finalize")
                self.assertNotIn("create_product", [c[0] for c in again.calls])
                self.assertEqual(list(fake.manifest(ROUND))[len(m):],
                                 ["ms_product_id", "ms_submission_id", "ms_state", "ms_zip_asset"])

    def test_a_stopped_save_of_a_failure(self):
        failed = {"commitStatus": "commitFailed",
                  "workflowStatus": {"currentStep": "driverValidation", "state": "failed", "messages": ["bad inf"]}}
        for stopped_at in (deleting, renaming):
            with self.subTest(stopped_at=stopped_at.__name__):
                fake = signed_round(ms_product_id="111", ms_submission_id="222", ms_state="committed")
                fake.fail_when(stopped_at, fake_gh.Stop())
                with self.assertRaises(fake_gh.Stop), unittest.mock.patch("sys.stdout", io.StringIO()), \
                        unittest.mock.patch.object(attest, "summary", lambda text: None):
                    reconcile_round(fake, FakeApi([failed]))
                self.assertEqual(reconcile_round(fake, FakeApi([])), "failed")
                self.assertEqual(fake.manifest(ROUND)["ms_state"], "failed")
                self.assertNotIn("manifest.json.pending", fake.files(ROUND))


class APendingManifestThatIsNotTheReconcilersNextSave(InTemporaryDirectory):
    """Driver attest finishes a stopped save of its own. Another job's
    stopped write, or a file somebody placed, stops it, with nothing
    changed."""
    SIGNED = "StreamToSpeaker-Driver-1.1.2.140-Signed.zip"

    def assertLeft(self, fake, pending):
        """`pending`: the manifest waiting as manifest.json.pending, laid
        out as these workflows write one."""
        fake.add_asset(fake.release(ROUND), "manifest.json.pending", driver_release.as_written(pending))
        before, writes = fake.files(ROUND), len(fake.writes())
        with self.assertRaisesRegex(driver_release.ReleaseError, "manifest.json.pending") as refusal:
            attest.Release.resolve(REPO, ROUND, run=fake)
        self.assertIn("nothing was changed", str(refusal.exception))
        self.assertEqual(fake.files(ROUND), before)
        self.assertEqual(len(fake.writes()), writes)

    def returned(self):
        fake = signed_round(ms_product_id="111", ms_submission_id="222", ms_state="done", ms_zip_asset=ROUND_ZIP)
        rel = fake.release(ROUND)
        fake.add_asset(rel, ROUND_ZIP, b"ms zip")
        fake.add_asset(rel, self.SIGNED, b"canonical zip")
        return fake, dict(fake.manifest(ROUND), signed_zip=self.SIGNED,
                          signed_zip_sha256=hashlib.sha256(b"canonical zip").hexdigest())

    def test_the_manifest_that_marks_the_release_attested_is_left_for_the_job_that_checked_the_package(self):
        fake, with_zip = self.returned()
        self.assertLeft(fake, dict(with_zip, attested=True))

    def test_a_manifest_that_says_certified(self):
        fake, with_zip = self.returned()
        self.assertLeft(fake, dict(with_zip, certified=True))

    def test_sign_cabs_stopped_write(self):
        fake = fake_gh.FakeReleases(REPO, error=driver_release.ReleaseError)
        m = unsigned("1.1.2.140")
        fake.add_release(ROUND, {"manifest.json": m, m["submission_cab"]: b"CABBYTES"})
        self.assertLeft(fake, dict(m, submission_cab_signed_sha256=CAB_SHA))

    def test_a_save_that_is_more_than_the_next_step(self):
        # From "created" the next save says "uploaded" and nothing else.
        cases = {"two steps at once": dict(ms_state="committed"),
                 "another submission": dict(ms_state="uploaded", ms_submission_id="999"),
                 "another driver source": dict(ms_state="uploaded", source_hash="9" * 64)}
        for what, kw in cases.items():
            with self.subTest(what):
                fake = signed_round(ms_product_id="111", ms_submission_id="222", ms_state="created")
                self.assertLeft(fake, dict(fake.manifest(ROUND), **kw))

    def test_ids_that_are_not_ids(self):
        for ids in (dict(ms_product_id=111, ms_submission_id=222), dict(ms_product_id="111", ms_submission_id=""),
                    dict(ms_product_id="111", ms_submission_id="2; x")):
            with self.subTest(ids=ids):
                fake = signed_round()
                self.assertLeft(fake, dict(fake.manifest(ROUND), ms_state="created", **ids))

    def test_with_the_manifest_in_place_gone_a_pending_one_is_still_checked(self):
        cases = {"a state Driver attest never saves": dict(ms_state="created", attested=True),
                 "ids that are not ids": dict(ms_state="created", ms_submission_id="2; x"),
                 "a state without the ids it needs": dict(ms_state="uploaded", ms_submission_id=None),
                 "another name for the zip": dict(ms_state="done", ms_zip_asset="another.zip"),
                 "no state": dict(ms_state=None)}
        for what, kw in cases.items():
            with self.subTest(what):
                fake = signed_round()
                rel = fake.release(ROUND)
                pending = {**fake.manifest(ROUND), "ms_product_id": "111", "ms_submission_id": "222", **kw}
                rel["assets"] = [a for a in rel["assets"] if a["name"] != "manifest.json"]
                fake.add_asset(rel, "another.zip", b"ms zip")
                self.assertLeft(fake, pending)


class TheReconcilersOwnZip(InTemporaryDirectory):
    def test_a_stopped_upload_of_microsofts_zip_is_made_again(self):
        # An earlier attempt had attached a zip and stopped before it
        # recorded it; this one, replacing it with what Partner Center
        # gave it, stops too. The next run uploads the zip itself.
        fake = signed_round(ms_product_id="111", ms_submission_id="222", ms_state="committed")
        fake.add_asset(fake.release(ROUND), ROUND_ZIP, b"an earlier download")
        fake.fail_when(deleting, fake_gh.Stop())
        with self.assertRaises(fake_gh.Stop):
            reconcile_round(fake, FakeApi([ANY_STEP]))
        self.assertEqual(fake.files(ROUND)[ROUND_ZIP + ".pending"], b"ZIPBYTES")
        self.assertEqual(reconcile_round(fake, FakeApi([ANY_STEP])), "finalize")
        files = fake.files(ROUND)
        self.assertEqual(files[ROUND_ZIP], b"ZIPBYTES")
        self.assertNotIn(ROUND_ZIP + ".pending", files)
        self.assertEqual((fake.manifest(ROUND)["ms_state"], fake.manifest(ROUND)["ms_zip_asset"]), ("done", ROUND_ZIP))

    def test_a_pending_file_the_reconciler_does_not_upload_is_left(self):
        fake = signed_round()
        fake.add_asset(fake.release(ROUND), "notes.txt.pending", b"placed by hand")
        before = fake.files(ROUND)
        with self.assertRaisesRegex(driver_release.ReleaseError, "notes.txt.pending is not a file this job uploads"):
            attest.Release.resolve(REPO, ROUND, run=fake)
        self.assertEqual(fake.files(ROUND), before)


class SavingTheManifest(InTemporaryDirectory):
    def test_a_failed_manifest_upload_leaves_the_release_with_a_manifest(self):
        # gh's --clobber deletes the old asset before it uploads the new
        # one; a failed upload must not leave the release without the
        # manifest that holds the Partner Center ids.
        fake = fake_gh.FakeReleases(REPO)
        before = dict(unsigned("1.1.1.140"), submission_cab_signed_sha256=CAB_SHA)
        fake.add_release("driver-v1.1.1.140", {"manifest.json": before})
        fake.clobber_upload_fails.add("manifest.json")
        fake.fail_when(fake_gh.uploading("manifest.json.pending"))
        after = attest.apply_created(dict(before), "111", "222")
        with unittest.mock.patch("subprocess.run", fake_gh.subprocess_run(fake)):
            rel = attest.Release.resolve(REPO, "driver-v1.1.1.140")[1]
            with self.assertRaises(Exception):
                rel.save_manifest(after)
        self.assertIn(fake.manifest("driver-v1.1.1.140"), (before, after))


if __name__ == "__main__":
    unittest.main()
