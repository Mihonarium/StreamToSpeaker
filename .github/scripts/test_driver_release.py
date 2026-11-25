"""Tests of driver_release.py and release_assets.py against fake_gh.

Each rule of driver_release.py is a class here, tested for every command
that writes a driver release."""
import contextlib
import io
import json
import os
import shutil
import tempfile
import threading
import unittest
import unittest.mock

import driver_release as dr
import fake_gh
import release_assets as ra
from fake_gh import FakeReleases, sha
from test_release_draft import bundle_for

REPO = "owner/repo"
SIGNED_ZIP = "StreamToSpeaker-Driver-1.1.2.140-Signed.zip"
COMMIT = "c0ffee" + "0" * 34
OTHER = "badc0de" + "0" * 33
MAIN = "refs/heads/main"
HASH = "5" * 64
CAB_SHA = "ca" * 32
TAG = "driver-v1.1.2.140"
HEAD_COMMIT = "d" * 40
CAB = "StreamToSpeaker-Driver-1.1.2.140-Submission.cab"


# The provenance bundle of the canonical zip record() publishes.
BUNDLE = bundle_for(b"canonical zip", SIGNED_ZIP)


def Fake():
    """The fake gh; a failing command raises what gh() raises."""
    return FakeReleases(REPO, error=ra.ReleaseError)


def submission_manifest(version="1.1.2.140", commit=COMMIT, **kw):
    m = {"schema": 1, "driver_version": version, "driver_prefix": version.rsplit(".", 1)[0],
         "driver_build": int(version.rsplit(".", 1)[1]), "commit": commit, "source_hash": HASH,
         "build_root": "D:\\a\\StreamToSpeaker\\StreamToSpeaker", "driver_h_stamped": True,
         "submission_cab": f"StreamToSpeaker-Driver-{version}-Submission.cab", "submission_cab_sha256": CAB_SHA,
         "submission_cab_signed_sha256": None, "attested": False, "signed_zip": None, "signed_zip_sha256": None}
    m.update(kw)
    return m


def certified_manifest(version="1.1.1.211", **kw):
    m = {"schema": 1, "driver_version": version, "driver_build": int(version.rsplit(".", 1)[1]),
         "commit": "e" * 40, "source_hash": HASH, "submission_cab": None, "submission_cab_sha256": None,
         "submission_cab_signed_sha256": None, "attested": False, "certified": True,
         "signed_zip": f"StreamToSpeaker-Driver-{version}-Signed.zip", "signed_zip_sha256": sha(b"signed zip"),
         "ms_zip_asset": f"StreamToSpeaker-Driver-{version}-Microsoft.zip", "ms_zip_sha256": sha(b"ms zip"),
         "lab_zip": f"StreamToSpeaker-Driver-{version}-lab-testsigned.zip", "lab_zip_sha256": sha(b"lab zip"),
         "ms_product_id": "1", "ms_submission_id": "2", "ms_state": "done"}
    m.update(kw)
    return m


def certified_release(fake, version="1.1.1.211", created_at="2026-10-12T00:00:00Z"):
    m = certified_manifest(version)
    return fake.add_release(f"driver-v{version}", {
        "manifest.json": m, m["signed_zip"]: b"signed zip", m["ms_zip_asset"]: b"ms zip",
        m["lab_zip"]: b"lab zip"}, created_at=created_at)


def attested_manifest(version="1.1.0.204", **kw):
    signed = f"StreamToSpeaker-Driver-{version}-Signed.zip"
    return submission_manifest(version, submission_cab_signed_sha256=sha(b"signed cab"), attested=True,
                               signed_zip=signed, signed_zip_sha256=sha(b"attested zip"),
                               ms_product_id="7", ms_submission_id="8", ms_state="done",
                               ms_zip_asset=f"StreamToSpeaker-Driver-{version}-Microsoft.zip", **kw)


def attested_release(fake, version="1.1.0.204", created_at="2026-08-01T00:00:00Z"):
    m = attested_manifest(version)
    return fake.add_release(f"driver-v{version}", {
        "manifest.json": m, m["submission_cab"]: b"signed cab", m["signed_zip"]: b"attested zip",
        m["ms_zip_asset"]: b"ms zip"}, created_at=created_at)


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.old_cwd = os.getcwd()
        os.chdir(self.tmp)
        self.summary = os.path.join(self.tmp, "summary.md")
        self.env = unittest.mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": self.summary,
                                                        "GITHUB_OUTPUT": os.path.join(self.tmp, "out")})
        self.env.start()

    def tearDown(self):
        self.env.stop()
        os.chdir(self.old_cwd)
        shutil.rmtree(self.tmp)

    def run_cmd(self, fake, *argv, ref=MAIN):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = dr.main([argv[0], "--repo", REPO, "--ref", ref, *argv[1:]], run=fake, sleep=lambda s: None)
        self.output = out.getvalue()
        return code

    def outputs(self):
        path = os.environ["GITHUB_OUTPUT"]
        if not os.path.exists(path):
            return {}
        with open(path) as fh:
            return dict(line.rstrip("\n").split("=", 1) for line in fh if "=" in line)

    def file(self, name, data):
        return fake_gh.write(self.tmp, name, data)

    # The commands, with the arguments their workflow steps give them.
    def release(self, fake, tag=TAG, commit=COMMIT, cab_sha=CAB_SHA, ref=MAIN):
        version = tag.removeprefix("driver-v")
        return self.run_cmd(fake, "submission-release", "--tag", tag, "--commit", commit, "--version", version,
                            "--prefix", version.rsplit(".", 1)[0], "--build", version.rsplit(".", 1)[1],
                            "--source-hash", HASH, "--build-root", "D:\\a\\StreamToSpeaker\\StreamToSpeaker",
                            "--cab-name", f"StreamToSpeaker-Driver-{version}-Submission.cab",
                            "--cab-sha256", cab_sha, ref=ref)

    def check(self, fake, tag=TAG, commit=COMMIT, ref=MAIN):
        return self.run_cmd(fake, "submission-check", "--tag", tag, "--commit", commit, ref=ref)

    def signed(self, fake, data=b"signed cab", tag=TAG, cab_sha=CAB_SHA, ref=MAIN):
        name = f"StreamToSpeaker-Driver-{tag.removeprefix('driver-v')}-Submission.cab"
        path = fake_gh.write(os.path.join(self.tmp, "signed"), name, data)
        return self.run_cmd(fake, "submission-signed", "--tag", tag, "--commit", COMMIT, "--cab-sha256", cab_sha,
                            "--signed", path, "--signed-sha256", sha(data), ref=ref)

    def cleanup(self, fake, tag=TAG, ref=MAIN):
        # The run's own commit: older builds of commits before it count as
        # builds of main.
        for r in fake.releases:
            fake.compares.setdefault((r["target_commitish"], HEAD_COMMIT), "ahead")
        return self.run_cmd(fake, "submission-cleanup", "--tag", tag, "--commit", HEAD_COMMIT, ref=ref)

    def record(self, fake, tag="driver-v1.1.2.140", data=b"canonical zip", bundle=None):
        version = tag.removeprefix("driver-v")
        path = fake_gh.write(os.path.join(self.tmp, "pkg"), f"StreamToSpeaker-Driver-{version}-Signed.zip", data)
        argv = ["attested-record", "--tag", tag, "--signed-zip", path,
                "--ms-zip-name", f"StreamToSpeaker-Driver-{version}-Microsoft.zip"]
        if bundle is not None:
            argv += ["--bundle", self.file("bundle.json", bundle)]
        return self.run_cmd(fake, *argv)


