import contextlib
import io
import json
import os
import struct
import tempfile
import unittest
import zipfile
import zlib

import repro_compare as rc
import test_certified


def rich_block(entries, key=0x1234ABCD):
    """A Rich header: DanS ^ key, 3 x key, (comp.id ^ key, count ^ key)..., Rich, key."""
    out = struct.pack("<IIII", 0x536E6144 ^ key, key, key, key)
    for product, build, count in entries:
        out += struct.pack("<II", ((product << 16) | build) ^ key, count ^ key)
    return out + b"Rich" + struct.pack("<I", key)


def make_pe(code=b"CODE" * 64, pdb_path=b"C:\\src\\driver.pdb", timestamp=0x1000, checksum=0x2000,
            guid=b"\x11" * 16, age=1, rich=None, stub_pad=0):
    """PE32+ with one section holding a CodeView debug entry, so the
    normalisation has something to mask. `rich` = [(product, build, count)]
    entries placed at 0x40; `stub_pad` shifts e_lfanew (different linkers
    pad the DOS stub differently)."""
    dos = bytearray(b"MZ" + b"\0" * 0x3E)
    if rich:
        dos += rich_block(rich)
    dos += b"\0" * stub_pad
    e_lfanew = len(dos)
    struct.pack_into("<I", dos, 0x3C, e_lfanew)
    sec_va, sec_raw = 0x1000, 0x400
    cv = b"RSDS" + guid + struct.pack("<I", age) + pdb_path + b"\0"
    dbg_entry = struct.pack("<IIHHIIII", 0, timestamp, 0, 0, 2, len(cv), sec_va + 28, sec_raw + 28)
    body = dbg_entry + cv + code
    coff = struct.pack("<HHIIIHH", 0x8664, 1, timestamp, 0, 0, 240, 0)
    opt = bytearray(240)
    struct.pack_into("<H", opt, 0, 0x20B)
    struct.pack_into("<I", opt, 64, checksum)
    struct.pack_into("<I", opt, 108, 16)                    # NumberOfRvaAndSizes
    struct.pack_into("<II", opt, 112 + 6 * 8, sec_va, 28)   # debug directory
    sec = b".rdata\0\0" + struct.pack("<IIIIIIHHI", len(body), sec_va, len(body), sec_raw, 0, 0, 0, 0, 0x40000040)
    header = bytes(dos) + b"PE\0\0" + coff + bytes(opt) + sec
    header += b"\0" * (sec_raw - len(header))
    return header + body


