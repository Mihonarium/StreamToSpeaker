import hashlib
import io
import itertools
import json
import os
import shutil
import struct
import subprocess
import tempfile
import unittest
import unittest.mock
import zipfile

import certified
import fake_gh
import sys
from test_release_draft import bundle_for

# The real 1.1.0.209 packages, when this box has them (dev machine only;
# CI runs the synthetic tests).
LAB_ZIP = os.path.expanduser("~/StreamToSpeaker-hlk/sts-driver-209.zip")
MS_ZIP = os.path.expanduser("~/StreamToSpeaker-hlk/signed/signed-whcp.zip")
HAVE_REAL = os.path.exists(LAB_ZIP) and os.path.exists(MS_ZIP)


def make_pe(code=b"CODE" * 64, cert=b""):
    """Minimal PE32+ image: DOS stub, PE signature, COFF, optional header
    with 16 data directories, then `code`, then an optional cert table."""
    e_lfanew = 0x40
    dos = bytearray(b"MZ" + b"\0" * (e_lfanew - 2))
    struct.pack_into("<I", dos, 0x3C, e_lfanew)
    coff = struct.pack("<HHIIIHH", 0x8664, 0, 0, 0, 0, 240, 0)
    opt = bytearray(240)
    struct.pack_into("<H", opt, 0, 0x20B)
    struct.pack_into("<I", opt, 64, 0x12345678)  # CheckSum
    header = bytes(dos) + b"PE\0\0" + coff
    body = header + bytes(opt) + code
    if cert:
        struct.pack_into("<II", opt, 112 + 4 * 8, len(body), len(cert))
        body = header + bytes(opt) + code + cert
    return body