def returned_release(fake, version="1.1.2.140", **kw):
    """An attestation round Microsoft has returned: the EV-signed CAB, the
    Partner Center ids and the Microsoft zip on the release."""
    m = submission_manifest(version, submission_cab_signed_sha256=sha(b"signed cab"), ms_product_id="7",
                            ms_submission_id="8", ms_state="done",
                            ms_zip_asset=f"StreamToSpeaker-Driver-{version}-Microsoft.zip", **kw)
    return fake.add_release(f"driver-v{version}", {"manifest.json": m, m["submission_cab"]: b"signed cab",
                                                    m["ms_zip_asset"]: b"ms zip"})


class AttestedRecordPublishesWhatWasAttested(Base):
    def test_a_zip_its_provenance_does_not_name_is_refused(self):
        fake = Fake()
        returned_release(fake)
        before = fake.files(TAG)
        self.assertEqual(self.record(fake, bundle=bundle_for(b"another zip", SIGNED_ZIP)), 1)
        self.assertIn("provenance", self.output)
        self.assertEqual(fake.files(TAG), before)


class AFinishedReleaseIsNeverChanged(Base):
    """Rule 1, for every command that writes a driver release."""

    def assertUntouched(self, fake, before, code):
        self.assertEqual(code, 1, self.output)
        self.assertEqual({r["tag_name"]: fake.files(r["tag_name"]) for r in fake.releases}, before)
        self.assertEqual(fake.writes(), [])

    def snapshot(self, fake):
        return {r["tag_name"]: fake.files(r["tag_name"]) for r in fake.releases}

    def test_a_driver_build_at_the_certified_releases_tag_is_refused(self):
        # The certified 1.1.1.211, built from commit eac87c1 with
        # DriverBuildNumber=211: a submission at commit count 211, a
        # re-run, or driver_build=211 computes the same tag.
        for commit in (COMMIT, "e" * 40):
            fake = Fake()
            certified_release(fake)
            before = self.snapshot(fake)
            self.assertUntouched(fake, before, self.check(fake, "driver-v1.1.1.211", commit))
            self.assertUntouched(fake, before, self.release(fake, "driver-v1.1.1.211", commit))
            self.assertIn("certified driver release; a finished driver release is never changed", self.output)

    def test_a_driver_build_at_an_attested_releases_tag_is_refused(self):
        fake = Fake()
        attested_release(fake)
        before = self.snapshot(fake)
        self.assertUntouched(fake, before, self.release(fake, "driver-v1.1.0.204"))

    def test_sign_cab_refuses_a_finished_release(self):
        for add in (certified_release, attested_release):
            fake = Fake()
            rel = add(fake)
            before = self.snapshot(fake)
            self.assertUntouched(fake, before, self.signed(fake, tag=rel["tag_name"]))

    def test_cleanup_never_deletes_a_finished_release(self):
        fake = Fake()
        certified_release(fake, "1.1.1.139")
        attested_release(fake, "1.1.1.138")
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        self.assertEqual(self.cleanup(fake), 0, self.output)
        self.assertEqual(sorted(r["tag_name"] for r in fake.releases),
                         ["driver-v1.1.1.138", "driver-v1.1.1.139", TAG])
        self.assertEqual(fake.writes(), [])

    def test_attested_record_refuses_a_finished_release(self):
        for add in (certified_release, attested_release):
            fake = Fake()
            rel = add(fake)
            before = self.snapshot(fake)
            self.assertUntouched(fake, before, self.record(fake, rel["tag_name"]))

    def test_writing_the_bytes_a_finished_release_holds_is_no_change(self):
        fake = Fake()
        certified_release(fake)
        rel = dr.DriverRelease(REPO, "driver-v1.1.1.211", fake, sleep=lambda s: None)
        self.assertEqual(rel.put(b"signed zip", "StreamToSpeaker-Driver-1.1.1.211-Signed.zip"), "unchanged")
        self.assertEqual(rel.write_manifest(certified_manifest()), "unchanged")
        # A new file first: no other rule stands in front of this one.
        with self.assertRaisesRegex(ra.ReleaseError, "never changed"):
            rel.put(b"a new file", "notes.txt")
        with self.assertRaisesRegex(ra.ReleaseError, "never changed"):
            rel.put(b"other bytes", "StreamToSpeaker-Driver-1.1.1.211-Signed.zip")
        with self.assertRaisesRegex(ra.ReleaseError, "never changed"):
            rel.write_manifest(certified_manifest(ms_state="other"))
        with self.assertRaisesRegex(ra.ReleaseError, "never changed"):
            rel.edit("t", "n")
        with self.assertRaisesRegex(ra.ReleaseError, "never changed"):
            rel.delete()
        self.assertEqual(fake.writes(), [])