class SysCompare(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()

    def path(self, name, data):
        p = os.path.join(self.tmp, name)
        with open(p, "wb") as f:
            f.write(data)
        return p

    def test_identical(self):
        rep = rc.compare_sys(self.path("a", make_pe()), self.path("b", make_pe()))
        self.assertTrue(rep["raw_identical"])
        self.assertTrue(rep["normalised_identical"])
        self.assertEqual(rep["differing_regions"], [])

    def test_timestamp_checksum_pdb_identity_are_masked(self):
        a = make_pe()
        # Same-length PDB path: a longer one shifts the section layout.
        b = make_pe(timestamp=0x9999, checksum=0x7777, guid=b"\x22" * 16, age=3, pdb_path=b"D:\\xyz\\driver.pdb")
        rep = rc.compare_sys(self.path("a", a), self.path("b", b))
        self.assertFalse(rep["raw_identical"])
        self.assertTrue(rep["normalised_identical"])
        self.assertEqual(rep["released"]["debug"][0]["pdb_path"], "C:\\src\\driver.pdb")
        self.assertEqual(rep["fresh"]["debug"][0]["pdb_age"], 3)

    def test_code_change_is_reported_with_section(self):
        code = bytearray(b"CODE" * 64)
        code[10] ^= 0xFF
        rep = rc.compare_sys(self.path("a", make_pe()), self.path("b", make_pe(code=bytes(code))))
        self.assertFalse(rep["normalised_identical"])
        self.assertEqual(len(rep["differing_regions"]), 1)
        r = rep["differing_regions"][0]
        self.assertEqual(r["size"], 1)
        self.assertEqual(r["region"], ".rdata")
        cv_len = 4 + 16 + 4 + len(b"C:\\src\\driver.pdb") + 1
        self.assertEqual(r["offset"], 0x400 + 28 + cv_len + 10)

    def test_header_shift_and_rich_header_are_structural(self):
        a = make_pe(rich=[(261, 35229, 8), (258, 35229, 1)])
        b = make_pe(rich=[(261, 35228, 8), (258, 35228, 1)], stub_pad=8)
        rep = rc.compare_sys(self.path("a", a), self.path("b", b))
        self.assertFalse(rep["raw_identical"])
        self.assertTrue(rep["normalised_identical"])
        self.assertFalse(rep["toolset_identical"])
        self.assertEqual(rep["e_lfanew"], {"released": 0x40 + 16 + 16 + 8, "fresh": 0x40 + 16 + 16 + 8 + 8})
        self.assertEqual(rep["released"]["rich"]["entries"][0], {"product_id": 261, "build": 35229, "count": 8})
        same = make_pe(rich=[(261, 35229, 8), (258, 35229, 1)])
        rep = rc.compare_sys(self.path("c", a), self.path("d", same))
        self.assertTrue(rep["toolset_identical"])
        self.assertTrue(rep["raw_identical"])

    def test_signature_is_stripped_before_comparing(self):
        plain = make_pe()
        signed = bytearray(plain)
        cert = b"CERT" * 8
        e_lfanew = 0x40
        struct.pack_into("<II", signed, e_lfanew + 24 + 112 + 4 * 8, len(plain), len(cert))
        signed += cert
        rep = rc.compare_sys(self.path("a", bytes(signed)), self.path("b", plain))
        self.assertTrue(rep["raw_identical"])

    def test_length_mismatch_is_a_region(self):
        rep = rc.compare_sys(self.path("a", make_pe()), self.path("b", make_pe() + b"\0" * 16))
        self.assertFalse(rep["normalised_identical"])
        self.assertEqual(rep["differing_regions"][-1]["size"], 16)


    def test_nonzero_header_padding_is_reported(self):
        # Header slack after the section table is not compared positionally
        # (its length follows e_lfanew), so it must be checked to be zero.
        b = bytearray(make_pe())
        b[0x3F0] = 0x5A
        rep = rc.compare_sys(self.path("a", make_pe()), self.path("b", bytes(b)))
        self.assertFalse(rep["normalised_identical"])
        self.assertEqual([(r["offset"], r["size"]) for r in rep["differing_regions"]], [(0x3F0, 1)])
        self.assertIn("non-zero padding in fresh", rep["differing_regions"][0]["region"])

    def test_nonzero_stub_slack_is_reported(self):
        # The bytes between the Rich header and the PE signature.
        a = make_pe(rich=[(261, 35229, 8)], stub_pad=8)
        b = bytearray(a)
        b[0x40 + 16 + 8 + 8 + 2] = 1
        rep = rc.compare_sys(self.path("a", a), self.path("b", bytes(b)))
        self.assertFalse(rep["normalised_identical"])
        self.assertEqual(len(rep["differing_regions"]), 1)

    def test_build_constant_change_is_a_code_difference(self):
        # A source tree stamped with a different build number compiles to a
        # different immediate; masking must never hide that.
        code8 = b"\x41\xb8" + struct.pack("<I", 8) + b"CODE" * 62
        code209 = b"\x41\xb8" + struct.pack("<I", 209) + b"CODE" * 62
        rep = rc.compare_sys(self.path("a", make_pe(code=code8)), self.path("b", make_pe(code=code209, timestamp=0x5555)))
        self.assertFalse(rep["normalised_identical"])
        self.assertEqual([r["region"] for r in rep["differing_regions"]], [".rdata"])


class TreeCompare(unittest.TestCase):
    def setUp(self):
        self.src = tempfile.mkdtemp()
        self.build = tempfile.mkdtemp()
        for root in (self.src, self.build):
            os.makedirs(os.path.join(root, "driver"))
            os.makedirs(os.path.join(root, "include"))
            self.write(root, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          8u\r\n")
            self.write(root, "include/abi.h", b"struct x;\r\n")

    def write(self, root, rel, data):
        with open(os.path.join(root, rel), "wb") as f:
            f.write(data)

    def test_identical_copy(self):
        rep = rc.compare_trees(self.src, self.build)
        self.assertTrue(rep["identical"])
        self.assertEqual(rep["files"], 2)

    def test_stamped_driver_h_is_caught(self):
        # What the old stamp_driver_h option did after the source-hash guard.
        self.write(self.build, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          209u\r\n")
        rep = rc.compare_trees(self.src, self.build)
        self.assertFalse(rep["identical"])
        self.assertEqual(rep["changed"], ["driver/driver.h"])
        self.assertEqual(rc.main(["tree", self.src, self.build]), 1)

    def test_extra_or_missing_files_are_caught(self):
        self.write(self.build, "driver/extra.cpp", b"")
        os.remove(os.path.join(self.build, "include", "abi.h"))
        rep = rc.compare_trees(self.src, self.build)
        self.assertEqual(rep["only_in_build"], ["driver/extra.cpp"])
        self.assertEqual(rep["only_in_source"], ["include/abi.h"])
        self.assertFalse(rep["identical"])

    def test_empty_tree_is_not_identical(self):
        rep = rc.compare_trees(tempfile.mkdtemp(), tempfile.mkdtemp())
        self.assertFalse(rep["identical"])

    # ---- attested releases: driver.h stamped before the build ----
    def test_stamp_replaces_only_the_build_number(self):
        src = b"// \xe2\x80\x94 note\r\n#define STREAM_TO_SPEAKER_DRIVER_BUILD          8u\r\n#define OTHER 8u\r\n"
        self.assertEqual(rc.stamp_driver_h(src, 204),
                         b"// \xe2\x80\x94 note\r\n#define STREAM_TO_SPEAKER_DRIVER_BUILD          204u\r\n#define OTHER 8u\r\n")
        with self.assertRaises(ValueError):
            rc.stamp_driver_h(b"#define NOTHING 1\r\n", 204)

    def test_stamped_tree_accepted_with_stamped_build(self):
        self.write(self.build, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          204u\r\n")
        rep = rc.compare_trees(self.src, self.build, stamped_build=204)
        self.assertTrue(rep["identical"])
        self.assertTrue(rep["stamp_ok"])
        self.assertEqual(rep["changed"], ["driver/driver.h"])
        self.assertEqual(rc.main(["tree", self.src, self.build, "--stamped-build", "204"]), 0)

    def test_stamped_tree_wrong_number_or_unstamped_is_rejected(self):
        self.write(self.build, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          205u\r\n")
        self.assertFalse(rc.compare_trees(self.src, self.build, stamped_build=204)["identical"])
        # Not stamped at all: the build would not be the one being verified.
        self.write(self.build, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          8u\r\n")
        rep = rc.compare_trees(self.src, self.build, stamped_build=204)
        self.assertFalse(rep["identical"])
        self.assertIn("204", rep["stamp_problem"])
        self.assertEqual(rc.main(["tree", self.src, self.build, "--stamped-build", "204"]), 1)

    def test_stamped_tree_extra_edit_in_driver_h_is_rejected(self):
        self.write(self.build, "driver/driver.h",
                   b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          204u\r\n#define BACKDOOR 1\r\n")
        self.assertFalse(rc.compare_trees(self.src, self.build, stamped_build=204)["identical"])

    def test_stamped_tree_other_changes_are_rejected(self):
        self.write(self.build, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          204u\r\n")
        self.write(self.build, "include/abi.h", b"struct y;\r\n")
        rep = rc.compare_trees(self.src, self.build, stamped_build=204)
        self.assertFalse(rep["identical"])
        self.write(self.build, "include/abi.h", b"struct x;\r\n")
        self.write(self.build, "driver/extra.cpp", b"")
        self.assertFalse(rc.compare_trees(self.src, self.build, stamped_build=204)["identical"])

    def test_stamped_tree_line_ending_change_is_rejected(self):
        self.write(self.build, "driver/driver.h", b"#define STREAM_TO_SPEAKER_DRIVER_BUILD          204u\n")
        self.assertFalse(rc.compare_trees(self.src, self.build, stamped_build=204)["identical"])


CERTIFIED_MANIFEST = {
    "schema": 1, "driver_version": "1.1.1.211", "driver_prefix": "1.1.1", "driver_build": 211,
    "commit": "9ad59bfda059ec9e5ed2de9bb0db866e7a1d2152", "source_hash": "f4" * 32,
    "submission_cab": None, "submission_cab_sha256": None, "submission_cab_signed_sha256": None,
    "attested": False, "certified": True,
    "signed_zip": "StreamToSpeaker-Driver-1.1.1.211-Signed.zip", "signed_zip_sha256": "e7" * 32,
    "lab_zip": "StreamToSpeaker-Driver-1.1.1.211-lab-testsigned.zip", "lab_zip_sha256": "AA" * 32,
}
# Shape of the 1.1.0.204 manifest (attested before build_root was recorded).
ATTESTED_MANIFEST = {
    "schema": 1, "driver_version": "1.1.0.204", "driver_prefix": "1.1.0", "driver_build": 204,
    "commit": "cde07512d466b2e09fa278505487be98981ef723", "source_hash": "80" * 32,
    "submission_cab": "StreamToSpeaker-Driver-1.1.0.204-Submission.cab",
    "submission_cab_sha256": "36" * 32, "submission_cab_signed_sha256": "C9" * 32,
    "attested": True,
    "signed_zip": "StreamToSpeaker-Driver-1.1.0.204-Signed.zip", "signed_zip_sha256": "c2" * 32,
    "ms_zip_asset": "StreamToSpeaker-Driver-1.1.0.204-Microsoft.zip",
}
RUNNER_ROOT = "D:\\a\\StreamToSpeaker\\StreamToSpeaker"
RUNNER_PDB = RUNNER_ROOT + "\\driver\\x64\\Release\\StreamToSpeaker.pdb"


class BuildPlan(unittest.TestCase):
    def test_certified(self):
        plan = rc.build_plan(CERTIFIED_MANIFEST, "driver-v1.1.1.211")
        self.assertTrue(plan["ok"], plan["errors"])
        self.assertEqual(plan["kind"], "certified")
        self.assertFalse(plan["stamp_driver_h"])
        self.assertEqual(plan["build_number"], "211")
        self.assertEqual(plan["reference_type"], "lab_zip")
        self.assertEqual(plan["reference_asset"], "StreamToSpeaker-Driver-1.1.1.211-lab-testsigned.zip")
        self.assertEqual(plan["reference_sha256"], "aa" * 32)
        self.assertEqual(rc.resolve_build_root(plan, "whatever")["root"], "C:\\build\\src")

    def test_certified_without_lab_zip_uses_the_conventional_name(self):
        m = dict(CERTIFIED_MANIFEST, lab_zip=None, lab_zip_sha256=None)
        plan = rc.build_plan(m, "driver-v1.1.1.211")
        self.assertTrue(plan["ok"])
        self.assertEqual(plan["reference_asset"], "StreamToSpeaker-Driver-1.1.1.211-lab-testsigned.zip")
        self.assertEqual(plan["reference_sha256"], "")

    def test_attested(self):
        plan = rc.build_plan(ATTESTED_MANIFEST, "driver-v1.1.0.204")
        self.assertTrue(plan["ok"], plan["errors"])
        self.assertEqual(plan["kind"], "attested")
        self.assertTrue(plan["stamp_driver_h"])
        self.assertEqual(plan["build_number"], "204")
        self.assertEqual(plan["reference_type"], "submission_cab")
        self.assertEqual(plan["reference_asset"], "StreamToSpeaker-Driver-1.1.0.204-Submission.cab")
        # The CAB kept on the release is the EV-signed one.
        self.assertEqual(plan["reference_sha256"], "c9" * 32)

    def test_plan_carries_source_hash_and_a_printable_commit(self):
        plan = rc.build_plan(dict(ATTESTED_MANIFEST, source_hash="AB" * 32, signed_zip_sha256="C2" * 32), "t")
        self.assertEqual(plan["source_hash"], "ab" * 32)
        self.assertEqual(plan["signed_zip"], "StreamToSpeaker-Driver-1.1.0.204-Signed.zip")
        self.assertEqual(plan["signed_zip_sha256"], "c2" * 32)
        self.assertEqual(plan["commit"], "cde07512d466b2e09fa278505487be98981ef723")
        # The commit is only displayed: anything that is not a commit id is
        # dropped rather than printed.
        for bad in ("abc\n::error::x", "##[error]x", 5, None, "not-a-commit"):
            plan = rc.build_plan(dict(ATTESTED_MANIFEST, commit=bad), "t")
            self.assertTrue(plan["ok"])
            self.assertEqual(plan["commit"], "")

    def test_both_flags_set_fails(self):
        plan = rc.build_plan(dict(CERTIFIED_MANIFEST, attested=True), "t")
        self.assertFalse(plan["ok"])

    def test_values_that_could_forge_step_outputs_fail(self):
        for field, bad in (("driver_version", "1.1.0.204\n"), ("signed_zip_sha256", "c2" * 32 + "\n"),
                           ("submission_cab", "a.cab\nkind=certified"), ("signed_zip", "x.zip\nkind=certified"),
                           ("submission_cab", "..\\x.cab"), ("build_root", "D:\\a\nkind=certified\\b"),
                           ("build_root", 5), ("submission_cab", ["a.cab"])):
            plan = rc.build_plan(dict(ATTESTED_MANIFEST, **{field: bad}), "t")
            self.assertFalse(plan["ok"], (field, bad))
        self.assertFalse(rc.build_plan(dict(CERTIFIED_MANIFEST, lab_zip="a.zip\nkind=attested"), "t")["ok"])
        self.assertFalse(rc.build_plan(dict(CERTIFIED_MANIFEST, lab_zip_sha256=7), "t")["ok"])

    def test_neither_certified_nor_attested_fails(self):
        for m in (dict(ATTESTED_MANIFEST, attested=False), dict(ATTESTED_MANIFEST, attested="true"), {}):
            plan = rc.build_plan(m, "driver-v1.1.0.204")
            self.assertFalse(plan["ok"])
            self.assertTrue(any("neither certified nor attested" in e for e in plan["errors"]), plan["errors"])

    def test_missing_fields_fail_with_the_field_named(self):
        for field in ("driver_build", "driver_version", "source_hash", "signed_zip", "signed_zip_sha256",
                      "submission_cab", "submission_cab_signed_sha256"):
            for bad in (None, ""):
                plan = rc.build_plan(dict(ATTESTED_MANIFEST, **{field: bad}), "driver-v1.1.0.204")
                self.assertFalse(plan["ok"], field)
                self.assertTrue(any(field in e for e in plan["errors"]), (field, plan["errors"]))
            m = dict(ATTESTED_MANIFEST)
            del m[field]
            self.assertFalse(rc.build_plan(m, "driver-v1.1.0.204")["ok"], field)

    def test_malformed_fields_fail(self):
        self.assertFalse(rc.build_plan(dict(ATTESTED_MANIFEST, submission_cab_signed_sha256="abc"), "t")["ok"])
        self.assertFalse(rc.build_plan(dict(ATTESTED_MANIFEST, driver_build="204; rm"), "t")["ok"])
        self.assertFalse(rc.build_plan(dict(ATTESTED_MANIFEST, driver_build=205), "t")["ok"])  # version says .204
        self.assertFalse(rc.build_plan(["not", "an", "object"], "t")["ok"])

    def test_build_number_cross_check(self):
        self.assertTrue(rc.build_plan(ATTESTED_MANIFEST, "t", "204")["ok"])
        self.assertTrue(rc.build_plan(ATTESTED_MANIFEST, "t", "")["ok"])
        plan = rc.build_plan(ATTESTED_MANIFEST, "t", "209")
        self.assertFalse(plan["ok"])
        self.assertTrue(any("differs from the manifest" in e for e in plan["errors"]))

    def test_attested_manifest_saying_not_stamped_fails(self):
        self.assertFalse(rc.build_plan(dict(ATTESTED_MANIFEST, driver_h_stamped=False), "t")["ok"])
        self.assertTrue(rc.build_plan(dict(ATTESTED_MANIFEST, driver_h_stamped=True), "t")["ok"])


class BuildRoot(unittest.TestCase):
    def test_pdb_build_root(self):
        self.assertEqual(rc.pdb_build_root(RUNNER_PDB), RUNNER_ROOT)
        self.assertEqual(rc.pdb_build_root("C:\\build\\src\\driver\\x64\\Release\\StreamToSpeaker.pdb"), "C:\\build\\src")
        self.assertEqual(rc.pdb_build_root("D:\\driver\\x64\\release\\streamtospeaker.pdb"), None)  # no directory
        for bad in ("", None, "StreamToSpeaker.pdb", "D:\\a\\x\\driver\\x64\\Debug\\StreamToSpeaker.pdb",
                    "\\\\server\\share\\driver\\x64\\Release\\StreamToSpeaker.pdb",
                    "/home/x/driver/x64/Release/StreamToSpeaker.pdb",
                    "D:\\a\\x y\\..\\driver\\x64\\Release\\Other.pdb"):
            self.assertIsNone(rc.pdb_build_root(bad), bad)

    def test_pdb_path_with_control_characters_is_rejected(self):
        evil = "D:\\a\nkind=certified\nstamp_driver_h=false\nz=q\\driver\\x64\\Release\\StreamToSpeaker.pdb"
        self.assertIsNone(rc.pdb_build_root(evil))
        self.assertIsNone(rc.pdb_build_root(RUNNER_PDB + "\n"))

    def test_same_path(self):
        self.assertTrue(rc.same_path("D:/a/X/", "d:\\A\\x"))
        self.assertFalse(rc.same_path("D:\\a\\x", "D:\\a\\xy"))

    def test_attested_root_comes_from_the_submitted_binary(self):
        plan = rc.build_plan(ATTESTED_MANIFEST, "t")
        res = rc.resolve_build_root(plan, RUNNER_PDB)
        self.assertEqual((res["root"], res["error"], res["warning"]), (RUNNER_ROOT, None, None))
        self.assertIn("PDB path", res["source"])

    def test_attested_recorded_root_agreeing_with_the_binary(self):
        plan = rc.build_plan(dict(ATTESTED_MANIFEST, build_root="d:\\a\\streamtospeaker\\StreamToSpeaker\\"), "t")
        res = rc.resolve_build_root(plan, RUNNER_PDB)
        self.assertEqual((res["root"], res["error"], res["warning"]), (RUNNER_ROOT, None, None))

    def test_attested_recorded_root_disagreeing_warns_and_the_binary_decides(self):
        plan = rc.build_plan(dict(ATTESTED_MANIFEST, build_root="C:\\else\\where"), "t")
        self.assertTrue(plan["ok"], plan["errors"])
        res = rc.resolve_build_root(plan, RUNNER_PDB)
        self.assertEqual((res["root"], res["error"]), (RUNNER_ROOT, None))
        self.assertIn("C:\\else\\where", res["warning"])
        self.assertIn(RUNNER_ROOT, res["warning"])

    def test_attested_without_usable_pdb_path_uses_the_recorded_root(self):
        plan = rc.build_plan(dict(ATTESTED_MANIFEST, build_root=RUNNER_ROOT), "t")
        for pdb in ("", "StreamToSpeaker.pdb", "D:\\x\\y\\StreamToSpeaker.pdb"):
            res = rc.resolve_build_root(plan, pdb)
            self.assertEqual((res["root"], res["error"]), (RUNNER_ROOT, None), pdb)
            self.assertIn("manifest build_root", res["source"])

    def test_attested_without_usable_pdb_path_or_recorded_root_fails(self):
        plan = rc.build_plan(ATTESTED_MANIFEST, "t")
        for pdb in ("", "StreamToSpeaker.pdb", "D:\\x\\y\\StreamToSpeaker.pdb"):
            res = rc.resolve_build_root(plan, pdb)
            self.assertIsNone(res["root"])
            self.assertIn("cannot tell where the driver was built", res["error"])

    def test_override_wins(self):
        plan = rc.build_plan(ATTESTED_MANIFEST, "t")
        self.assertEqual(rc.resolve_build_root(plan, RUNNER_PDB, "E:\\x\\y")["root"], "E:\\x\\y")
        self.assertEqual(rc.resolve_build_root(plan, "", "E:\\x\\y")["root"], "E:\\x\\y")
        plan = rc.build_plan(CERTIFIED_MANIFEST, "t")
        self.assertEqual(rc.resolve_build_root(plan, "", "E:\\x\\y")["root"], "E:\\x\\y")

    def test_certified_root_is_fixed(self):
        # build_root is an attestation field; a lab build is always
        # rebuilt at the fixed directory, whatever else is in the manifest
        # or the lab binary.
        for m in (CERTIFIED_MANIFEST, dict(CERTIFIED_MANIFEST, build_root="C:\\lab\\src"),
                  dict(CERTIFIED_MANIFEST, build_root="not a path")):
            plan = rc.build_plan(m, "t")
            self.assertTrue(plan["ok"], plan["errors"])
            res = rc.resolve_build_root(plan, RUNNER_PDB)
            self.assertEqual((res["root"], res["warning"]), ("C:\\build\\src", None))

    def test_valid_build_root(self):
        for good in ("C:\\build\\src", RUNNER_ROOT, RUNNER_ROOT + "\\", "D:\\a b\\c d", "c:\\x\\y.z\\w"):
            self.assertTrue(rc.valid_build_root(good), good)
        for bad in ("C:\\", "C:", "C:\\Windows", "D:\\a", "D:\\a\\", "relative\\dir", "\\\\server\\share\\a\\b",
                    "D:/a/b", "D:\\a\\\\b", "C:\\a\\..", "D:\\a\\x\\..\\y", "D:\\a\\.\\b", "D:\\a\\b.", "D:\\a\\b ",
                    "D:\\a\\ b", "D:\\a\\b[c]", "D:\\a\\b*", "D:\\a\\b?", "D:\\a\\b\nx", "D:\\a\\b\x7f", "D:\\a\\b:c",
                    "", None, 5, ["D:\\a\\b"]):
            self.assertFalse(rc.valid_build_root(bad), repr(bad))

    def test_recorded_root_trailing_backslash_is_dropped(self):
        plan = rc.build_plan(dict(ATTESTED_MANIFEST, build_root=RUNNER_ROOT + "\\"), "t")
        self.assertEqual(rc.resolve_build_root(plan, "")["root"], RUNNER_ROOT)

    def test_shallow_or_relative_roots_are_refused(self):
        # The build step empties the directory: never a drive root or a
        # top-level directory, wherever the path came from.
        attested = rc.build_plan(ATTESTED_MANIFEST, "t")
        for shallow in ("C:\\", "C:\\Windows", "D:\\a", "relative\\dir", "\\\\server\\share\\a\\b", "D:\\a\\b\nx",
                        "D:\\a\\x\\..", "D:\\a\\StreamToSpeake[r]"):
            res = rc.resolve_build_root(attested, RUNNER_PDB, shallow)
            self.assertIsNone(res["root"], shallow)
            self.assertIn("refusing to build there", res["error"])
        # ... also when the path comes out of the submitted binary:
        for pdb in ("D:\\a\\driver\\x64\\Release\\StreamToSpeaker.pdb",
                    "D:\\a\\x\\..\\driver\\x64\\Release\\StreamToSpeaker.pdb",
                    "D:\\a\\x\\y.\\driver\\x64\\Release\\StreamToSpeaker.pdb"):
            res = rc.resolve_build_root(attested, pdb)
            self.assertIsNone(res["root"], pdb)
            self.assertIn("refusing to build there", res["error"])

    def test_attested_manifest_with_an_unusable_build_root_fails(self):
        for bad in ("D:\\a", "D:\\a\\x\\..", "relative\\dir", "D:\\a\\b[c]"):
            plan = rc.build_plan(dict(ATTESTED_MANIFEST, build_root=bad), "t")
            self.assertFalse(plan["ok"], bad)
            self.assertTrue(any("build_root" in e for e in plan["errors"]))

    def test_unknown_kind_has_no_root(self):
        res = rc.resolve_build_root({"kind": ""}, RUNNER_PDB)
        self.assertIsNone(res["root"])
        self.assertIn("no build plan", res["error"])


class PlanCli(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.out = os.path.join(self.tmp, "gh_output")
        self._old = os.environ.get("GITHUB_OUTPUT")
        os.environ["GITHUB_OUTPUT"] = self.out

    def tearDown(self):
        if self._old is None:
            os.environ.pop("GITHUB_OUTPUT", None)
        else:
            os.environ["GITHUB_OUTPUT"] = self._old

    def manifest(self, m):
        p = os.path.join(self.tmp, "manifest.json")
        with open(p, "w", encoding="ascii") as f:
            json.dump(m, f)
        return p

    def outputs(self):
        if not os.path.exists(self.out):
            return {}
        with open(self.out, encoding="utf-8") as f:
            return dict(line.rstrip("\n").split("=", 1) for line in f if "=" in line)

    def test_plan_and_buildroot_for_an_attested_release(self):
        plan_json = os.path.join(self.tmp, "plan.json")
        self.assertEqual(rc.main(["plan", self.manifest(ATTESTED_MANIFEST), "driver-v1.1.0.204", "--json", plan_json]), 0)
        out = self.outputs()
        self.assertEqual(out["kind"], "attested")
        self.assertEqual(out["stamp_driver_h"], "true")
        self.assertEqual(out["source_hash"], "80" * 32)
        self.assertEqual(out["signed_zip"], "StreamToSpeaker-Driver-1.1.0.204-Signed.zip")
        self.assertEqual(out["signed_zip_sha256"], "c2" * 32)
        self.assertEqual(out["build_number"], "204")
        self.assertEqual(out["version"], "1.1.0.204")
        self.assertEqual(out["reference_asset"], "StreamToSpeaker-Driver-1.1.0.204-Submission.cab")
        sys_path = os.path.join(self.tmp, "ref.sys")
        with open(sys_path, "wb") as f:
            f.write(make_pe(pdb_path=RUNNER_PDB.encode("ascii")))
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 0)
        self.assertEqual(self.outputs()["build_root"], RUNNER_ROOT)

    def test_buildroot_fails_when_the_submitted_binary_has_no_usable_pdb_path(self):
        plan_json = os.path.join(self.tmp, "plan.json")
        self.assertEqual(rc.main(["plan", self.manifest(ATTESTED_MANIFEST), "driver-v1.1.0.204", "--json", plan_json]), 0)
        sys_path = os.path.join(self.tmp, "ref.sys")
        with open(sys_path, "wb") as f:
            f.write(make_pe())  # C:\src\driver.pdb
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 1)
        self.assertNotIn("build_root", self.outputs())
        with open(sys_path, "wb") as f:
            f.write(b"not a PE")
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 1)

    def test_buildroot_with_a_recorded_root(self):
        plan_json = os.path.join(self.tmp, "plan.json")
        sys_path = os.path.join(self.tmp, "ref.sys")
        # PDB path of an unexpected form: the manifest's build_root is used.
        self.assertEqual(rc.main(["plan", self.manifest(dict(ATTESTED_MANIFEST, build_root=RUNNER_ROOT,
                                                             driver_h_stamped=True)),
                                  "driver-v1.1.0.204", "--json", plan_json]), 0)
        with open(sys_path, "wb") as f:
            f.write(make_pe())  # C:\src\driver.pdb
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 0)
        self.assertEqual(self.outputs()["build_root"], RUNNER_ROOT)
        # A recorded root that disagrees with the binary: warned about, the
        # binary decides.
        os.remove(self.out)
        self.assertEqual(rc.main(["plan", self.manifest(dict(ATTESTED_MANIFEST, build_root="C:\\else\\where")),
                                  "driver-v1.1.0.204", "--json", plan_json]), 0)
        with open(sys_path, "wb") as f:
            f.write(make_pe(pdb_path=RUNNER_PDB.encode("ascii")))
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 0)
        self.assertIn("::warning::the manifest records build_root C:\\else\\where", buf.getvalue())
        self.assertEqual(self.outputs()["build_root"], RUNNER_ROOT)

    def test_gh_output_refuses_multi_line_values(self):
        with self.assertRaises(ValueError):
            rc.gh_output(build_root="D:\\a\nkind=certified")
        self.assertEqual(self.outputs(), {})

    def test_buildroot_override_and_shallow_roots(self):
        plan_json = os.path.join(self.tmp, "plan.json")
        self.assertEqual(rc.main(["plan", self.manifest(ATTESTED_MANIFEST), "driver-v1.1.0.204", "--json", plan_json]), 0)
        sys_path = os.path.join(self.tmp, "ref.sys")
        with open(sys_path, "wb") as f:
            f.write(make_pe(pdb_path=RUNNER_PDB.encode("ascii")))
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path, "--build-root", "E:\\x\\y"]), 0)
        self.assertEqual(self.outputs()["build_root"], "E:\\x\\y")
        # The build step empties the directory: never a drive root or a
        # top-level directory, whoever asks.
        for shallow in ("C:\\", "C:\\Windows", "D:\\a", "relative\\dir"):
            self.assertEqual(rc.main(["buildroot", plan_json, sys_path, "--build-root", shallow]), 1, shallow)
        with open(sys_path, "wb") as f:
            f.write(make_pe(pdb_path=b"D:\\a\\driver\\x64\\Release\\StreamToSpeaker.pdb"))
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 1)

    def test_plan_failure_sets_no_outputs(self):
        self.assertEqual(rc.main(["plan", self.manifest(dict(ATTESTED_MANIFEST, attested=False)), "driver-v1.1.0.204"]), 1)
        self.assertEqual(self.outputs(), {})
        self.assertEqual(rc.main(["plan", os.path.join(self.tmp, "missing.json"), "t"]), 1)
        bad = os.path.join(self.tmp, "bad.json")
        with open(bad, "w") as f:
            f.write("{nope")
        self.assertEqual(rc.main(["plan", bad, "t"]), 1)

    def test_certified_plan_cli(self):
        plan_json = os.path.join(self.tmp, "plan.json")
        self.assertEqual(rc.main(["plan", self.manifest(CERTIFIED_MANIFEST), "driver-v1.1.1.211",
                                  "--build-number", "211", "--json", plan_json]), 0)
        out = self.outputs()
        self.assertEqual((out["kind"], out["stamp_driver_h"]), ("certified", "false"))
        sys_path = os.path.join(self.tmp, "ref.sys")
        with open(sys_path, "wb") as f:
            f.write(make_pe())
        self.assertEqual(rc.main(["buildroot", plan_json, sys_path]), 0)
        self.assertEqual(self.outputs()["build_root"], "C:\\build\\src")


