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
import zipfile

import certified

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

    def test_canonical_zip_is_flat_and_deterministic(self):
        ms, _ = fake_packages()
        z1, z2 = certified.canonical_zip(ms), certified.canonical_zip(ms)
        self.assertEqual(z1, z2)
        self.assertEqual(sorted(zipfile.ZipFile(io.BytesIO(z1)).namelist()), sorted(certified.PACKAGE_FILES))


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
        self.assertIn("95342530", notes)
        self.assertIn("new HLK run", notes)


if __name__ == "__main__":
    unittest.main()
