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