class InfCompare(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()

    def path(self, name, text, encoding="utf-8"):
        p = os.path.join(self.tmp, name)
        with open(p, "wb") as f:
            f.write(text.encode(encoding))
        return p

    def test_date_masked_version_kept(self):
        a = self.path("a", "[Version]\r\nDriverVer = 09/18/2026,1.1.0.209\r\n")
        b = self.path("b", "[Version]\r\nDriverVer = 10/02/2026,1.1.0.209\r\n")
        rep = rc.compare_inf(a, b)
        self.assertFalse(rep["raw_identical"])
        self.assertTrue(rep["normalised_identical"])
        c = self.path("c", "[Version]\r\nDriverVer = 10/02/2026,1.1.0.210\r\n")
        rep = rc.compare_inf(a, c)
        self.assertFalse(rep["normalised_identical"])
        self.assertEqual(rep["differing_lines"][0]["line"], 2)

    def test_utf16_bom(self):
        a = self.path("a", "\ufeff[Version]\r\nDriverVer = 09/18/2026,1.1.0.209\r\n", "utf-16-le")
        b = self.path("b", "\ufeff[Version]\r\nDriverVer = 09/19/2026,1.1.0.209\r\n", "utf-16-le")
        rep = rc.compare_inf(a, b)
        self.assertEqual(rep["released"]["encoding"], "utf-16-le")
        self.assertTrue(rep["normalised_identical"])


def make_zip(path, members):
    with zipfile.ZipFile(path, "w") as z:
        for name, data in members.items():
            z.writestr(name, data)
    with open(path, "rb") as fh:
        return rc.sha256(fh.read())


class PackageCheck(unittest.TestCase):
    MEMBERS = {"drivers/x/StreamToSpeaker.sys": b"SYS", "drivers/x/StreamToSpeaker.inf": b"INF",
               "drivers/x/streamtospeaker.cat": b"CAT", "drivers/x/readme.txt": b"-"}

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.zip = os.path.join(self.tmp, "pkg.zip")
        self.out = os.path.join(self.tmp, "out")

    def test_good_package_is_extracted_flat_with_canonical_names(self):
        digest = make_zip(self.zip, self.MEMBERS)
        rep = rc.check_package(self.zip, digest.upper(), self.out)
        self.assertTrue(rep["ok"])
        self.assertEqual(sorted(os.listdir(self.out)),
                         ["StreamToSpeaker.cat", "StreamToSpeaker.inf", "StreamToSpeaker.sys"])
        self.assertEqual(rep["files"]["StreamToSpeaker.cat"]["sha256"], rc.sha256(b"CAT"))
        self.assertEqual(rep["files"]["StreamToSpeaker.cat"]["member"], "drivers/x/streamtospeaker.cat")

    def test_hash_mismatch_fails_and_extracts_nothing(self):
        make_zip(self.zip, self.MEMBERS)
        rep = rc.check_package(self.zip, "0" * 64, self.out)
        self.assertFalse(rep["sha256_ok"])
        self.assertFalse(rep["ok"])
        self.assertFalse(os.path.exists(self.out))
        self.assertEqual(rc.main(["package", self.zip, self.out, "--sha256", "0" * 64]), 1)

    def test_missing_expected_hash_fails(self):
        make_zip(self.zip, self.MEMBERS)
        self.assertFalse(rc.check_package(self.zip, "", self.out)["ok"])

    def test_missing_file_fails(self):
        members = dict(self.MEMBERS)
        del members["drivers/x/streamtospeaker.cat"]
        digest = make_zip(self.zip, members)
        rep = rc.check_package(self.zip, digest, self.out)
        self.assertEqual(rep["missing"], ["StreamToSpeaker.cat"])
        self.assertFalse(rep["ok"])

    def test_duplicate_file_fails(self):
        members = dict(self.MEMBERS, **{"other/StreamToSpeaker.sys": b"EVIL"})
        digest = make_zip(self.zip, members)
        rep = rc.check_package(self.zip, digest, self.out)
        self.assertEqual(rep["duplicates"], ["other/StreamToSpeaker.sys"])
        self.assertFalse(rep["ok"])

    def test_cli_success(self):
        digest = make_zip(self.zip, self.MEMBERS)
        self.assertEqual(rc.main(["package", self.zip, self.out, "--sha256", digest]), 0)


class FilesCompare(unittest.TestCase):
    def setUp(self):
        self.a, self.b = tempfile.mkdtemp(), tempfile.mkdtemp()
        for d in (self.a, self.b):
            for name, data in (("StreamToSpeaker.sys", b"SYS"), ("StreamToSpeaker.inf", b"INF"),
                               ("StreamToSpeaker.cat", b"CAT")):
                with open(os.path.join(d, name), "wb") as f:
                    f.write(data)

    def test_identical(self):
        rep = rc.compare_files(self.a, self.b)
        self.assertTrue(rep["identical"])
        self.assertEqual(rc.main(["files", self.a, self.b]), 0)

    def test_name_case_does_not_matter(self):
        os.rename(os.path.join(self.b, "StreamToSpeaker.cat"), os.path.join(self.b, "streamtospeaker.cat"))
        self.assertTrue(rc.compare_files(self.a, self.b)["identical"])

    def test_different_bytes(self):
        with open(os.path.join(self.b, "StreamToSpeaker.sys"), "wb") as f:
            f.write(b"SYS2")
        rep = rc.compare_files(self.a, self.b)
        self.assertFalse(rep["identical"])
        self.assertFalse(rep["files"]["StreamToSpeaker.sys"]["identical"])
        self.assertTrue(rep["files"]["StreamToSpeaker.inf"]["identical"])
        self.assertEqual(rc.main(["files", self.a, self.b]), 1)

    def test_missing_on_both_sides_is_not_identical(self):
        for d in (self.a, self.b):
            os.remove(os.path.join(d, "StreamToSpeaker.cat"))
        rep = rc.compare_files(self.a, self.b)
        self.assertFalse(rep["files"]["StreamToSpeaker.cat"]["identical"])
        self.assertFalse(rep["identical"])


def make_cab(files, compression=1, reserve=(0, 0, 0), flags_extra=0, block=32768, folder_index=0):
    """A one-folder cabinet as makecab lays it out. files = [(name, data)];
    compression 0 = stored, 1 = MSZIP (each block one deflate stream with
    the previous block as dictionary); reserve = (header, folder, data)
    reserved-area sizes, as a signed cabinet has."""
    blob = b"".join(d for _, d in files)
    blocks, prev = [], b""
    for off in range(0, len(blob), block):
        chunk = blob[off:off + block]
        if compression == 1:
            c = (zlib.compressobj(9, zlib.DEFLATED, -15, zdict=prev) if prev
                 else zlib.compressobj(9, zlib.DEFLATED, -15))
            payload = b"CK" + c.compress(chunk) + c.flush()
        else:
            payload = chunk
        blocks.append((payload, len(chunk)))
        prev = chunk
    res_hdr, res_folder, res_data = reserve
    has_res = any(reserve)
    flags = (0x0004 if has_res else 0) | flags_extra
    header_size = 36 + (4 + res_hdr if has_res else 0)
    cffiles, uoff = b"", 0
    for name, d in files:
        cffiles += struct.pack("<IIHHHH", len(d), uoff, folder_index, 0, 0, 0x20) + name.encode("latin-1") + b"\0"
        uoff += len(d)
    coff_files = header_size + 8 + res_folder
    coff_data = coff_files + len(cffiles)
    cfdata = b"".join(struct.pack("<IHH", 0, len(payload), n) + b"\xAA" * res_data + payload
                      for payload, n in blocks)
    header = struct.pack("<4sIIIIIBBHHHHH", b"MSCF", 0, coff_data + len(cfdata), 0, coff_files, 0, 3, 1,
                         1, len(files), flags, 0x1234, 0)
    if has_res:
        header += struct.pack("<HBB", res_hdr, res_folder, res_data) + b"\xBB" * res_hdr
    folder = struct.pack("<IHH", coff_data, len(blocks), compression) + b"\xCC" * res_folder
    return header + folder + cffiles + cfdata


def varied(n, seed=1):
    """n bytes that compress, but not to nothing, and differ block to block."""
    out, x = bytearray(), seed
    while len(out) < n:
        x = (x * 1103515245 + 12345) & 0x7FFFFFFF
        out += b"line %d of the driver %d\r\n" % (x % 977, x % 13)
    return bytes(out[:n])


class CleanText(unittest.TestCase):
    def test_plain_text_is_unchanged(self):
        for t in ("C:\\src\\driver.pdb", "DriverVer = 08/05/2026,1.1.0.204", "a: b #1 [x] ~`'\"%"):
            self.assertEqual(rc.clean_text(t), t)

    def test_control_and_separator_characters_are_escaped(self):
        self.assertEqual(rc.clean_text("a\nb\r\tc\x00\x7f"), "a\\x0ab\\x0d\\x09c\\x00\\x7f")
        self.assertEqual(rc.clean_text("a\u2028b\u2029c\u202ed\u200be"), "a\\u2028b\\u2029c\\u202ed\\u200be")
        # Nothing but printable ASCII comes out, whatever the console's
        # encoding is.
        self.assertEqual(rc.clean_text("na\u00efve \ufffd \U0001f600"), "na\\xefve \\ufffd \\U0001f600")
        self.assertTrue(rc.clean_text("".join(map(chr, range(0x250)))).isascii())
        self.assertNotIn("\n", rc.clean_text("x\n::set-output name=a::b"))

    def test_workflow_command_markers_are_broken_up(self):
        for t in ("::error::x", " ::set-output name=a::b", ":::", "::::", "a::b",
                  "##[error]x", "x ##[set-output name=a]b", "###[", "####[x]"):
            c = rc.clean_text(t)
            self.assertNotIn("::", c, t)
            self.assertNotIn("##[", c, t)
        self.assertEqual(rc.clean_text("::error::x"), ":\\:error:\\:x")
        self.assertEqual(rc.clean_text("##[error]x"), "#\\#[error]x")

    def test_long_text_is_cut_and_other_types_are_shown(self):
        self.assertEqual(rc.clean_text("x" * 500), "x" * 400 + "...")
        self.assertEqual(rc.clean_text(5), "5")
        self.assertEqual(rc.clean_text(None), "None")

    def test_command_text_escapes_percent(self):
        self.assertEqual(rc.command_text("50%0A::x"), "50%250A:\\:x")


class WorkflowCommandHygiene(unittest.TestCase):
    """What the scripts print about release files must not be readable by
    the Actions runner as workflow commands."""
    EVIL = "C:\\b\\x.pdb\n::set-output name=normalised_identical::true\r\n##[error]boom"

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self._env = {k: os.environ.get(k) for k in ("GITHUB_ACTIONS", "GITHUB_OUTPUT")}
        os.environ["GITHUB_ACTIONS"] = "true"
        os.environ["GITHUB_OUTPUT"] = os.path.join(self.tmp, "gh_output")

    def tearDown(self):
        for k, v in self._env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v

    def path(self, name, data):
        p = os.path.join(self.tmp, name)
        with open(p, "wb") as f:
            f.write(data)
        return p

    def run_main(self, argv):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = rc.main(argv)
        return code, buf.getvalue().splitlines()

    def assert_quarantined(self, lines):
        """The report sits between ::stop-commands::<token> and ::<token>::,
        and nothing in between (or anywhere) could be read as a command."""
        self.assertRegex(lines[0], r"^::stop-commands::[0-9a-f]{32}$")
        token = lines[0].split("::")[2]
        self.assertEqual(lines[-1], f"::{token}::")
        for line in lines[1:-1]:
            self.assertFalse(line.lstrip().startswith("::"), line)
            self.assertNotIn("##[", line)
            self.assertNotIn(token, line)

    def test_sys_report_with_a_hostile_pdb_path_and_section_name(self):
        hostile = bytearray(make_pe(pdb_path=self.EVIL.encode("utf-8")))
        e_lfanew = struct.unpack_from("<I", hostile, 0x3C)[0]
        hostile[e_lfanew + 24 + 240:e_lfanew + 24 + 248] = b"::er\nror"     # the section's name
        a = self.path("a.sys", bytes(hostile))
        rep = rc.compare_sys(a, a)
        shown = rep["released"]["debug"][0]["pdb_path"]
        self.assertNotIn("\n", shown)
        self.assertNotIn("::", shown)
        self.assertNotIn("##[", shown)
        self.assertEqual(rep["released"]["sections"][0]["name"], ":\\:er\\x0aror")
        # ... while the build-root derivation still sees the real bytes.
        self.assertEqual(rc.reference_pdb_path(a), self.EVIL)
        code, lines = self.run_main(["sys", a, a, "--json", os.path.join(self.tmp, "r.json")])
        self.assertEqual(code, 0)
        self.assert_quarantined(lines)
        with open(os.path.join(self.tmp, "r.json"), encoding="utf-8") as f:
            self.assertNotIn("\\n::", f.read())

    def test_inf_report_with_hostile_lines(self):
        a = self.path("a.inf", b"[Version]\r\nDriverVer = 08/05/2026,1.0.0.1 ##[error]x\r\n::error::y\r\n")
        b = self.path("b.inf", b"[Version]\r\nDriverVer = 08/06/2026,1.0.0.1\r\nplain\r\n")
        rep = rc.compare_inf(a, b)
        self.assertFalse(rep["normalised_identical"])
        self.assertNotIn("##[", rep["released"]["driverver"])
        for d in rep["differing_lines"]:
            self.assertNotIn("::", d["released"])
            self.assertNotIn("##[", d["released"])
        code, lines = self.run_main(["inf", a, b])
        self.assertEqual(code, 1)
        self.assert_quarantined(lines)

    def test_package_report_with_hostile_member_names(self):
        z = os.path.join(self.tmp, "p.zip")
        with zipfile.ZipFile(z, "w") as zf:
            zf.writestr("a\n::error::x/StreamToSpeaker.sys", b"S")
            zf.writestr("##[error]y/StreamToSpeaker.inf", b"I")
            zf.writestr("StreamToSpeaker.cat", b"C")
            zf.writestr("dup\n::warning::z/streamtospeaker.CAT", b"C2")
        with open(z, "rb") as f:
            sha = rc.sha256(f.read())
        code, lines = self.run_main(["package", z, os.path.join(self.tmp, "out"), "--sha256", sha])
        self.assertEqual(code, 1)                      # the duplicate .cat
        self.assert_quarantined(lines)
        self.assertTrue(any("duplicate" in line for line in lines))

    def test_plan_errors_are_single_cleaned_command_lines(self):
        m = os.path.join(self.tmp, "manifest.json")
        with open(m, "w", encoding="utf-8") as f:
            json.dump(dict(ATTESTED_MANIFEST, signed_zip="x.zip\n::set-output name=kind::certified",
                           submission_cab="##[set-output name=kind]certified"), f)
        code, lines = self.run_main(["plan", m, "driver-v1.1.0.204"])
        self.assertEqual(code, 1)
        for line in lines:
            if line.startswith("::"):
                self.assertTrue(line.startswith("::error::the manifest of "), line)
                self.assertNotIn("::", line[len("::error::"):])
            self.assertNotIn("##[", line)
        self.assertEqual(sum(line.startswith("::error::") for line in lines), 2)

    def test_buildroot_shows_a_hostile_pdb_path_cleaned(self):
        m = os.path.join(self.tmp, "manifest.json")
        with open(m, "w", encoding="utf-8") as f:
            json.dump(ATTESTED_MANIFEST, f)
        plan_json = os.path.join(self.tmp, "plan.json")
        self.run_main(["plan", m, "driver-v1.1.0.204", "--json", plan_json])
        ref = self.path("ref.sys", make_pe(pdb_path=self.EVIL.encode("utf-8")))
        code, lines = self.run_main(["buildroot", plan_json, ref])
        self.assertEqual(code, 1)
        commands = [line for line in lines if line.lstrip().startswith("::")]
        self.assertEqual(len(commands), 1)
        self.assertTrue(commands[0].startswith("::error::the submitted .sys records the PDB path"))
        self.assertNotIn("::", commands[0][len("::error::"):])
        for line in lines:
            self.assertNotIn("##[", line)

    def test_unparsable_sys_gives_no_verdict(self):
        good = self.path("good.sys", make_pe())
        bad = self.path("bad.sys", b"MZ\n::error::not really a PE")
        code, lines = self.run_main(["sys", bad, good])
        self.assertEqual(code, 2)
        self.assert_quarantined(lines)
        self.assertFalse(os.path.exists(os.environ["GITHUB_OUTPUT"]))

    def test_no_markers_outside_actions(self):
        os.environ.pop("GITHUB_ACTIONS")
        a = self.path("a.sys", make_pe())
        _code, lines = self.run_main(["sys", a, a])
        self.assertFalse(any("stop-commands" in line for line in lines))


class ArchiveExtraction(unittest.TestCase):
    SYS = make_pe()
    INF = b"[Version]\r\nDriverVer = 08/05/2026,1.1.0.204\r\n"

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.out = os.path.join(self.tmp, "work", "out")      # two levels down: room to escape upwards
        os.makedirs(os.path.dirname(self.out))

    def write(self, name, data):
        p = os.path.join(self.tmp, name)
        with open(p, "wb") as f:
            f.write(data)
        return p

    def tree(self):
        return sorted(os.path.relpath(os.path.join(d, f), self.tmp).replace(os.sep, "/")
                      for d, _dirs, files in os.walk(self.tmp) for f in files)

    def read(self, name):
        with open(os.path.join(self.out, name), "rb") as f:
            return f.read()

    # ---- cabinet reader ----
    def test_cab_stored_and_mszip_round_trip(self):
        big = varied(100_000)                                  # four MSZIP blocks
        files = [("StreamToSpeaker\\StreamToSpeaker.cat", b"CAT"), ("StreamToSpeaker\\StreamToSpeaker.inf", self.INF),
                 ("StreamToSpeaker\\StreamToSpeaker.pdb", big), ("StreamToSpeaker\\StreamToSpeaker.sys", self.SYS)]
        for compression in (0, 1):
            for reserve in ((0, 0, 0), (20, 0, 0), (20, 3, 5)):
                cab = make_cab(files, compression=compression, reserve=reserve)
                got = {name: read() for name, _size, read in rc.cab_entries(cab)}
                self.assertEqual(got, dict(files), (compression, reserve))
        self.assertLess(len(make_cab(files, compression=1)), len(make_cab(files, compression=0)))

    def test_cab_block_needs_the_previous_block_as_dictionary(self):
        # Two blocks with the same content: the second one compresses to
        # almost nothing because it refers back into the first.
        data = varied(32768, seed=7) * 2
        cab = make_cab([("f.bin", data)], compression=1)
        (name, size, read), = rc.cab_entries(cab)
        self.assertEqual((name, size, read()), ("f.bin", len(data), data))

    def test_cab_errors(self):
        good = make_cab([("StreamToSpeaker.sys", self.SYS)], compression=1)
        with self.assertRaises(rc.ArchiveError):
            rc.cab_entries(b"PK\x03\x04 not a cabinet")
        with self.assertRaises(rc.ArchiveError):
            rc.cab_entries(good[:30])                          # header cut short
        with self.assertRaises(rc.ArchiveError):
            rc.cab_entries(make_cab([("a", b"x")], flags_extra=0x0002))    # continues in another volume
        for broken in (good[:-10],                                          # data block cut short
                       make_cab([("a", b"x" * 10)], compression=3),         # LZX
                       make_cab([("a", b"x" * 10)], folder_index=4),        # no such folder
                       good.replace(b"CK", b"XX", 1)):
            entries = rc.cab_entries(broken)
            with self.assertRaises(rc.ArchiveError):
                entries[0][2]()
        # A file whose declared size runs past the folder's data.
        cab = bytearray(make_cab([("a", b"x" * 10)], compression=0))
        coff_files = struct.unpack_from("<I", cab, 16)[0]
        struct.pack_into("<I", cab, coff_files, 5000)
        with self.assertRaises(rc.ArchiveError):
            rc.cab_entries(bytes(cab))[0][2]()

    # ---- reference package ----
    def test_reference_from_a_submission_cab(self):
        cab = self.write("sub.cab", make_cab([("StreamToSpeaker\\StreamToSpeaker.inf", self.INF),
                                              ("StreamToSpeaker\\StreamToSpeaker.pdb", varied(70_000)),
                                              ("StreamToSpeaker\\StreamToSpeaker.sys", self.SYS)], reserve=(20, 0, 0)))
        with open(cab, "rb") as f:
            sha = rc.sha256(f.read())
        rep = rc.extract_reference(cab, "submission_cab", self.out, sha.upper())
        self.assertTrue(rep["ok"], rep)
        self.assertEqual(sorted(os.listdir(self.out)), ["StreamToSpeaker.inf", "StreamToSpeaker.sys"])
        self.assertEqual(self.read("StreamToSpeaker.sys"), self.SYS)
        self.assertEqual(self.read("StreamToSpeaker.inf"), self.INF)
        self.assertEqual(rep["files"]["StreamToSpeaker.sys"]["sha256"], rc.sha256(self.SYS))

    def test_reference_cab_needs_the_recorded_hash(self):
        cab = self.write("sub.cab", make_cab([("StreamToSpeaker.inf", self.INF), ("StreamToSpeaker.sys", self.SYS)]))
        for expected in (None, "", "00" * 32):
            rep = rc.extract_reference(cab, "submission_cab", self.out, expected)
            self.assertFalse(rep["ok"], expected)
            self.assertTrue(rep["error"])
            self.assertFalse(os.path.exists(self.out))

    def test_reference_from_a_lab_zip(self):
        z = self.write("lab.zip", b"")
        with zipfile.ZipFile(z, "w") as zf:
            zf.writestr("x64/Release/StreamToSpeaker.sys", self.SYS)
            zf.writestr("x64/Release/streamtospeaker.INF", self.INF)
            zf.writestr("x64/Release/streamtospeaker.cat", b"CAT")
        with open(z, "rb") as f:
            sha = rc.sha256(f.read())
        self.assertTrue(rc.extract_reference(z, "lab_zip", self.out, sha)["ok"])
        self.assertEqual(sorted(os.listdir(self.out)), ["StreamToSpeaker.inf", "StreamToSpeaker.sys"])
        # An older certified manifest records no hash for the lab zip.
        rep = rc.extract_reference(z, "lab_zip", os.path.join(self.tmp, "work", "out2"), None)
        self.assertTrue(rep["ok"])
        self.assertFalse(rep["sha256_ok"])
        self.assertFalse(rc.extract_reference(z, "lab_zip", os.path.join(self.tmp, "work", "out3"), "11" * 32)["ok"])

    def test_reference_missing_duplicate_and_unreadable(self):
        cab = self.write("a.cab", make_cab([("StreamToSpeaker.sys", self.SYS)]))
        with open(cab, "rb") as f:
            sha = rc.sha256(f.read())
        rep = rc.extract_reference(cab, "submission_cab", self.out, sha)
        self.assertEqual((rep["ok"], rep["missing"]), (False, ["StreamToSpeaker.inf"]))
        cab = self.write("b.cab", make_cab([("StreamToSpeaker.sys", self.SYS), ("StreamToSpeaker.inf", self.INF),
                                            ("other\\STREAMTOSPEAKER.SYS", b"second")]))
        with open(cab, "rb") as f:
            sha = rc.sha256(f.read())
        rep = rc.extract_reference(cab, "submission_cab", self.out, sha)
        self.assertEqual((rep["ok"], rep["duplicates"]), (False, ["other\\STREAMTOSPEAKER.SYS"]))
        junk = self.write("c.cab", b"MSCF" + b"\xff" * 40)
        with open(junk, "rb") as f:
            sha = rc.sha256(f.read())
        rep = rc.extract_reference(junk, "submission_cab", self.out, sha)
        self.assertFalse(rep["ok"])
        self.assertIn("could not read the archive", rep["error"])
        notzip = self.write("d.zip", b"not a zip at all")
        self.assertFalse(rc.extract_reference(notzip, "lab_zip", self.out, None)["ok"])
        self.assertFalse(rc.extract_reference(os.path.join(self.tmp, "nope.zip"), "lab_zip", self.out, None)["ok"])
        self.assertFalse(rc.extract_reference(cab, "something_else", self.out, sha)["ok"])
        self.assertFalse(os.path.exists(self.out))

    def test_entries_cannot_escape_the_output_directory(self):
        before = None
        for label, names in (
                ("dotdot", ("..\\..\\StreamToSpeaker.sys", "../../StreamToSpeaker.inf")),
                ("absolute", ("C:\\Windows\\System32\\drivers\\StreamToSpeaker.sys", "/etc/StreamToSpeaker.inf")),
                ("mixed", ("a/..\\..\\../StreamToSpeaker.sys", "\\\\server\\share\\StreamToSpeaker.inf"))):
            out = os.path.join(self.tmp, "work", "out-" + label)
            cab = self.write(label + ".cab", make_cab([(names[0], self.SYS), (names[1], self.INF)]))
            with open(cab, "rb") as f:
                sha = rc.sha256(f.read())
            self.assertTrue(rc.extract_reference(cab, "submission_cab", out, sha)["ok"], label)
            self.assertEqual(sorted(os.listdir(out)), ["StreamToSpeaker.inf", "StreamToSpeaker.sys"])
            z = self.write(label + ".zip", b"")
            with zipfile.ZipFile(z, "w") as zf:
                zf.writestr(names[0], self.SYS)
                zf.writestr(names[1], self.INF)
                zf.writestr("../../StreamToSpeaker.cat", b"CAT")
            with open(z, "rb") as f:
                zsha = rc.sha256(f.read())
            zout = os.path.join(self.tmp, "work", "zout-" + label)
            self.assertTrue(rc.check_package(z, zsha, zout)["ok"], label)
            self.assertEqual(sorted(os.listdir(zout)),
                             ["StreamToSpeaker.cat", "StreamToSpeaker.inf", "StreamToSpeaker.sys"])
            self.assertTrue(rc.extract_reference(z, "lab_zip", os.path.join(self.tmp, "work", "rout-" + label), zsha)["ok"])
        # Nothing was written anywhere but the archives themselves and the
        # output directories under work/.
        outside = [p for p in self.tree() if not p.startswith("work/") and not p.endswith((".cab", ".zip"))]
        self.assertEqual(outside, [])
        self.assertEqual(sorted(os.listdir(os.path.join(self.tmp, "work"))),
                         sorted(["out-dotdot", "out-absolute", "out-mixed", "zout-dotdot", "zout-absolute",
                                 "zout-mixed", "rout-dotdot", "rout-absolute", "rout-mixed"]))

    def test_oversized_members_are_refused(self):
        old = rc.MAX_MEMBER_BYTES
        rc.MAX_MEMBER_BYTES = 1000
        try:
            cab = self.write("big.cab", make_cab([("StreamToSpeaker.sys", varied(5000)),
                                                  ("StreamToSpeaker.inf", self.INF)]))
            with open(cab, "rb") as f:
                sha = rc.sha256(f.read())
            rep = rc.extract_reference(cab, "submission_cab", self.out, sha)
            self.assertFalse(rep["ok"])
            self.assertIn("larger than", rep["error"])
            z = self.write("big.zip", b"")
            with zipfile.ZipFile(z, "w", zipfile.ZIP_DEFLATED) as zf:
                zf.writestr("StreamToSpeaker.sys", b"\0" * 50_000)
                zf.writestr("StreamToSpeaker.inf", self.INF)
                zf.writestr("StreamToSpeaker.cat", b"C")
            with open(z, "rb") as f:
                zsha = rc.sha256(f.read())
            rep = rc.check_package(z, zsha, self.out)
            self.assertFalse(rep["ok"])
            self.assertIn("larger than", rep["error"])
        finally:
            rc.MAX_MEMBER_BYTES = old
        self.assertFalse(os.path.exists(self.out))

    def test_reference_cli(self):
        cab = self.write("sub.cab", make_cab([("d\\StreamToSpeaker.inf", self.INF), ("d\\StreamToSpeaker.sys", self.SYS)]))
        with open(cab, "rb") as f:
            sha = rc.sha256(f.read())
        report = os.path.join(self.tmp, "reference.json")
        self.assertEqual(rc.main(["reference", cab, self.out, "--type", "submission_cab", "--sha256", sha,
                                  "--json", report]), 0)
        with open(report, encoding="utf-8") as f:
            self.assertTrue(json.load(f)["ok"])
        self.assertEqual(rc.main(["reference", cab, os.path.join(self.tmp, "work", "o2"), "--type", "submission_cab"]), 1)
        self.assertEqual(rc.main(["reference", cab, os.path.join(self.tmp, "work", "o3"), "--type", "lab_zip"]), 1)


@unittest.skipUnless(test_certified.HAVE_REAL, "real 1.1.0.209 packages not on this machine")
class RealPackages(unittest.TestCase):
    def test_lab_and_microsoft_sys_identical_after_strip(self):
        tmp = tempfile.mkdtemp()
        paths = {}
        for label, zpath in (("lab", test_certified.LAB_ZIP), ("ms", test_certified.MS_ZIP)):
            with zipfile.ZipFile(zpath) as z:
                name = next(n for n in z.namelist() if n.lower().endswith("streamtospeaker.sys"))
                paths[label] = os.path.join(tmp, label + ".sys")
                with open(paths[label], "wb") as f:
                    f.write(z.read(name))
        rep = rc.compare_sys(paths["lab"], paths["ms"])
        self.assertTrue(rep["raw_identical"])
        self.assertIsNotNone(rep["released"]["rich"])
        self.assertTrue(any("pdb_path" in e for e in rep["released"]["debug"]))


if __name__ == "__main__":
    unittest.main()