class EachWorkflowWritesOnlyWhatIsItsToAdvance(Base):
    """Rule 2."""

    def test_a_branch_dispatch_writes_no_release(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        for code in (self.check(fake, ref="refs/heads/feature"), self.release(fake, ref="refs/heads/feature"),
                     self.signed(fake, ref="refs/heads/feature"), self.cleanup(fake, "driver-v1.1.2.150",
                                                                                ref="refs/heads/feature")):
            self.assertEqual(code, 1)
            self.assertIn("written only by runs on main", self.output)
        self.assertEqual(fake.writes(), [])

    def test_a_new_driver_build_creates_its_release_with_the_manifest_only(self):
        fake = Fake()
        certified_release(fake)
        self.assertEqual(self.check(fake, "driver-v1.1.2.140"), 0, self.output)
        self.assertEqual(self.release(fake, "driver-v1.1.2.140"), 0, self.output)
        rel = fake.release(TAG)
        self.assertTrue(rel["prerelease"])
        self.assertEqual(rel["target_commitish"], COMMIT)
        self.assertEqual(list(fake.files(TAG)), ["manifest.json"])
        self.assertEqual(fake.manifest(TAG), submission_manifest())
        self.assertIn(f"sha256 `{CAB_SHA}`", rel["body"])

    def test_rebuilding_a_submission_nobody_has_signed_yet_replaces_its_manifest(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        self.assertEqual(self.release(fake, cab_sha="cb" * 32), 0, self.output)
        self.assertEqual(fake.manifest(TAG)["submission_cab_sha256"], "cb" * 32)

    def test_the_empty_release_an_earlier_attempt_created_is_taken_over(self):
        fake = Fake()
        fake.add_release(TAG, target=COMMIT)
        self.assertEqual(self.release(fake), 0, self.output)
        self.assertEqual(fake.manifest(TAG), submission_manifest())

    def test_a_driver_build_refuses_a_release_that_is_not_its_own(self):
        cases = {
            "another commit": ({"manifest.json": submission_manifest(commit=OTHER)}, COMMIT, "commit"),
            "EV-signed": ({"manifest.json": submission_manifest(submission_cab_signed_sha256=sha(b"s")),
                           CAB: b"s"}, COMMIT, "EV-signed"),
            "submitted": ({"manifest.json": submission_manifest(submission_cab_signed_sha256=sha(b"s"),
                                                                ms_submission_id="9", ms_state="created"),
                           CAB: b"s"}, COMMIT, "EV-signed"),
            "no manifest": ({"notes.txt": b"x"}, COMMIT, "no manifest.json"),
            "empty, other commit": ({}, COMMIT, "no manifest.json"),
            "hand-made": ({"manifest.json": {"schema": 1}}, COMMIT, "not a Driver submission"),
        }
        for what, (assets, commit, says) in cases.items():
            with self.subTest(what):
                fake = Fake()
                fake.add_release(TAG, assets, target=OTHER if what == "empty, other commit" else COMMIT)
                before = fake.files(TAG)
                self.assertEqual(self.check(fake, commit=commit), 1)
                self.assertIn(says, self.output)
                self.assertEqual(self.release(fake, commit=commit), 1)
                self.assertEqual(fake.files(TAG), before)
                self.assertEqual(fake.writes(), [])

    def test_sign_cab_adds_the_signed_cab_and_records_it(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        self.assertEqual(self.signed(fake), 0, self.output)
        files = fake.files(TAG)
        self.assertEqual(files[CAB], b"signed cab")
        self.assertEqual(files[CAB + ".sha256"], f"{sha(b'signed cab')}  {CAB}".encode())
        self.assertEqual(fake.manifest(TAG)["submission_cab_signed_sha256"], sha(b"signed cab"))
        self.assertIn("Run **Driver attest**", open(self.summary).read())

    def test_re_running_sign_cab_alone_works_and_after_success_changes_nothing(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        # An earlier attempt got the signed CAB up and stopped before the
        # manifest: a new signature (other bytes) replaces it.
        fake.add_asset(fake.release(TAG), CAB, b"an earlier signature")
        self.assertEqual(self.signed(fake), 0, self.output)
        self.assertEqual(fake.files(TAG)[CAB], b"signed cab")
        writes = len(fake.writes())
        self.assertEqual(self.signed(fake), 0, self.output)
        self.assertEqual(len(fake.writes()), writes)
        # Another signature after the release recorded one is refused.
        self.assertEqual(self.signed(fake, data=b"yet another signature"), 1)
        self.assertIn("already records another EV-signed CAB", self.output)

    def test_sign_cab_refuses_a_cab_a_later_build_replaced(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest(submission_cab_sha256="cb" * 32)})
        self.assertEqual(self.signed(fake), 1)
        self.assertIn("a later build of this commit replaced it", self.output)
        self.assertEqual(fake.writes(), [])

    def test_cleanup_deletes_only_older_builds_nobody_signed_or_submitted(self):
        fake = Fake()
        fake.add_release("driver-v1.1.2.130", {"manifest.json": submission_manifest("1.1.2.130")}, target=COMMIT)
        fake.add_release("driver-v1.1.2.131", {"manifest.json": submission_manifest(
            "1.1.2.131", submission_cab_signed_sha256=sha(b"s"))})
        fake.add_release("driver-v1.1.2.132", {"manifest.json": submission_manifest(
            "1.1.2.132", submission_cab_signed_sha256=sha(b"s"), ms_submission_id="5", ms_state="failed")})
        fake.add_release("driver-v1.1.2.133", {"notes.txt": b"made by hand"})
        fake.add_release("driver-v1.1.2.134", {"manifest.json": {"schema": 1, "driver_version": "1.1.2.134"}})
        fake.add_release("driver-v1.1.2.150", {"manifest.json": submission_manifest("1.1.2.150")})
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        self.assertEqual(self.cleanup(fake), 0, self.output)
        self.assertEqual(sorted(r["tag_name"] for r in fake.releases),
                         ["driver-v1.1.2.131", "driver-v1.1.2.132", "driver-v1.1.2.133", "driver-v1.1.2.134",
                          TAG, "driver-v1.1.2.150"])

    def test_cleanup_keeps_a_release_whose_manifest_cannot_be_read(self):
        fake = Fake()
        fake.add_release("driver-v1.1.2.130", {"manifest.json": submission_manifest("1.1.2.130")})
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        fake.fail_when(lambda a: a[:3] == ("release", "download", "driver-v1.1.2.130"))
        self.assertEqual(self.cleanup(fake), 0, self.output)
        self.assertIn("kept driver-v1.1.2.130", self.output)
        self.assertIsNotNone(fake.release("driver-v1.1.2.130"))

    def test_attested_record_finishes_a_returned_round(self):
        fake = Fake()
        returned_release(fake)
        self.assertEqual(self.record(fake, bundle=BUNDLE), 0, self.output)
        files = fake.files(TAG)
        zip_name = "StreamToSpeaker-Driver-1.1.2.140-Signed.zip"
        self.assertEqual(files[zip_name], b"canonical zip")
        self.assertEqual(files[zip_name + ".sha256"], f"{sha(b'canonical zip')}  {zip_name}".encode())
        self.assertEqual(files[zip_name + ".sigstore.json"], BUNDLE)
        m = fake.manifest(TAG)
        self.assertEqual((m["attested"], m["signed_zip"], m["signed_zip_sha256"]),
                         (True, zip_name, sha(b"canonical zip")))
        self.assertIn("Microsoft-attested", fake.release(TAG)["name"])

    def test_attested_record_refuses_what_is_not_a_returned_round(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": {"schema": 1}})
        self.assertEqual(self.record(fake), 1)
        self.assertEqual(fake.writes(), [])


class ARecordedFileIsNeverReplaced(Base):
    """Rule 3."""

    def test_a_file_the_manifest_records_keeps_its_bytes(self):
        fake = Fake()
        returned_release(fake)
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        self.assertEqual(rel.put(b"ms zip", "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip"), "unchanged")
        with self.assertRaisesRegex(ra.ReleaseError, "a recorded file is never replaced"):
            rel.put(b"another zip", "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip")
        with self.assertRaisesRegex(ra.ReleaseError, "a recorded file is never replaced"):
            rel.put(b"another cab", CAB)
        self.assertEqual(fake.writes(), [])


def snapshot(fake, tag):
    return fake.files(tag)


class AFailedUploadLeavesTheReleaseWithWhatItHad(Base):
    """Rule 4, for every command that uploads, and for release_assets.put
    itself."""

    def test_put_keeps_the_old_file_when_its_replacement_fails_to_upload(self):
        fake = Fake()
        fake.add_release("v1", {"a.txt": b"old"})
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        fake.fail_when(fake_gh.uploading("a.txt.pending"))
        with self.assertRaises(ra.ReleaseError):
            rel.put(b"new", "a.txt")
        self.assertEqual(fake.files("v1")["a.txt"], b"old")

    def test_a_replacement_stopped_after_the_delete_is_finished_by_the_next_writer(self):
        fake = Fake()
        fake.add_release("v1", {"a.txt": b"old", "b.txt": b"b"})
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        fake.fail_when(lambda a: a[:3] == ("api", "--method", "PATCH"), fake_gh.Stop())
        with self.assertRaises(fake_gh.Stop):
            rel.put(b"new", "a.txt")
        # The bytes are on the release, under the temporary name.
        self.assertEqual(fake.files("v1"), {"b.txt": b"b", "a.txt.pending": b"new"})
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        rel.finish_interrupted()
        self.assertEqual(fake.files("v1"), {"b.txt": b"b", "a.txt": b"new"})

    def test_a_replacement_stopped_before_the_delete_is_rolled_forward_by_the_next_writer(self):
        # The new bytes were completely uploaded: the writer had decided.
        fake = Fake()
        fake.add_release("v1", {"a.txt": b"old"})
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        fake.fail_when(lambda a: a[:3] == ("api", "--method", "DELETE"), fake_gh.Stop())
        with self.assertRaises(fake_gh.Stop):
            rel.put(b"new", "a.txt")
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        rel.finish_interrupted()
        self.assertEqual(fake.files("v1"), {"a.txt": b"new"})

    def test_a_broken_pending_upload_is_removed_by_the_next_writer(self):
        fake = Fake()
        rel = fake.add_release("v1", {"a.txt": b"old"})
        fake.add_asset(rel, "a.txt.pending", b"", state="starter")
        ra.Release.find(REPO, "v1", fake, sleep=lambda s: None).finish_interrupted()
        self.assertEqual(fake.files("v1"), {"a.txt": b"old"})

    def test_a_rename_that_fails_for_a_while_is_retried(self):
        fake = Fake()
        fake.add_release("v1", {"a.txt": b"old"})
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        fake.fail_when(lambda a: a[:3] == ("api", "--method", "PATCH"))
        self.assertEqual(rel.put(b"new", "a.txt"), "replaced")
        self.assertEqual(fake.files("v1"), {"a.txt": b"new"})

    def test_unchanged_bytes_are_not_uploaded_again(self):
        fake = Fake()
        fake.add_release("v1", {"a.txt": b"same"})
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        self.assertEqual(rel.put(b"same", "a.txt"), "unchanged")
        self.assertEqual(fake.writes(), [])

    def test_a_broken_upload_of_an_earlier_run_is_replaced(self):
        fake = Fake()
        rel = fake.add_release("v1")
        fake.add_asset(rel, "a.txt", b"", state="starter")
        rel = ra.Release.find(REPO, "v1", fake, sleep=lambda s: None)
        self.assertEqual(rel.put(b"new", "a.txt"), "added")
        self.assertEqual(fake.files("v1"), {"a.txt": b"new"})

    def test_a_driver_build_whose_manifest_upload_fails_keeps_the_old_manifest(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        fake.fail_when(fake_gh.uploading("manifest.json.pending"))
        self.assertEqual(self.release(fake, cab_sha="cb" * 32), 1)
        self.assertEqual(fake.manifest(TAG), submission_manifest())

    def test_sign_cab_whose_upload_fails_leaves_the_release_as_it_was(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        before = fake.files(TAG)
        fake.fail_when(fake_gh.uploading(CAB))
        self.assertEqual(self.signed(fake), 1)
        self.assertEqual(fake.files(TAG), before)

    def test_a_stopped_sign_cab_is_finished_by_its_re_run(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        fake.fail_when(fake_gh.uploading("manifest.json.pending"), fake_gh.Stop())
        with self.assertRaises(fake_gh.Stop):
            self.signed(fake)
        self.assertEqual(fake.manifest(TAG), submission_manifest())
        self.assertEqual(self.signed(fake), 0, self.output)
        self.assertEqual(fake.manifest(TAG)["submission_cab_signed_sha256"], sha(b"signed cab"))

    def test_attested_record_whose_zip_upload_fails_leaves_the_round_unfinished(self):
        fake = Fake()
        returned_release(fake)
        before = fake.files(TAG)
        fake.fail_when(fake_gh.uploading("StreamToSpeaker-Driver-1.1.2.140-Signed.zip"))
        self.assertEqual(self.record(fake), 1)
        self.assertEqual(fake.files(TAG), before)
        self.assertEqual(self.record(fake), 0, self.output)
        self.assertIs(fake.manifest(TAG)["attested"], True)

    def test_a_manifest_left_as_pending_is_finished_by_the_next_writer(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json.pending": submission_manifest()}, target=COMMIT)
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        self.assertIsNone(rel.manifest)
        self.assertTrue(rel.interrupted)
        self.assertEqual(self.signed(fake), 0, self.output)
        self.assertEqual(sorted(fake.files(TAG)), sorted([CAB, CAB + ".sha256", "manifest.json"]))


class TheManifestIsWrittenLast(Base):
    """Rule 5, for every command that writes a manifest."""

    def assertManifestLast(self, fake):
        calls = fake.upload_calls()
        self.assertTrue(calls)
        self.assertEqual([len(calls[-1]), calls[-1][0].split(".pending")[0]], [1, "manifest.json"], calls)
        self.assertFalse([n for c in calls[:-1] for n in c if n.startswith("manifest.json")], calls)

    def test_sign_cab(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        self.assertEqual(self.signed(fake), 0, self.output)
        self.assertManifestLast(fake)

    def test_attested_record(self):
        fake = Fake()
        returned_release(fake)
        self.assertEqual(self.record(fake, bundle=BUNDLE), 0, self.output)
        self.assertManifestLast(fake)

    def test_a_manifest_that_records_a_missing_file_is_refused(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        with self.assertRaisesRegex(ra.ReleaseError, f"would record {CAB}"):
            rel.write_manifest(submission_manifest(submission_cab_signed_sha256=sha(b"x")))
        self.assertEqual(fake.writes(), [])


class DriverBuildNumbers(Base):
    """A new driver build is numbered above every driver release, and its
    build number names no other release's driver."""

    def test_a_build_below_the_certified_211_is_refused_before_anything_is_built(self):
        fake = Fake()
        certified_release(fake)
        self.assertEqual(self.check(fake, "driver-v1.1.1.135"), 1)
        self.assertIn("would number this driver below driver-v1.1.1.211", self.output)
        self.assertIn("driver_build above 211", self.output)
        self.assertEqual(self.release(fake, "driver-v1.1.1.135"), 1)
        self.assertEqual(fake.writes(), [])

    def test_a_build_above_every_release_is_accepted(self):
        fake = Fake()
        certified_release(fake)
        attested_release(fake)
        self.assertEqual(self.check(fake, "driver-v1.1.1.212"), 0, self.output)
        self.assertEqual(self.check(fake, "driver-v1.1.2.135"), 0, self.output)

    def test_a_build_number_another_release_carries_is_refused(self):
        fake = Fake()
        certified_release(fake)
        self.assertEqual(self.check(fake, "driver-v1.2.0.211"), 1)
        self.assertIn("build 211 already names the driver of driver-v1.1.1.211", self.output)


class CertifiedGuard(Base):
    """Driver submission's first job: no attestation build while a
    certified driver matches the driver source."""

    def guard(self, fake, *extra):
        return self.run_cmd(fake, "submission-guard", "--source-hash", HASH, *extra)

    def test_a_matching_certified_release_stops_the_run(self):
        fake = Fake()
        certified_release(fake)
        self.assertEqual(self.guard(fake), 0, self.output)
        self.assertEqual(self.outputs()["proceed"], "false")

    def test_force_builds_anyway(self):
        fake = Fake()
        certified_release(fake)
        self.assertEqual(self.guard(fake, "--force"), 0, self.output)
        self.assertEqual(self.outputs()["proceed"], "true")

    def test_a_certified_release_beyond_the_first_page_is_found(self):
        fake = Fake()
        certified_release(fake, created_at="2026-01-01T00:00:00Z")
        for i in range(120):
            fake.add_release(f"v0.{i}.0", prerelease=False, created_at=f"2026-06-01T00:{i // 60:02d}:{i % 60:02d}Z")
        self.assertEqual(self.guard(fake), 0, self.output)
        self.assertEqual(self.outputs()["proceed"], "false")

    def test_a_manifest_that_cannot_be_read_stops_the_run_instead_of_proceeding(self):
        fake = Fake()
        certified_release(fake)
        fake.fail_when(lambda a: a[:2] == ("release", "download"))
        self.assertEqual(self.guard(fake), 1)
        self.assertNotIn("proceed", self.outputs())

    def test_no_match_proceeds(self):
        fake = Fake()
        attested_release(fake)
        self.assertEqual(self.guard(fake), 0, self.output)
        self.assertEqual(self.outputs()["proceed"], "true")


class AttestedFetch(Base):
    def fetch(self, fake, *extra):
        return self.run_cmd(fake, "attested-fetch", "--dir", os.path.join(self.tmp, "assets"), *extra)

    def test_without_a_tag_the_newest_returned_round_is_taken(self):
        fake = Fake()
        attested_release(fake, "1.1.2.211", created_at="2026-02-01T00:00:00Z")
        returned_release(fake, "1.1.2.140")
        self.assertEqual(self.fetch(fake), 0, self.output)
        out = self.outputs()
        self.assertEqual((out["tag"], out["version"]), (TAG, "1.1.2.140"))
        with open(out["cab"], "rb") as fh:
            self.assertEqual(fh.read(), b"signed cab")
        self.assertEqual(out["ms_zip_name"], "StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip")

    def test_a_cab_that_is_not_the_submitted_one_is_refused(self):
        fake = Fake()
        returned_release(fake)
        fake.release(TAG)["assets"] = [a for a in fake.release(TAG)["assets"] if a["name"] != CAB]
        fake.add_asset(fake.release(TAG), CAB, b"another cab")
        self.assertEqual(self.fetch(fake, "--tag", TAG), 1)
        self.assertIn("it is not the CAB that was submitted", self.output)

    def test_a_finished_release_is_fetched_only_for_a_dry_run(self):
        fake = Fake()
        attested_release(fake)
        self.assertEqual(self.fetch(fake, "--tag", "driver-v1.1.0.204"), 1)
        self.assertEqual(self.fetch(fake, "--tag", "driver-v1.1.0.204", "--dry-run"), 0, self.output)
        self.assertEqual(fake.writes(), [])

    def test_no_returned_round(self):
        fake = Fake()
        attested_release(fake)
        self.assertEqual(self.fetch(fake), 1)
        self.assertIn("no unfinished driver-v* release with an attached Microsoft zip", self.output)


class TestSignedFallback(Base):
    """A tag run whose driver lookup found no Microsoft-signed driver."""
    TAGGED = "f" * 40

    def fake(self):
        fake = Fake()
        certified_release(fake)
        fake.tags[("tree", "e" * 40)] = {"driver": "d1", "include": "i1", "service": "s1"}
        return fake

    def fallback(self, fake):
        return self.run_cmd(fake, "fallback-check", "--source-hash", "7" * 64, "--commit", self.TAGGED)

    def test_a_driver_released_from_the_same_trees_stops_the_fallback(self):
        # The tree is the certified one, but its hash came out different
        # (runner image, .gitattributes): shipping test-signed would be wrong.
        fake = self.fake()
        fake.tags[("tree", self.TAGGED)] = {"driver": "d1", "include": "i1", "service": "s2"}
        self.assertEqual(self.fallback(fake), 1)
        self.assertIn("driver-v1.1.1.211 was built from the same driver/ and include/ trees", self.output)
        self.assertIn("5" * 64, self.output)

    def test_another_driver_tree_may_fall_back_and_says_what_differs(self):
        fake = self.fake()
        fake.tags[("tree", self.TAGGED)] = {"driver": "d2", "include": "i1"}
        self.assertEqual(self.fallback(fake), 0, self.output)
        self.assertIn("driver-v1.1.1.211", self.output)
        self.assertIn("5" * 64, self.output)
        self.assertIn("7" * 64, self.output)

    def test_a_tree_that_cannot_be_read_stops_the_fallback(self):
        fake = self.fake()
        self.assertEqual(self.fallback(fake), 1)
        self.assertEqual(fake.writes(), [])

    def test_a_released_commit_that_is_gone_is_not_compared(self):
        fake = self.fake()
        del fake.tags[("tree", "e" * 40)]
        fake.tags[("tree", self.TAGGED)] = {"driver": "d1", "include": "i1"}
        self.assertEqual(self.fallback(fake), 0, self.output)
        self.assertIn("not compared", self.output)


class TwoRunsAtOnce(Base):
    """A write is decided on the release as it is at that moment: a run
    refuses to write a release another run (or a person) changed since this
    run read it or last wrote it."""

    def test_a_run_that_read_the_release_before_another_finished_it_writes_nothing(self):
        fake = Fake()
        returned_release(fake)
        early = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)     # run 1 reads the release
        self.assertEqual(self.record(fake), 0, self.output)                 # run 2 finishes it
        with self.assertRaises(ra.ReleaseError):
            early.write_manifest(dict(early.manifest, ms_state="done"))
        with self.assertRaises(ra.ReleaseError):
            early.put(b"x", "notes.txt")
        self.assertIs(fake.manifest(TAG)["attested"], True)
        self.assertNotIn("notes.txt", fake.files(TAG))

    def test_a_release_changed_by_hand_since_this_run_read_it_is_not_written(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        fake.add_asset(fake.release(TAG), "attached-by-hand.zip", b"zip")
        writes = len(fake.writes())
        with self.assertRaisesRegex(ra.ReleaseError, "changed since this run"):
            rel.write_manifest(submission_manifest(submission_cab_sha256="cb" * 32))
        self.assertEqual(len(fake.writes()), writes)
        self.assertEqual(fake.manifest(TAG), submission_manifest())

    def test_its_own_writes_do_not_count_as_a_change(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        rel.put(b"a", "a.txt")
        rel.put(b"b", "a.txt")
        rel.write_manifest(submission_manifest(submission_cab_sha256="cb" * 32))
        self.assertEqual(fake.files(TAG)["a.txt"], b"b")

    def test_another_release_given_the_same_tag_is_not_written(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        fake.releases[0]["tag_name"] = "driver-v1.1.2.141"
        fake.add_release(TAG, {"manifest.json": submission_manifest()}, target=COMMIT)
        with self.assertRaises(ra.ReleaseError):
            rel.put(b"a", "a.txt")

    def test_two_writers_interleaved_never_leave_only_a_pending_manifest(self):
        # Writer A has uploaded manifest.json.pending and is about to delete
        # the old manifest; writer B, which read the release just then,
        # replaces manifest.json too. Whatever B does, the release ends with
        # a complete manifest.json, and B never deletes A's upload.
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": b"old"})
        a = ra.Release.find(REPO, TAG, fake, sleep=lambda s: None)
        old_id = a.asset("manifest.json")["id"]
        results = {}

        def run(name, rel, data, stopped=None):
            try:
                results[name] = rel.put(data, "manifest.json")
            except Exception as e:  # each writer's own outcome is reported
                results[name] = e
            if stopped:
                stopped.set()

        def deleting_old(thread):
            return lambda c: (threading.current_thread() is thread and c[:3] == ("api", "--method", "DELETE")
                              and c[3].endswith(f"/{old_id}"))
        ta = threading.Thread(target=run, args=("A", a, b"A"))
        a_at_delete = fake.pause_before(deleting_old(ta))
        ta.start()
        self.assertTrue(a_at_delete["arrived"].wait(30))
        b = ra.Release.find(REPO, TAG, fake, sleep=lambda s: None)
        b_stopped = threading.Event()
        tb = threading.Thread(target=run, args=("B", b, b"B", b_stopped))
        b_at_delete = fake.pause_before(deleting_old(tb), arrived=b_stopped)
        tb.start()
        self.assertTrue(b_stopped.wait(30))
        a_at_delete["go"].set()
        ta.join(30)
        b_at_delete["go"].set()
        tb.join(30)
        files = fake.files(TAG)
        self.assertIn("manifest.json", files, (results, sorted(files)))
        self.assertIn(files["manifest.json"], (b"A", b"B"))
        self.assertNotIn("manifest.json.pending", files)

    def test_put_never_deletes_a_pending_upload_it_did_not_make(self):
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": b"old", "manifest.json.pending": b"another run's"})
        rel = ra.Release.find(REPO, TAG, fake, sleep=lambda s: None)
        with self.assertRaisesRegex(ra.ReleaseError, "manifest.json.pending"):
            rel.put(b"mine", "manifest.json")
        self.assertEqual(fake.files(TAG)["manifest.json.pending"], b"another run's")
        self.assertEqual(fake.writes(), [])


class AnInterruptedManifestWriteIsFinishedByTheReRun(Base):
    """attested-record's last write replaces manifest.json with the finished
    one. Wherever that write stops, every reader treats the release
    correctly and the re-run completes it."""
    PENDING = "manifest.json.pending"

    def stop_record(self, fake, predicate, exc=None, times=1):
        for _ in range(times):
            fake.fail_when(predicate, exc)
        try:
            return self.record(fake)
        except fake_gh.Stop:
            return "stopped"

    def assertReRunFinishes(self, fake):
        self.assertEqual(self.record(fake), 0, self.output)
        files = fake.files(TAG)
        self.assertNotIn(self.PENDING, files)
        self.assertIs(fake.manifest(TAG)["attested"], True)
        self.assertEqual(fake.manifest(TAG)["ms_submission_id"], "8")

    def test_the_rename_fails(self):
        fake = Fake()
        returned_release(fake)
        rename = lambda c: c[:3] == ("api", "--method", "PATCH") and "/releases/assets/" in c[3]
        self.assertEqual(self.stop_record(fake, rename, times=3), 1)
        self.assertIn(self.PENDING, fake.files(TAG))
        self.assertNotIn("manifest.json", fake.files(TAG))
        self.assertReRunFinishes(fake)

    def test_stopped_after_the_old_manifest_is_deleted(self):
        fake = Fake()
        returned_release(fake)
        rename = lambda c: c[:3] == ("api", "--method", "PATCH") and "/releases/assets/" in c[3]
        self.assertEqual(self.stop_record(fake, rename, fake_gh.Stop()), "stopped")
        self.assertReRunFinishes(fake)

    def test_stopped_before_the_old_manifest_is_deleted(self):
        fake = Fake()
        returned_release(fake)
        old_id = next(a["id"] for a in fake.release(TAG)["assets"] if a["name"] == "manifest.json")
        delete = lambda c: c[:3] == ("api", "--method", "DELETE") and c[3].endswith(f"/{old_id}")
        self.assertEqual(self.stop_record(fake, delete, fake_gh.Stop()), "stopped")
        # Readers still see the old, unfinished manifest.
        self.assertIsNot(dr.DriverRelease(REPO, TAG, fake).manifest.get("attested"), True)
        self.assertReRunFinishes(fake)

    def test_stopped_during_the_pending_upload(self):
        fake = Fake()
        returned_release(fake)
        fake.broken_uploads.add(self.PENDING)
        self.assertEqual(self.record(fake), 1)
        self.assertEqual(self.record(fake), 0, self.output)
        self.assertIs(fake.manifest(TAG)["attested"], True)
        self.assertNotIn(self.PENDING, fake.files(TAG))

    def test_readers_never_take_the_pending_manifest_for_the_release_s(self):
        fake = Fake()
        returned_release(fake)
        rename = lambda c: c[:3] == ("api", "--method", "PATCH") and "/releases/assets/" in c[3]
        self.stop_record(fake, rename, times=3)
        self.assertIsNone(dr.DriverRelease(REPO, TAG, fake).manifest)
        with self.assertRaisesRegex(ra.ReleaseError, "manifest.json.pending"):
            dr.manifests(REPO, dr.driver_releases(REPO, fake), fake)
        # The certified guard and the fallback check stop instead of answering.
        self.assertEqual(self.run_cmd(fake, "submission-guard", "--source-hash", HASH), 1)
        self.assertEqual(self.run_cmd(fake, "fallback-check", "--source-hash", "7" * 64, "--commit", "f" * 40), 1)


class FinishingAnInterruptedWriteKeepsWhatItRecorded(Base):
    def test_a_complete_pending_manifest_is_rolled_forward_not_dropped(self):
        # Driver attest recorded the new Partner Center ids, and the run
        # stopped after manifest.json.pending was up, before the old
        # manifest was deleted: the next writer keeps the ids.
        fake = Fake()
        fake.add_release(TAG, {"manifest.json": submission_manifest(),
                               "manifest.json.pending": submission_manifest(ms_product_id="111",
                                                                            ms_submission_id="222",
                                                                            ms_state="created")})
        rel = dr.DriverRelease(REPO, TAG, fake, sleep=lambda s: None)
        rel.begin()
        self.assertEqual(fake.manifest(TAG)["ms_submission_id"], "222")
        self.assertEqual(rel.manifest["ms_submission_id"], "222")
        self.assertNotIn("manifest.json.pending", fake.files(TAG))

    def test_nothing_is_rolled_forward_over_a_finished_manifest(self):
        fake = Fake()
        rel = certified_release(fake)
        fake.add_asset(rel, "manifest.json.pending", json.dumps(certified_manifest(certified=False)).encode())
        before = fake.files("driver-v1.1.1.211")
        dr.DriverRelease(REPO, "driver-v1.1.1.211", fake).begin()
        self.assertEqual(fake.files("driver-v1.1.1.211"), before)


class APendingManifestIsPutInPlaceOnlyIfAWriteWouldHaveWrittenIt(Base):
    """What a stopped run of these workflows leaves passed write_manifest's
    checks before it went up. Anything else (placed or left by someone
    else) is reported and left for a person."""

    def returned_with_pending(self, pending):
        fake = Fake()
        rel = returned_release(fake)
        fake.add_asset(rel, "manifest.json.pending", pending)
        return fake

    def finishing(self, **kw):
        return dict(submission_manifest(submission_cab_signed_sha256=sha(b"signed cab"), ms_product_id="7",
                                        ms_submission_id="8", ms_state="done",
                                        ms_zip_asset="StreamToSpeaker-Driver-1.1.2.140-Microsoft.zip"),
                    attested=True, signed_zip=SIGNED_ZIP, signed_zip_sha256=sha(b"canonical zip"), **kw)

    def assertLeftForAPerson(self, fake, before, says):
        self.assertEqual(self.record(fake), 1)
        self.assertIn("manifest.json.pending", self.output)
        self.assertIn(says, self.output)
        self.assertEqual(fake.files(TAG), before)

    def test_a_finishing_manifest_that_records_a_missing_file(self):
        fake = self.returned_with_pending(self.finishing())
        self.assertLeftForAPerson(fake, fake.files(TAG), SIGNED_ZIP)

    def test_a_finishing_manifest_whose_file_has_other_bytes(self):
        fake = self.returned_with_pending(self.finishing())
        fake.add_asset(fake.release(TAG), SIGNED_ZIP, b"other bytes")
        self.assertLeftForAPerson(fake, fake.files(TAG), "sha256")

    def test_a_pending_manifest_that_is_not_json(self):
        fake = self.returned_with_pending(b"not json")
        self.assertLeftForAPerson(fake, fake.files(TAG), "JSON")

    def test_a_finishing_manifest_whose_files_are_there_is_put_in_place(self):
        fake = self.returned_with_pending(self.finishing())
        fake.add_asset(fake.release(TAG), SIGNED_ZIP, b"canonical zip")
        self.assertEqual(self.record(fake), 0, self.output)
        self.assertIs(fake.manifest(TAG)["attested"], True)


class ABrokenFirstManifestBlocksNobody(Base):
    def test_the_run_that_wrote_it_replaces_it(self):
        fake = Fake()
        rel = fake.add_release(TAG, target=COMMIT)
        fake.add_asset(rel, "manifest.json", b"", state="starter")
        self.assertIsNone(dr.DriverRelease(REPO, TAG, fake).manifest)
        self.assertEqual(self.check(fake), 0, self.output)
        self.assertEqual(self.release(fake), 0, self.output)
        self.assertEqual(fake.manifest(TAG), submission_manifest())


class CleanupKeepsWhatItCannotShowIsAnUntouchedBuild(Base):
    HEAD = "d" * 40

    def cleanup_from(self, fake):
        return self.run_cmd(fake, "submission-cleanup", "--tag", TAG, "--commit", self.HEAD)

    def old(self, fake, version="1.1.2.130", assets=None, ancestor=True, **kw):
        m = submission_manifest(version, **kw)
        rel = fake.add_release(f"driver-v{version}", {"manifest.json": m, **(assets or {})}, target=COMMIT)
        if ancestor:
            fake.compares[(COMMIT, self.HEAD)] = "ahead"
        return rel

    def test_an_untouched_unsigned_build_of_main_is_deleted(self):
        fake = Fake()
        self.old(fake)
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        self.assertEqual(self.cleanup_from(fake), 0, self.output)
        self.assertIsNone(fake.release("driver-v1.1.2.130"))

    def test_an_ev_signed_cab_the_manifest_does_not_record_yet_is_kept(self):
        fake = Fake()
        self.old(fake, assets={"StreamToSpeaker-Driver-1.1.2.130-Submission.cab": b"approved, signed"})
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        self.assertEqual(self.cleanup_from(fake), 0, self.output)
        self.assertIsNotNone(fake.release("driver-v1.1.2.130"))
        self.assertIn("driver-v1.1.2.130", open(self.summary).read())

    def test_anything_else_is_kept(self):
        cases = {
            "an unknown manifest field": dict(extra_field="x"),
            "a recorded signature": dict(submission_cab_signed_sha256=sha(b"s")),
            "a Partner Center id": dict(ms_submission_id="5"),
            "not an ancestor of main": dict(ancestor=False),
        }
        for what, kw in cases.items():
            with self.subTest(what):
                fake = Fake()
                self.old(fake, **kw)
                fake.add_release(TAG, {"manifest.json": submission_manifest()})
                self.assertEqual(self.cleanup_from(fake), 0, self.output)
                self.assertIsNotNone(fake.release("driver-v1.1.2.130"), what)

    def test_a_list_that_cannot_be_read_deletes_nothing(self):
        fake = Fake()
        self.old(fake)
        fake.add_release(TAG, {"manifest.json": submission_manifest()})
        fake.fail_when(lambda c: "--paginate" in c)
        self.assertEqual(self.cleanup_from(fake), 0, self.output)
        self.assertIsNotNone(fake.release("driver-v1.1.2.130"))


class TheRunSummaryKeepsEachManifestWritten(Base):
    def test_the_manifest_that_finished_a_release_is_in_the_summary(self):
        # What a maintainer puts back if a write from outside the group
        # replaced it in the moment before this run's write.
        fake = Fake()
        returned_release(fake)
        self.assertEqual(self.record(fake), 0, self.output)
        text = open(self.summary).read()
        self.assertIn("manifest.json written to driver-v1.1.2.140", text)
        self.assertIn(json.dumps(fake.manifest(TAG), indent=2), text)


class NoTemporaryFolderIsLeftBehind(Base):
    def test_commands_remove_what_they_create(self):
        tmp = tempfile.mkdtemp(dir=self.tmp)
        with unittest.mock.patch.object(tempfile, "tempdir", tmp):
            fake = Fake()
            returned_release(fake)
            self.assertEqual(self.record(fake, bundle=BUNDLE), 0, self.output)
            fake.add_release("driver-v1.1.2.150", target=COMMIT)
            self.assertEqual(self.release(fake, "driver-v1.1.2.150"), 0, self.output)
            self.assertEqual(self.run_cmd(fake, "attested-fetch", "--tag", TAG, "--dry-run",
                                          "--dir", os.path.join(self.tmp, "a")), 0, self.output)
        self.assertEqual(os.listdir(tmp), [])


class Words(unittest.TestCase):
    def test_the_next_step_says_whether_attestation_starts_by_itself(self):
        on = dr.submission_notes(TAG, "1.1.2.140", COMMIT, HASH, CAB_SHA, True)
        off = dr.submission_notes(TAG, "1.1.2.140", COMMIT, HASH, CAB_SHA, False)
        self.assertIn("starts by itself", on)
        self.assertIn("It does not start by itself", off)
        self.assertIn(f"`tag` = `{TAG}`", off)
        for text in (on, off):
            self.assertNotIn("That's it", text)


if __name__ == "__main__":
    unittest.main()