class HashFiles(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        os.makedirs(os.path.join(self.tmp, "driver", "sub"))
        os.makedirs(os.path.join(self.tmp, "include"))
        self.write("driver/b.cpp", b"int b;\n")
        self.write("driver/A.h", b"int a;\n")
        self.write("driver/sub/x.h", b"x\r\n")
        self.write("driver/bin.dat", b"\0\n\0")
        self.write("include/i.h", b"i\n")

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def write(self, rel, data):
        with open(os.path.join(self.tmp, rel), "wb") as f:
            f.write(data)

    def test_ntfs_order_crlf_and_binary_rules(self):
        # NTFS collation: A.h, b.cpp, bin.dat, sub/x.h (dir descended in
        # place), then include/. LF text hashed as CRLF; CR-containing and
        # NUL-containing files untouched.
        digests = [hashlib.sha256(b).digest() for b in (
            b"int a;\r\n", b"int b;\r\n", b"\0\n\0", b"x\r\n", b"i\r\n")]
        want = hashlib.sha256(b"".join(digests)).hexdigest()
        self.assertEqual(certified.hash_files(self.tmp), want)

    def test_empty_when_nothing_matches(self):
        self.assertEqual(certified.hash_files(os.path.join(self.tmp, "include")), "")

    def test_reproduces_attested_204_manifest(self):
        # driver-v1.1.0.204's manifest records source_hash for commit
        # cde07512, computed by hashFiles on windows-2022. If that commit is
        # reachable here, the algorithm must reproduce it.
        repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
        if subprocess.run(["git", "-C", repo, "cat-file", "-e", "cde07512d466b2e09fa278505487be98981ef723^{commit}"],
                          capture_output=True).returncode != 0:
            self.skipTest("commit cde07512 not available")
        tmp = tempfile.mkdtemp()
        try:
            tar = subprocess.run(["git", "-C", repo, "archive", "cde07512", "driver", "include"],
                                 check=True, capture_output=True).stdout
            subprocess.run(["tar", "-x", "-C", tmp], input=tar, check=True)
            self.assertEqual(certified.hash_files(tmp),
                             "80acdc4b860b5a171f0c6514388e8d8667741c331a19f0cf91870114d163115f")
        finally:
            shutil.rmtree(tmp)


class PeStrip(unittest.TestCase):
    def test_strips_cert_table_and_checksum(self):
        unsigned = make_pe()
        signed = make_pe(cert=b"\x30\x82SIG" + b"\0" * 40)
        self.assertNotEqual(unsigned, signed)
        self.assertEqual(certified.pe_strip_signature(signed), certified.pe_strip_signature(unsigned))
        # The stripped image carries neither a cert table nor a checksum.
        stripped = certified.pe_strip_signature(signed)
        self.assertEqual(len(stripped), len(unsigned))
        self.assertEqual(certified.pe_certificate_table(stripped), b"")

    def test_different_code_stays_different(self):
        a = certified.pe_strip_signature(make_pe(code=b"A" * 64, cert=b"sig1"))
        b = certified.pe_strip_signature(make_pe(code=b"B" * 64, cert=b"sig1"))
        self.assertNotEqual(a, b)

    def test_same_code_matches_signed_against_unsigned(self):
        unsigned = make_pe()
        self.assertTrue(certified.pe_same_code(make_pe(cert=b"sig" * 10), unsigned))
        self.assertFalse(certified.pe_same_code(make_pe(code=b"X" * 256, cert=b"sig"), unsigned))

    def test_same_code_tolerates_alignment_padding_only(self):
        unsigned = make_pe(code=b"C" * 61)
        padded = make_pe(code=b"C" * 61 + b"\0" * 3, cert=b"sig")
        self.assertTrue(certified.pe_same_code(padded, unsigned))
        self.assertTrue(certified.pe_same_code(unsigned, padded))
        self.assertFalse(certified.pe_same_code(make_pe(code=b"C" * 61 + b"\0" * 8, cert=b"sig"), unsigned))
        self.assertFalse(certified.pe_same_code(make_pe(code=b"C" * 61 + b"\0\1", cert=b"sig"), unsigned))

    def test_sys_match_command(self):
        d = tempfile.mkdtemp()
        try:
            signed, sub, other = (os.path.join(d, n) for n in ("s.sys", "u.sys", "o.sys"))
            for path, data in ((signed, make_pe(cert=b"sig")), (sub, make_pe()), (other, make_pe(code=b"Z" * 256))):
                with open(path, "wb") as f:
                    f.write(data)
            self.assertEqual(certified.main(["sys-match", "--signed", signed, "--submitted", sub]), 0)
            self.assertEqual(certified.main(["sys-match", "--signed", signed, "--submitted", other]), 1)
        finally:
            shutil.rmtree(d)

    def test_rejects_non_pe(self):
        with self.assertRaises(ValueError):
            certified.pe_strip_signature(b"not a pe file")

    def test_rejects_cert_table_not_at_end(self):
        pe = bytearray(make_pe(cert=b"sig"))
        pe += b"trailing"
        with self.assertRaises(ValueError):
            certified.pe_strip_signature(bytes(pe))


def zip_of(files):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for name, data in files.items():
            z.writestr(name, data)
    return buf.getvalue()


def zip_with_host(ms_zip, host):
    """canonical_zip's bytes as a build that records `host` would make them."""
    real = zipfile.ZipInfo.__init__

    def init(self, *a, **kw):
        real(self, *a, **kw)
        self.create_system = host
    with unittest.mock.patch.object(zipfile.ZipInfo, "__init__", init), \
            unittest.mock.patch.object(certified, "ZIP_HOST", host, create=True):
        return certified.canonical_zip(ms_zip)


def fake_packages(sys_code=b"K" * 64):
    inf = b"[Version]\r\nDriverVer = 09/18/2026,1.1.0.209\r\n"
    lab = zip_of({"StreamToSpeaker.inf": inf,
                  "StreamToSpeaker.sys": make_pe(sys_code, cert=b"CN=HLK Test"),
                  "streamtospeaker.cat": b"labcat"})
    ms = zip_of({"drivers/guid/StreamToSpeaker.inf": inf,
                 "drivers/guid/StreamToSpeaker.sys": make_pe(sys_code, cert=b"CN=HLK Test" + certified.MS_PUBLISHER),
                 "drivers/guid/streamtospeaker.cat": b"cat" + certified.MS_PUBLISHER})
    return ms, lab


class VerifyPackages(unittest.TestCase):
    def test_matching_packages_pass(self):
        ms, lab = fake_packages()
        self.assertEqual(certified.verify_packages(ms, lab, "1.1.0.209"), [])

    def test_wrong_version(self):
        ms, lab = fake_packages()
        self.assertTrue(any("DriverVer" in p for p in certified.verify_packages(ms, lab, "1.1.0.210")))

    def test_a_version_that_is_only_the_start_of_the_infs_is_wrong(self):
        # ",1.1.0.20" is a substring of ",1.1.0.209": the INF's DriverVer
        # must be the version, not merely contain it.
        ms, lab = fake_packages()
        for version in ("1.1.0.20", "1.1.0.2", "1.1.0", "1.0.209"):
            self.assertTrue(any("DriverVer" in p for p in certified.verify_packages(ms, lab, version)), version)

    def test_inf_mismatch(self):
        ms, _ = fake_packages()
        _, lab = fake_packages()
        lab = zip_of({**{i.filename: zipfile.ZipFile(io.BytesIO(lab)).read(i) for i in zipfile.ZipFile(io.BytesIO(lab)).infolist()},
                      "StreamToSpeaker.inf": b"different"})
        self.assertTrue(any("INF differs" in p for p in certified.verify_packages(ms, lab)))

    def test_sys_code_mismatch(self):
        ms, _ = fake_packages(b"K" * 64)
        _, lab = fake_packages(b"L" * 64)
        self.assertTrue(any(".sys code differs" in p for p in certified.verify_packages(ms, lab)))

    def test_missing_microsoft_signature(self):
        _, lab = fake_packages()
        ms = lab  # the lab package itself has no Microsoft signature
        problems = certified.verify_packages(ms, lab)
        self.assertTrue(any("no Microsoft Windows Hardware Compatibility Publisher" in p for p in problems))
        self.assertTrue(any(".cat is not signed" in p for p in problems))

    def test_missing_file_raises(self):
        with self.assertRaises(ValueError):
            certified.read_package(zip_of({"StreamToSpeaker.inf": b"x"}))

    def test_duplicate_sys_raises(self):
        with self.assertRaises(ValueError):
            certified.read_package(zip_of({"a/StreamToSpeaker.sys": b"1", "b/StreamToSpeaker.sys": b"2",
                                           "StreamToSpeaker.inf": b"", "StreamToSpeaker.cat": b""}))

    def test_canonical_zip_is_the_same_on_every_platform(self):
        # zipfile records the platform that made an entry (the "version
        # made by" host: 0 on Windows, 3 elsewhere), so the same inputs made
        # different zips on the Windows verify job and the Linux publish
        # job. The builder pins it to 0, the host of every Windows-made
        # package (driver-v1.1.1.211's published one among them).
        ms, _ = fake_packages()
        made = {}
        real = sys.platform
        for platform in ("win32", "linux"):
            sys.platform = platform
            try:
                made[platform] = certified.canonical_zip(ms)
            finally:
                sys.platform = real
        self.assertEqual(made["win32"], made["linux"])
        self.assertEqual({i.create_system for i in zipfile.ZipFile(io.BytesIO(made["linux"])).infolist()}, {0})
        # What used to differ: only that host byte of each central
        # directory entry.
        unix = zip_with_host(ms, 3)
        self.assertEqual(len(unix), len(made["linux"]))
        differ = [i for i, (a, b) in enumerate(zip(unix, made["linux"])) if a != b]
        self.assertEqual(len(differ), len(certified.PACKAGE_FILES))
        self.assertTrue(all(unix[i] == 3 and made["linux"][i] == 0 for i in differ))

    def test_canonical_zip_is_flat_and_deterministic(self):
        ms, _ = fake_packages()
        z1, z2 = certified.canonical_zip(ms), certified.canonical_zip(ms)
        self.assertEqual(z1, z2)
        self.assertEqual(sorted(zipfile.ZipFile(io.BytesIO(z1)).namelist()), sorted(certified.PACKAGE_FILES))


class PublishingTheRelease(unittest.TestCase):
    """`certified.py release` against a fake of the release it writes."""
    REPO = "owner/repo"
    TAG = "driver-v1.1.0.209"
    LAB = "StreamToSpeaker-Driver-1.1.0.209-lab-testsigned.zip"
    SIGNED = "StreamToSpeaker-Driver-1.1.0.209-Signed.zip"
    COMMIT = "a" * 40

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.old_cwd = os.getcwd()
        os.chdir(self.tmp)
        self.ms, self.lab = fake_packages()
        for name, data in (("ms.zip", self.ms), ("lab.zip", self.lab)):
            with open(name, "wb") as fh:
                fh.write(data)

    def tearDown(self):
        os.chdir(self.old_cwd)
        shutil.rmtree(self.tmp)

    def fake(self, **assets):
        fake = fake_gh.FakeReleases(self.REPO)
        fake.add_release(self.TAG, {self.LAB: self.lab, **assets}, target=self.COMMIT)
        return fake

    def release(self, fake, *extra, signed=None):
        # The canonical package as the verify job hands it to publish.
        if not os.path.exists("signed.zip") or signed is not None:
            with open("signed.zip", "wb") as fh:
                fh.write(certified.canonical_zip(self.ms) if signed is None else signed)
        argv = ["release", "--repo", self.REPO, "--tag", self.TAG, "--version", "1.1.0.209",
                "--commit", self.COMMIT, "--source-hash", "5" * 64, "--product-id", "1",
                "--submission-id", "2", "--ms-zip", "ms.zip", "--lab-zip", "lab.zip",
                "--signed-zip", "signed.zip", *extra]
        with unittest.mock.patch("subprocess.run", fake_gh.subprocess_run(fake)), \
                unittest.mock.patch("sys.stdout", io.StringIO()):
            try:
                return certified.main(argv)
            except Exception as e:  # what the workflow step would see as a failure
                return e

    def test_the_lab_package_on_the_release_is_never_uploaded_again(self):
        fake = self.fake()
        self.assertEqual(self.release(fake), 0)
        self.assertNotIn(self.LAB, fake.uploaded_names())
        self.assertEqual(fake.files(self.TAG)[self.LAB], self.lab)
        self.assertIs(fake.manifest(self.TAG)["certified"], True)

    def test_manifest_json_is_uploaded_last_in_a_call_of_its_own(self):
        fake = self.fake()
        self.assertEqual(self.release(fake), 0)
        calls = fake.upload_calls()
        self.assertEqual(len(calls[-1]), 1)
        self.assertTrue(calls[-1][0].startswith("manifest.json"), calls)
        self.assertFalse([n for c in calls[:-1] for n in c if n.startswith("manifest.json")], calls)

    def test_a_failed_upload_leaves_the_release_with_what_it_had(self):
        # An earlier attempt left a Signed.zip; this attempt's upload of
        # it fails half way. The release keeps a Signed.zip and the lab
        # package, and gets no manifest that names a missing file.
        fake = self.fake(**{self.SIGNED: b"an earlier attempt's zip"})
        fake.clobber_upload_fails.add(self.SIGNED)
        fake.fail_when(fake_gh.uploading(self.SIGNED + ".pending"))
        self.assertNotEqual(self.release(fake), 0)
        files = fake.files(self.TAG)
        self.assertEqual(files[self.SIGNED], b"an earlier attempt's zip")
        self.assertEqual(files[self.LAB], self.lab)
        self.assertNotIn("manifest.json", files)

    def test_what_is_published_is_the_file_it_is_given(self):
        # The file the verify job made and the provenance names, not one
        # made again here.
        other_host = zip_with_host(self.ms, 3)
        fake = self.fake()
        self.assertEqual(self.release(fake, signed=other_host), 0)
        self.assertEqual(fake.files(self.TAG)[self.SIGNED], other_host)
        self.assertEqual(fake.manifest(self.TAG)["signed_zip_sha256"], hashlib.sha256(other_host).hexdigest())

    def test_a_file_the_provenance_does_not_name_is_refused(self):
        fake = self.fake()
        with open("bundle.json", "wb") as fh:
            fh.write(bundle_for(b"another zip", self.SIGNED))
        self.assertNotEqual(self.release(fake, "--bundle", "bundle.json"), 0)
        self.assertEqual(fake.writes(), [])
        with open("bundle.json", "wb") as fh:
            fh.write(bundle_for(certified.canonical_zip(self.ms), self.SIGNED))
        self.assertEqual(self.release(fake, "--bundle", "bundle.json"), 0)
        self.assertIn(self.SIGNED + ".sigstore.json", fake.files(self.TAG))

    def test_a_zip_that_is_not_microsofts_package_is_refused(self):
        fake = self.fake()
        self.assertNotEqual(self.release(fake, signed=certified.canonical_zip(fake_packages(b"L" * 64)[0])), 0)
        self.assertEqual(fake.writes(), [])

    def test_a_certified_release_is_never_changed(self):
        fake = self.fake()
        self.assertEqual(self.release(fake), 0)
        before = fake.files(self.TAG)
        writes = len(fake.writes())
        # The same inputs again change nothing and succeed...
        self.assertEqual(self.release(fake), 0)
        self.assertEqual(len(fake.writes()), writes)
        # ...other inputs are refused.
        self.assertNotEqual(self.release(fake, "--report-id", "95342530"), 0)
        self.assertEqual(len(fake.writes()), writes)
        self.assertEqual(fake.files(self.TAG), before)

    def bundle(self, attempt):
        """bundle.json: the provenance bundle one attempt of the job gets
        for the canonical zip (every attempt gets another). Its bytes."""
        bundle = json.loads(bundle_for(certified.canonical_zip(self.ms), self.SIGNED))
        bundle["verificationMaterial"] = {"tlogEntries": [{"logIndex": str(attempt)}]}
        with open("bundle.json", "wb") as fh:
            fh.write(json.dumps(bundle).encode())
        return json.dumps(bundle).encode()

    def test_a_stopped_upload_of_the_bundle_is_made_again_by_the_re_run(self):
        # One attempt put its bundle up and failed later; the next stopped
        # while replacing it with its own; the third finds the second's as
        # <name>.pending and uploads its own.
        name = self.SIGNED + ".sigstore.json"
        fake = self.fake(**{name: self.bundle(1)})
        fake.fail_when(lambda call: call[:3] == ("api", "--method", "DELETE"), fake_gh.Stop())
        second = self.bundle(2)
        with self.assertRaises(fake_gh.Stop):
            self.release(fake, "--bundle", "bundle.json")
        self.assertEqual(fake.files(self.TAG)[name + ".pending"], second)
        third = self.bundle(3)
        self.assertEqual(self.release(fake, "--bundle", "bundle.json"), 0)
        files = fake.files(self.TAG)
        self.assertEqual(files[name], third)
        self.assertFalse([n for n in files if n.endswith(".pending")])
        self.assertIs(fake.manifest(self.TAG)["certified"], True)

    def test_a_pending_file_this_job_did_not_leave_stops_it(self):
        # This job writes its manifest in one upload, onto a release that
        # has none, so it never leaves one half written: not even a pending
        # manifest that says what this run would write is put in place. And
        # it never replaces the lab package or Microsoft's zip, so it leaves
        # no pending copy of them either.
        done = self.fake()
        self.assertEqual(self.release(done), 0)
        published = done.files(self.TAG)
        manifest = published.pop("manifest.json")
        ms_name = "StreamToSpeaker-Driver-1.1.0.209-Microsoft.zip"
        self.assertEqual(published[ms_name], self.ms)
        for name, data in (("manifest.json", manifest), ("notes.txt", b"placed by hand"),
                           (self.LAB, b"another lab package"), (ms_name, b"another zip")):
            with self.subTest(name):
                fake = fake_gh.FakeReleases(self.REPO)
                fake.add_release(self.TAG, {**published, name + ".pending": data}, target=self.COMMIT)
                before = fake.files(self.TAG)
                self.assertEqual(self.release(fake), 1)
                self.assertEqual(fake.files(self.TAG), before)
                self.assertEqual(fake.writes(), [])

    def test_an_attestation_submission_is_not_turned_into_a_certified_release(self):
        fake = self.fake(**{"manifest.json": {"schema": 1, "driver_version": "1.1.0.209", "attested": False,
                                              "submission_cab": "StreamToSpeaker-Driver-1.1.0.209-Submission.cab",
                                              "submission_cab_signed_sha256": "ab" * 32,
                                              "ms_product_id": "7", "ms_submission_id": "8",
                                              "ms_state": "committed"}})
        before = fake.files(self.TAG)
        self.assertNotEqual(self.release(fake), 0)
        self.assertEqual(fake.files(self.TAG), before)


@unittest.skipUnless(HAVE_REAL, "real 1.1.0.209 packages not on this machine")
class RealPackages(unittest.TestCase):
    def setUp(self):
        with open(MS_ZIP, "rb") as f:
            self.ms = f.read()
        with open(LAB_ZIP, "rb") as f:
            self.lab = f.read()

    def test_microsoft_return_is_the_lab_package(self):
        self.assertEqual(certified.verify_packages(self.ms, self.lab, "1.1.0.209"), [])

    def test_sys_bytes_differ_only_by_signature(self):
        ms, lab = certified.read_package(self.ms), certified.read_package(self.lab)
        self.assertNotEqual(ms["streamtospeaker.sys"], lab["streamtospeaker.sys"])
        self.assertEqual(certified.pe_strip_signature(ms["streamtospeaker.sys"]),
                         certified.pe_strip_signature(lab["streamtospeaker.sys"]))
        # Primary signer stays the lab test cert; Microsoft's is nested.
        self.assertIn(b"StreamToSpeaker HLK Test", certified.pe_certificate_table(ms["streamtospeaker.sys"]))
        self.assertNotIn(certified.MS_PUBLISHER, certified.pe_certificate_table(lab["streamtospeaker.sys"]))

    def test_canonical_zip_round_trips(self):
        canon = certified.canonical_zip(self.ms)
        self.assertEqual(certified.verify_packages(canon, self.lab, "1.1.0.209"), [])


class FakeApi:
    def __init__(self, results):
        self.results = list(results)
        self.downloads = []

    def get_submission(self, pid, sid):
        return self.results.pop(0)

    def download(self, url):
        self.downloads.append(url)
        return b"ZIP"


DONE = {"workflowStatus": {"currentStep": "finalizeIngestion", "state": "completed"},
        "downloads": {"items": [{"type": "initialPackage", "url": "https://sas/init"},
                                {"type": "derivedPackage", "url": "https://sas/derived"},
                                {"type": "signedPackage", "url": "https://sas/signed"},
                                {"type": "driverMetadata", "url": "https://sas/meta"},
                                {"type": "certificationReport", "url": "https://dash/report/95342530/p/s"}]}}
PENDING = {"workflowStatus": {"currentStep": "sign", "state": "started"},
           "downloads": {"items": [{"type": "initialPackage", "url": "https://sas/init"}]}}


class FetchSignedPackage(unittest.TestCase):
    def test_returns_signed_package_when_completed(self):
        api = FakeApi([PENDING, PENDING, DONE])
        data, sub = certified.fetch_signed_package(api, "p", "s", now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(data, b"ZIP")
        self.assertEqual(api.downloads, ["https://sas/signed"])
        self.assertEqual(sub["workflowStatus"]["state"], "completed")

    def test_completed_without_signed_package_keeps_waiting(self):
        no_zip = {"workflowStatus": {"currentStep": "finalizeIngestion", "state": "completed"},
                  "downloads": {"items": [{"type": "initialPackage", "url": "https://sas/init"}]}}
        api = FakeApi([no_zip, DONE])
        data, _ = certified.fetch_signed_package(api, "p", "s", now=lambda: 0, sleep=lambda s: None)
        self.assertEqual(data, b"ZIP")

    def test_failure_raises(self):
        api = FakeApi([{"workflowStatus": {"currentStep": "driverValidation", "state": "failed",
                                           "messages": ["bad"]}}])
        with self.assertRaises(RuntimeError):
            certified.fetch_signed_package(api, "p", "s", now=lambda: 0, sleep=lambda s: None)

    def test_deadline(self):
        api = FakeApi([PENDING] * 100)
        clock = itertools.count(0, 600)
        with self.assertRaises(RuntimeError):
            certified.fetch_signed_package(api, "p", "s", now=lambda: next(clock),
                                           sleep=lambda s: None, deadline_s=1200)


class Manifest(unittest.TestCase):
    def test_fields(self):
        m = certified.build_manifest("1.1.0.209", "abc", "5b" * 32, 14599256519720956, 1152921505701928485,
                                     ["WINDOWS_v100_X64_25H2_FULL"], 95342530, b"ms", b"lab", b"signed")
        self.assertTrue(m["certified"])
        self.assertFalse(m["attested"])
        self.assertEqual(m["driver_build"], 209)
        self.assertEqual(m["driver_prefix"], "1.1.0")
        self.assertEqual(m["certification"], {"product_id": "14599256519720956",
                                              "submission_id": "1152921505701928485",
                                              "os": ["WINDOWS_v100_X64_25H2_FULL"], "report_id": 95342530})
        self.assertEqual(m["signed_zip"], "StreamToSpeaker-Driver-1.1.0.209-Signed.zip")
        self.assertEqual(m["signed_zip_sha256"], hashlib.sha256(b"signed").hexdigest())
        self.assertEqual(m["lab_zip"], "StreamToSpeaker-Driver-1.1.0.209-lab-testsigned.zip")
        self.assertEqual(m["ms_zip_asset"], "StreamToSpeaker-Driver-1.1.0.209-Microsoft.zip")
        self.assertEqual(m["ms_state"], "done")
        self.assertIsNone(m["submission_cab"])
        json.dumps(m)  # serialisable

    def test_attest_treats_certified_as_done(self):
        import attest
        m = certified.build_manifest("1.1.0.209", "abc", "5b" * 32, 1, 2, [], None, b"", b"", b"")
        self.assertEqual(attest.next_action(m), "done")

    def test_notes_mention_certification(self):
        m = certified.build_manifest("1.1.0.209", "abc", "5b" * 32, 1, 2, ["WINDOWS_v100_X64_25H2_FULL"],
                                     95342530, b"", b"", b"")
        notes = certified.release_notes(m)
        self.assertIn("WHQL-certified", notes)
        self.assertIn("DownloadCertificationReport/95342530/1/2", notes)
        self.assertIn("new HLK run", notes)
        self.assertNotIn("Windows 10", notes)
        self.assertIn("Windows 11, version 25H2 (x64) (`WINDOWS_v100_X64_25H2_FULL`)", notes)

    def test_notes_without_report_id(self):
        m = certified.build_manifest("1.1.0.209", "abc", "5b" * 32, 1, 2, [], None, b"", b"", b"")
        self.assertIn("| Certification report | n/a |", certified.release_notes(m))
        m = certified.build_manifest("1.1.0.209", "abc", "5b" * 32, "", "", [], 95342530, b"", b"", b"")
        self.assertIn("| Certification report | n/a |", certified.release_notes(m))



class DispatchInputs(unittest.TestCase):
    SHA = "0123456789abcdef0123456789abcdef01234567"

    def env(self, **kw):
        base = {"INPUT_VERSION": "1.1.1.211", "INPUT_OS": "WINDOWS_v100_X64_25H2_FULL",
                "INPUT_DRY_RUN": "false", "GITHUB_SHA": self.SHA,
                "INPUT_PRODUCT": "13928634707663450", "INPUT_SUBMISSION": "1152921505702104817"}
        base.update(kw)
        return base

    def test_hardware_api_run_from_the_dispatched_commit(self):
        values, problems = certified.check_inputs(self.env())
        self.assertEqual(problems, [])
        self.assertEqual(values, {
            "version": "1.1.1.211", "tag": "driver-v1.1.1.211", "commit": self.SHA, "mode": "api",
            "product_id": "13928634707663450", "submission_id": "1152921505702104817", "report_id": "",
            "ms_zip": "StreamToSpeaker-Driver-1.1.1.211-Microsoft.zip",
            "lab_zip": "StreamToSpeaker-Driver-1.1.1.211-lab-testsigned.zip",
            "os": "WINDOWS_v100_X64_25H2_FULL", "dry_run": "false"})

    def test_hand_downloaded_zip_for_another_commit(self):
        other = "f" * 40
        values, problems = certified.check_inputs(self.env(
            INPUT_PRODUCT="", INPUT_SUBMISSION="", INPUT_MS_ZIP="signed-whcp.zip", INPUT_COMMIT=other,
            INPUT_TAG="driver-v1.1.1.211", INPUT_REPORT="95342530", INPUT_DRY_RUN="true"))
        self.assertEqual(problems, [])
        self.assertEqual((values["mode"], values["ms_zip"], values["commit"], values["report_id"], values["dry_run"]),
                         ("asset", "signed-whcp.zip", other, "95342530", "true"))

    def test_spaces_around_a_pasted_value_are_dropped(self):
        values, problems = certified.check_inputs(self.env(
            INPUT_COMMIT=self.SHA + " ", INPUT_PRODUCT=" 13928634707663450\t", INPUT_OS="A,\tB"))
        self.assertEqual(problems, [])
        self.assertEqual((values["commit"], values["product_id"], values["os"]),
                         (self.SHA, "13928634707663450", "A,B"))

    def test_spaces_around_the_version_are_refused(self):
        # The version also names the run's concurrency group, which takes it
        # as typed: "1.1.1.211 " would run beside a run of "1.1.1.211".
        for version in (" 1.1.1.211", "1.1.1.211\t"):
            _, problems = certified.check_inputs(self.env(INPUT_VERSION=version))
            self.assertEqual(len(problems), 1, version)

    def test_signature_list_is_normalised(self):
        values, problems = certified.check_inputs(self.env(INPUT_OS=" WINDOWS_v100_X64_25H2_FULL , B_2 ,"))
        self.assertEqual((problems, values["os"]), ([], "WINDOWS_v100_X64_25H2_FULL,B_2"))

    def test_a_dry_run_takes_no_partner_center_credential(self):
        # A dry run must not hold the Partner Center secret: it checks a zip
        # already attached to the release.
        values, problems = certified.check_inputs(self.env(INPUT_PRODUCT="1", INPUT_SUBMISSION="2",
                                                           INPUT_DRY_RUN="true"))
        self.assertTrue(any("ms_zip_asset" in p and "dry run" in p for p in problems), problems)
        values, problems = certified.check_inputs(self.env(INPUT_MS_ZIP="m.zip", INPUT_DRY_RUN="true"))
        self.assertEqual(problems, [])

    def test_needs_both_ids_or_an_asset(self):
        for kw in ({"INPUT_PRODUCT": ""}, {"INPUT_SUBMISSION": ""}):
            _, problems = certified.check_inputs(self.env(**kw))
            self.assertEqual(len(problems), 1, kw)

    def test_values_that_could_change_a_script_are_refused(self):
        for key, bad in [
            ("INPUT_VERSION", "1.1.1.211\n"), ("INPUT_VERSION", "1.1.1.٢١١"), ("INPUT_VERSION", ""),
            ("INPUT_TAG", "v1.0.0"), ("INPUT_TAG", "driver-v1.1.1.211; id"),
            ("INPUT_PRODUCT", "1; id"), ("INPUT_SUBMISSION", "$(id)"), ("INPUT_REPORT", "1\n2"),
            ("INPUT_MS_ZIP", "a.zip\nx=y"), ("INPUT_MS_ZIP", "a b.zip"), ("INPUT_LAB_ZIP", "../lab.zip"),
            ("INPUT_LAB_ZIP", "lab.tar"), ("INPUT_OS", "A;B"), ("INPUT_OS", " , "), ("INPUT_OS", "A\n"),
            ("INPUT_COMMIT", "HEAD"), ("INPUT_COMMIT", self.SHA.upper()), ("INPUT_COMMIT", self.SHA[:7]),
            ("INPUT_DRY_RUN", "yes"), ("INPUT_DRY_RUN", ""),
            ("INPUT_LAB_ZIP", "StreamToSpeaker-Driver-1.1.1.211-Microsoft.zip"),
            ("INPUT_LAB_ZIP", "StreamToSpeaker-Driver-1.1.1.211-Signed.zip"),
            ("INPUT_MS_ZIP", "StreamToSpeaker-Driver-1.1.1.211-Signed.zip"),
            ("INPUT_MS_ZIP", "StreamToSpeaker-Driver-1.1.1.211-lab-testsigned.zip"),
            ("INPUT_LAB_ZIP", "STREAMTOSPEAKER-DRIVER-1.1.1.211-MICROSOFT.zip"),
        ]:
            _, problems = certified.check_inputs(self.env(**{key: bad}))
            self.assertEqual(len(problems), 1, (key, bad, problems))

    def test_command_writes_the_values_to_github_output(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "output")
            env = dict(self.env(), GITHUB_OUTPUT=out)
            with unittest.mock.patch.dict(os.environ, env, clear=True), \
                    unittest.mock.patch("sys.stdout", io.StringIO()):
                self.assertEqual(certified.main(["inputs"]), 0)
            with open(out, encoding="utf-8") as f:
                written = dict(line.split("=", 1) for line in f.read().splitlines())
        self.assertEqual(written, certified.check_inputs(self.env())[0])

    def test_command_fails_without_writing(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "output")
            env = dict(self.env(INPUT_COMMIT="HEAD"), GITHUB_OUTPUT=out)
            with unittest.mock.patch.dict(os.environ, env, clear=True), \
                    unittest.mock.patch("sys.stdout", io.StringIO()) as shown:
                self.assertEqual(certified.main(["inputs"]), 1)
            self.assertFalse(os.path.exists(out))
        self.assertIn("::error::commit must be a full commit SHA", shown.getvalue())


if __name__ == "__main__":
    unittest.main()
