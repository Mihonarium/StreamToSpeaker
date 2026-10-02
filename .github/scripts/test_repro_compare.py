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
