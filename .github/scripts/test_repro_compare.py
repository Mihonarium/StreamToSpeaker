import contextlib
import io
import json
import os
import struct
import tempfile
import unittest
import zipfile

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
