#!/usr/bin/env python3
"""Compare a released driver package against a fresh build of the same source.

Used by driver-reproducibility.yml. Pure Python, no third-party modules.

  repro_compare.py sys  <released.sys> <fresh.sys>   [--json out.json]
  repro_compare.py inf  <released.inf> <fresh.inf>   [--json out.json]
  repro_compare.py tree <checkout>     <build_root>  [--stamped-build N] [--json out.json]
  repro_compare.py plan <manifest.json> <tag>        [--build-number N] [--json out.json]
  repro_compare.py buildroot <plan.json> <reference.sys> [--build-root DIR]
  repro_compare.py package <package.zip> <out_dir> --sha256 <expected> [--json out.json]
  repro_compare.py reference <archive> <out_dir> --type lab_zip|submission_cab [--sha256 <expected>]
  repro_compare.py files <dir_a> <dir_b>              [--json out.json]

`sys` strips the Authenticode certificate table from both files (what
`signtool remove /s` does; a no-op on an unsigned build), compares the raw
bytes, and — if they differ — compares again with the PE fields that are
known to vary between otherwise identical builds masked out:

  * COFF FileHeader.TimeDateStamp
  * OptionalHeader.CheckSum
  * every IMAGE_DEBUG_DIRECTORY entry's TimeDateStamp, and the payload of
    the CodeView (RSDS: PDB GUID, age, path) and REPRO entries

The comparison is structural rather than positional: the PE header block
(COFF + optional header + section table) is compared from each file's own
e_lfanew (linkers pad the DOS stub differently), then every section's raw
bytes. Everything else — code, data, relocations, imports — must match,
and the bytes no piece covers (stub slack, header padding, gaps between
sections) must be zero in both files.
The Rich header (the linker's record of which compiler/linker/library
build numbers produced each object) is decoded and compared separately as
`toolset_identical`; it is metadata, not image content, so it does not
affect `normalised_identical` but a mismatch is reported loudly. The report
lists every differing byte range with the section it falls in, and decodes
both files' headers (linker version, timestamps, PDB, Rich header).

`inf` compares the two INFs with the DriverVer *date* masked (stampinf
writes the build date); the version part must still match.

`tree` checks that driver/ and include/ under the build root are
byte-identical to the checkout the source-hash guard ran on.

`package` checks a driver package zip against the SHA-256 a release
manifest records for it, then extracts its .sys/.inf/.cat flat into
out_dir under their canonical names (any directory layout inside the zip,
names matched case-insensitively, a duplicate is an error) and records
each file's SHA-256.

`reference` does the same for the package a release was signed from: the
lab zip of a certified release or the submission CAB of an attested one
(read with the cabinet reader below, stored and MSZIP folders), extracting
its .sys/.inf. The SHA-256 is required for a submission CAB. Neither
command writes to a path taken from the archive.

`files` checks that the .sys/.inf/.cat in two directories are
byte-identical (e.g. what an installer put on disk vs the package it is
supposed to bundle).

Text that comes from a release (manifest values, archive member names, PDB
paths, section names, INF lines) is passed through clean_text before it is
printed or stored in a report, and the reports are printed with the
runner's workflow commands switched off (see "Untrusted text").

`plan` reads a driver-v* release manifest and says how that driver was
built, so the rebuild can repeat it without being told per release:

  * certified (`certified: true`, HLK lab build): reference package =
    `lab_zip`, driver.h compiled as committed, built at C:\\build\\src;
  * attested (`attested: true`, driver-submission.yml build): reference
    package = `submission_cab` (the EV-signed CAB on the release, whose
    SHA-256 is `submission_cab_signed_sha256`), driver.h stamped with
    `driver_build` exactly as driver-submission.yml does, built where that
    workflow's checkout was.

A manifest that is neither, or that lacks a field the rebuild needs, is an
error (exit 1 with the list of problems), never a pass.

`buildroot` resolves the directory to build in. The linker embeds the PDB
path, so the rebuild has to happen at the path the reference binary was
linked at. For an attested release that path is read from the reference
.sys (<root>\\driver\\x64\\Release\\StreamToSpeaker.pdb); the manifest's
`build_root` is used when the PDB path does not have that form, and a
`build_root` that disagrees with the binary is reported as a warning (the
binary decides). A certified release is built at C:\\build\\src.
`--build-root` overrides either. The result has to be a plain directory
path at least two levels below a drive root (see valid_build_root), or it
is an error.

`tree --stamped-build N` allows exactly one difference between the two
trees: driver/driver.h, which must be the checkout's file with
STREAM_TO_SPEAKER_DRIVER_BUILD set to N.

Exit status is 0 when the normalised comparison matches, 1 otherwise.
When GITHUB_OUTPUT is set, `raw_identical`, `normalised_identical` and
`toolset_identical` (true/false) are appended to it.
"""
import argparse
import contextlib
import hashlib
import json
import os
import re
import secrets
import struct
import sys
import zipfile
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from certified import PACKAGE_FILES, pe_strip_signature  # noqa: E402

IMAGE_DEBUG_TYPE = {
    0: "UNKNOWN", 1: "COFF", 2: "CODEVIEW", 3: "FPO", 4: "MISC", 5: "EXCEPTION",
    6: "FIXUP", 7: "OMAP_TO_SRC", 8: "OMAP_FROM_SRC", 9: "BORLAND", 10: "RESERVED10",
    11: "CLSID", 12: "VC_FEATURE", 13: "POGO", 14: "ILTCG", 15: "MPX", 16: "REPRO",
    17: "SPGO", 20: "EX_DLLCHARACTERISTICS",
}


# ---------------------------------------------------------------------------
# Untrusted text
# ---------------------------------------------------------------------------
# Release assets and the files inside them (manifest values, archive member
# names, PDB paths, section names, INF lines) are not trusted. The Actions
# runner reads workflow commands from the log: a line that starts with "::"
# and, in the legacy form, "##[" anywhere in a line. So such text is never
# printed or put in a report as-is.
COMMAND_COLON_RE = re.compile(r":(?=:)")
COMMAND_HASH_RE = re.compile(r"#(?=#\[)")


def clean_text(value, limit=400):
    """`value` as one line of printable ASCII that the runner cannot read
    as a workflow command: every character outside 0x20-0x7e becomes a
    \\xNN / \\uNNNN escape (so no line breaks, no look-alike or
    direction-changing characters, and nothing the console encoding could
    choke on), "::" becomes ":\\:" and "##[" becomes "#\\#[". Text longer
    than `limit` characters is cut."""
    text = value if isinstance(value, str) else str(value)
    out = []
    for ch in text[:limit]:
        cp = ord(ch)
        if 0x20 <= cp <= 0x7E:
            out.append(ch)
        else:
            out.append(f"\\x{cp:02x}" if cp < 0x100 else f"\\u{cp:04x}" if cp < 0x10000 else f"\\U{cp:08x}")
    cleaned = COMMAND_HASH_RE.sub(r"#\\", COMMAND_COLON_RE.sub(r":\\", "".join(out)))
    return cleaned + ("..." if len(text) > limit else "")


def command_text(value):
    """clean_text for the message of a workflow command this script prints
    itself (::error::, ::warning::), where "%" starts an escape."""
    return clean_text(value).replace("%", "%25")


@contextlib.contextmanager
def quarantined_output():
    """Print a report as plain log text: on an Actions runner, workflow
    commands are switched off until a token nobody could have put into a
    release asset beforehand is printed back."""
    if not os.environ.get("GITHUB_ACTIONS"):
        yield
        return
    token = secrets.token_hex(16)
    print(f"::stop-commands::{token}", flush=True)
    try:
        yield
    finally:
        print(f"::{token}::", flush=True)


# ---------------------------------------------------------------------------
# PE parsing (only what the comparison needs)
# ---------------------------------------------------------------------------
class PE:
    def __init__(self, data):
        if data[:2] != b"MZ":
            raise ValueError("not a PE file")
        self.data = data
        self.e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
        pe = self.e_lfanew
        if data[pe:pe + 4] != b"PE\0\0":
            raise ValueError("bad PE header")
        (self.machine, self.nsections, self.timestamp, _symtab, _nsyms,
         self.opt_size, self.characteristics) = struct.unpack_from("<HHIIIHH", data, pe + 4)
        self.opt = pe + 24
        self.magic = struct.unpack_from("<H", data, self.opt)[0]
        self.linker_major, self.linker_minor = struct.unpack_from("<BB", data, self.opt + 2)
        self.checksum_off = self.opt + 64
        self.checksum = struct.unpack_from("<I", data, self.checksum_off)[0]
        self.dir_off = self.opt + (112 if self.magic == 0x20B else 96)
        self.ndirs = struct.unpack_from("<I", data, self.dir_off - 4)[0]
        self.dirs = [struct.unpack_from("<II", data, self.dir_off + 8 * i) for i in range(self.ndirs)]
        sec_tab = self.opt + self.opt_size
        self.sections = []
        for i in range(self.nsections):
            off = sec_tab + 40 * i
            # A label only: the section table's raw bytes are compared as
            # part of the header block.
            name = clean_text(data[off:off + 8].rstrip(b"\0").decode("ascii", "replace"))
            vsize, va, rawsize, rawptr = struct.unpack_from("<IIII", data, off + 8)
            self.sections.append({"name": name, "va": va, "vsize": vsize,
                                  "raw": rawptr, "rawsize": rawsize, "hdr": off})
        self.sec_tab_end = sec_tab + 40 * self.nsections

    def rva_to_off(self, rva):
        for s in self.sections:
            if s["va"] <= rva < s["va"] + max(s["vsize"], s["rawsize"]):
                return rva - s["va"] + s["raw"]
        if rva < (self.sections[0]["raw"] if self.sections else len(self.data)):
            return rva  # headers are mapped 1:1
        raise ValueError(f"RVA {rva:#x} not in any section")

    def debug_entries(self):
        """IMAGE_DEBUG_DIRECTORY entries as dicts (with file offsets)."""
        if self.ndirs <= 6:
            return []
        rva, size = self.dirs[6]
        if not size:
            return []
        off = self.rva_to_off(rva)
        out = []
        for i in range(size // 28):
            e = off + 28 * i
            (chars, ts, major, minor, typ, dsize, draw_rva, draw_ptr) = struct.unpack_from("<IIHHIIII", self.data, e)
            ent = {"entry_off": e, "timestamp": ts, "type": typ,
                   "type_name": IMAGE_DEBUG_TYPE.get(typ, str(typ)),
                   "size": dsize, "raw_off": draw_ptr}
            if typ == 2 and dsize >= 24 and self.data[draw_ptr:draw_ptr + 4] == b"RSDS":
                guid = self.data[draw_ptr + 4:draw_ptr + 20]
                age = struct.unpack_from("<I", self.data, draw_ptr + 20)[0]
                path = self.data[draw_ptr + 24:draw_ptr + dsize].split(b"\0", 1)[0]
                ent["pdb_guid"] = guid.hex()
                ent["pdb_age"] = age
                ent["pdb_path"] = path.decode("utf-8", "replace")
            if typ == 16:
                ent["repro"] = self.data[draw_ptr:draw_ptr + dsize].hex()
            out.append(ent)
        return out

    def rich_header(self):
        """Decode the Rich header: list of (product_id, build, count) plus the
        checksum. Product ids identify the compiler/linker components; the
        build number is the toolset version that produced each object."""
        data = self.data[:self.e_lfanew]
        end = data.rfind(b"Rich")
        if end < 0 or end + 8 > len(data):
            return None
        key = struct.unpack_from("<I", data, end + 4)[0]
        start = None
        for i in range(end - 4, 0x40 - 4, -4):
            if struct.unpack_from("<I", data, i)[0] ^ key == 0x536E6144:  # "DanS"
                start = i
                break
        if start is None:
            return None
        entries = []
        for i in range(start + 16, end, 8):
            compid, count = struct.unpack_from("<II", data, i)
            compid ^= key
            count ^= key
            entries.append({"product_id": compid >> 16, "build": compid & 0xFFFF, "count": count})
        return {"offset": start, "end": end + 8, "key": f"{key:#010x}", "entries": entries}

    def info(self):
        return {
            "size": len(self.data),
            "machine": f"{self.machine:#06x}",
            "linker_version": f"{self.linker_major}.{self.linker_minor}",
            "coff_timestamp": f"{self.timestamp:#010x}",
            "checksum": f"{self.checksum:#010x}",
            "sections": [{"name": s["name"], "raw": s["raw"], "rawsize": s["rawsize"]} for s in self.sections],
            "debug": [{k: (clean_text(v) if k == "pdb_path" else v) for k, v in e.items()
                       if k not in ("entry_off", "raw_off")} for e in self.debug_entries()],
            "rich": self.rich_header(),
        }

    def region_of(self, off):
        if off < self.e_lfanew:
            return "DOS stub / Rich header"
        if off < self.opt:
            return "COFF header"
        if off < self.opt + self.opt_size:
            return "optional header"
        if off < self.sec_tab_end:
            return "section table"
        for s in self.sections:
            if s["raw"] <= off < s["raw"] + s["rawsize"]:
                return s["name"]
        return "outside sections / overlay"


# ---------------------------------------------------------------------------
# Normalisation
# ---------------------------------------------------------------------------
def masked_ranges(pe):
    """(offset, size, label) ranges that legitimately vary between builds."""
    ranges = [
        (0x3C, 4, "DOS e_lfanew"),  # header block is compared from each file's own offset
        (pe.e_lfanew + 8, 4, "COFF TimeDateStamp"),
        (pe.checksum_off, 4, "OptionalHeader CheckSum"),
    ]
    for e in pe.debug_entries():
        ranges.append((e["entry_off"] + 4, 4, f"debug[{e['type_name']}] TimeDateStamp"))
        if e["type"] in (2, 16) and e["size"]:
            ranges.append((e["raw_off"], e["size"], f"debug[{e['type_name']}] payload"))
    return ranges


def normalise(data):
    pe = PE(data)
    out = bytearray(data)
    for off, size, _ in masked_ranges(pe):
        out[off:off + size] = b"\0" * size
    return bytes(out)


def structure(pe, total_len):
    """Split an image into the pieces compared structurally: DOS stub up to
    the Rich header, PE header block, each section's raw bytes, and whatever
    follows the last section. Absolute (offset_start, offset_end) ranges,
    taken from the unmasked parse so masking can't disturb them."""
    rich = pe.rich_header()
    stub_end = rich["offset"] if rich else pe.e_lfanew
    parts = [("DOS stub", 0, stub_end),
             ("PE header block", pe.e_lfanew, pe.sec_tab_end)]
    last = pe.sec_tab_end
    for sec in pe.sections:
        parts.append((sec["name"], sec["raw"], sec["raw"] + sec["rawsize"]))
        last = max(last, sec["raw"] + sec["rawsize"])
    parts.append(("overlay", last, total_len))
    return parts


def uncompared_ranges(pe, total_len):
    """Byte ranges `structure()` leaves out (Rich header aside): the slack
    between the Rich header and the PE signature, the header padding after
    the section table, and any gaps between sections. Linkers fill them
    with zeros; they are not compared positionally because their length
    follows e_lfanew, but anything non-zero in them must not go unseen."""
    covered = sorted((a, b) for _, a, b in structure(pe, total_len) if b > a)
    rich = pe.rich_header()
    if rich:
        covered = sorted(covered + [(rich["offset"], rich["end"])])
    gaps, pos = [], 0
    for a, b in covered:
        if a > pos:
            gaps.append((pos, a))
        pos = max(pos, b)
    if pos < total_len:
        gaps.append((pos, total_len))
    return gaps


def nonzero_ranges(data, start, end):
    """Contiguous runs of non-zero bytes in data[start:end], absolute."""
    out = []
    i = start
    while i < end:
        if data[i] == 0:
            i += 1
            continue
        j = i
        while j < end and data[j] != 0:
            j += 1
        out.append((i, j - i))
        i = j
    return out


def diff_regions(a, b, limit=200):
    """Contiguous byte ranges where a and b differ (length mismatch counts
    as one trailing region)."""
    n = min(len(a), len(b))
    regions = []
    i = 0
    while i < n and len(regions) < limit:
        if a[i] == b[i]:
            i += 1
            continue
        j = i
        while j < n and a[j] != b[j]:
            j += 1
        regions.append((i, j - i))
        i = j
    if len(a) != len(b):
        regions.append((n, abs(len(a) - len(b))))
    return regions


def sha256(b):
    return hashlib.sha256(b).hexdigest()


def gh_output(**kv):
    path = os.environ.get("GITHUB_OUTPUT")
    if not path or not kv:
        return
    with open(path, "a", encoding="utf-8") as fh:
        for k, v in kv.items():
            # One line per value: anything else would let a value define
            # further outputs.
            if any(ord(c) < 0x20 or ord(c) == 0x7F for c in str(v)):
                raise ValueError(f"refusing to write a control character to step output {k}")
            fh.write(f"{k}={str(v).lower() if isinstance(v, bool) else v}\n")


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------
def compare_sys(released_path, fresh_path):
    with open(released_path, "rb") as fh:
        released = pe_strip_signature(fh.read())
    with open(fresh_path, "rb") as fh:
        fresh = pe_strip_signature(fh.read())

    pe_r, pe_f = PE(released), PE(fresh)
    report = {
        "released": {"path": released_path, "stripped_sha256": sha256(released), **pe_r.info()},
        "fresh": {"path": fresh_path, "stripped_sha256": sha256(fresh), **pe_f.info()},
    }
    raw_identical = released == fresh
    report["raw_identical"] = raw_identical

    norm_r, norm_f = normalise(released), normalise(fresh)
    report["masked"] = [{"offset": o, "size": s, "what": w} for o, s, w in masked_ranges(pe_r)]

    rich_r, rich_f = pe_r.rich_header(), pe_f.rich_header()
    report["toolset_identical"] = (rich_r["entries"] if rich_r else None) == (rich_f["entries"] if rich_f else None)

    # Structural comparison: piece by piece, each from its own offset, so an
    # e_lfanew shift (different DOS-stub padding) doesn't smear the whole
    # header as "different" and the Rich header is left out.
    parts_r = structure(pe_r, len(norm_r))
    parts_f = structure(pe_f, len(norm_f))
    regions = []
    same_layout = [p[0] for p in parts_r] == [p[0] for p in parts_f]
    if same_layout:
        for (name, r0, r1), (_, f0, f1) in zip(parts_r, parts_f):
            for off, size in diff_regions(norm_r[r0:r1], norm_f[f0:f1]):
                regions.append({"offset": r0 + off, "size": size,
                                "region": name if name != "PE header block" else pe_r.region_of(r0 + off)})
    else:
        # Different section list: fall back to a plain byte diff.
        for off, size in diff_regions(norm_r, norm_f):
            regions.append({"offset": off, "size": size, "region": pe_r.region_of(off)})
    # Bytes outside every compared piece must be zero padding in both files.
    for label, pe, data in (("released", pe_r, norm_r), ("fresh", pe_f, norm_f)):
        for g0, g1 in uncompared_ranges(pe, len(data)):
            for off, size in nonzero_ranges(data, g0, g1):
                regions.append({"offset": off, "size": size,
                                "region": f"non-zero padding in {label} ({pe.region_of(off)})"})
    report["same_layout"] = same_layout
    report["e_lfanew"] = {"released": pe_r.e_lfanew, "fresh": pe_f.e_lfanew}
    report["normalised_identical"] = same_layout and not regions
    report["differing_regions"] = regions
    return report


def print_sys_report(rep):
    def side(label, d):
        print(f"  {label}: {d['path']}")
        print(f"    stripped sha256 : {d['stripped_sha256']}")
        print(f"    size            : {d['size']} bytes, {len(d['sections'])} sections "
              f"({', '.join(s['name'] for s in d['sections'])})")
        print(f"    linker          : {d['linker_version']}   COFF timestamp {d['coff_timestamp']}   "
              f"checksum {d['checksum']}")
        for e in d["debug"]:
            extra = ""
            if "pdb_path" in e:
                extra = f"  pdb={e['pdb_path']} guid={e['pdb_guid']} age={e['pdb_age']}"
            elif "repro" in e:
                extra = f"  repro={e['repro']}"
            print(f"    debug[{e['type_name']}] ts={e['timestamp']:#010x} size={e['size']}{extra}")
        rich = d["rich"]
        if rich:
            ents = ", ".join(f"{e['product_id']}:{e['build']}x{e['count']}" for e in rich["entries"])
            print(f"    rich header     : {ents}  (product_id:build x count)")
        else:
            print("    rich header     : none")

    print("== StreamToSpeaker.sys (Authenticode signatures stripped from both) ==")
    side("released", rep["released"])
    side("fresh   ", rep["fresh"])
    print(f"  raw identical        : {rep['raw_identical']}")
    print(f"  normalised identical : {rep['normalised_identical']}  "
          f"(masked: {'; '.join(m['what'] for m in rep['masked'])}; "
          f"e_lfanew {rep['e_lfanew']['released']:#x} vs {rep['e_lfanew']['fresh']:#x})")
    print(f"  toolset identical    : {rep['toolset_identical']}  (Rich header build ids)")
    if not rep["toolset_identical"]:
        print("    the two images were produced by different compiler/linker/library builds; "
              "code is compared regardless")
    if rep["differing_regions"]:
        print("  differing regions after masking (offset, size, region):")
        for r in rep["differing_regions"]:
            print(f"    {r['offset']:#010x}  {r['size']:>7}  {r['region']}")
        total = sum(r["size"] for r in rep["differing_regions"])
        print(f"    {len(rep['differing_regions'])} region(s), {total} byte(s) total")


DRIVERVER_RE = re.compile(r"(DriverVer\s*=\s*)\d{2}/\d{2}/\d{4}(\s*,)", re.IGNORECASE)


def read_inf(path):
    with open(path, "rb") as fh:
        raw = fh.read()
    if raw.startswith(b"\xff\xfe"):
        return raw[2:].decode("utf-16-le"), "utf-16-le"
    if raw.startswith(b"\xfe\xff"):
        return raw[2:].decode("utf-16-be"), "utf-16-be"
    if raw.startswith(b"\xef\xbb\xbf"):
        return raw[3:].decode("utf-8"), "utf-8-sig"
    return raw.decode("utf-8", "replace"), "utf-8"


def compare_inf(released_path, fresh_path):
    text_r, enc_r = read_inf(released_path)
    text_f, enc_f = read_inf(fresh_path)
    ver_r = DRIVERVER_RE.search(text_r)
    ver_f = DRIVERVER_RE.search(text_f)
    masked_r = DRIVERVER_RE.sub(r"\1XX/XX/XXXX\2", text_r)
    masked_f = DRIVERVER_RE.sub(r"\1XX/XX/XXXX\2", text_f)
    lines_r, lines_f = masked_r.splitlines(), masked_f.splitlines()
    diffs = [(i + 1, a, b) for i, (a, b) in enumerate(zip(lines_r, lines_f)) if a != b]
    if len(lines_r) != len(lines_f):
        diffs.append((min(len(lines_r), len(lines_f)) + 1,
                      f"<{len(lines_r)} lines>", f"<{len(lines_f)} lines>"))
    driverver_r = text_r[ver_r.start():text_r.find("\n", ver_r.start())].strip() if ver_r else None
    driverver_f = text_f[ver_f.start():text_f.find("\n", ver_f.start())].strip() if ver_f else None
    def shown(text):
        return None if text is None else clean_text(text)

    return {
        "released": {"path": released_path, "encoding": enc_r, "driverver": shown(driverver_r)},
        "fresh": {"path": fresh_path, "encoding": enc_f, "driverver": shown(driverver_f)},
        "raw_identical": text_r == text_f and enc_r == enc_f,
        "normalised_identical": masked_r == masked_f and enc_r == enc_f,
        "differing_lines": [{"line": n, "released": shown(a), "fresh": shown(b)} for n, a, b in diffs[:50]],
    }


def print_inf_report(rep):
    print("== StreamToSpeaker.inf (DriverVer date masked) ==")
    for label in ("released", "fresh"):
        d = rep[label]
        print(f"  {label:8}: {d['path']}  [{d['encoding']}]  {d['driverver']}")
    print(f"  raw identical        : {rep['raw_identical']}")
    print(f"  normalised identical : {rep['normalised_identical']}")
    for d in rep["differing_lines"]:
        print(f"    line {d['line']}: released=[{d['released']}] fresh=[{d['fresh']}]")


def tree_files(root, subdirs):
    """{relative/posix/path: sha256} of every file under root/<subdir>."""
    out = {}
    for sub in subdirs:
        base = os.path.join(root, sub)
        for dirpath, _dirs, files in os.walk(base):
            for name in files:
                full = os.path.join(dirpath, name)
                rel = os.path.relpath(full, root).replace(os.sep, "/")
                with open(full, "rb") as fh:
                    out[rel] = sha256(fh.read())
    return out


STAMP_FILE = "driver/driver.h"
STAMP_RE = re.compile(rb"(STREAM_TO_SPEAKER_DRIVER_BUILD\s+)\d+u")


def stamp_driver_h(data, build):
    """driver.h as driver-submission.yml's "Stamp driver build number" step
    leaves it: every STREAM_TO_SPEAKER_DRIVER_BUILD value replaced by
    `build`, nothing else touched. Raises when there is nothing to stamp."""
    out, n = STAMP_RE.subn(lambda m: m.group(1) + str(int(build)).encode("ascii") + b"u", data)
    if n == 0:
        raise ValueError("no STREAM_TO_SPEAKER_DRIVER_BUILD definition to stamp")
    return out


def compare_trees(source_root, build_root, subdirs=("driver", "include"), stamped_build=None):
    """Is the tree msbuild is about to compile byte-identical to the checkout
    whose hashFiles() matched the release manifest? Catches any step that
    rewrites a source file between the identity guard and the build.

    With `stamped_build` (attested releases, whose driver.h was stamped
    before compiling) exactly one difference is required: driver/driver.h
    must be the checkout's file with the build constant set to that number.
    """
    a, b = tree_files(source_root, subdirs), tree_files(build_root, subdirs)
    changed = sorted(k for k in a.keys() & b.keys() if a[k] != b[k])
    rep = {
        "source_root": source_root,
        "build_root": build_root,
        "files": len(a),
        "only_in_source": sorted(a.keys() - b.keys()),
        "only_in_build": sorted(b.keys() - a.keys()),
        "changed": changed,
        "identical": bool(a) and a == b,
    }
    if stamped_build is None:
        return rep
    rep["stamped_build"] = int(stamped_build)
    rep["stamp_ok"] = False
    rep["stamp_problem"] = None
    try:
        with open(os.path.join(source_root, *STAMP_FILE.split("/")), "rb") as fh:
            want = stamp_driver_h(fh.read(), stamped_build)
        with open(os.path.join(build_root, *STAMP_FILE.split("/")), "rb") as fh:
            got = fh.read()
    except (OSError, ValueError) as e:
        rep["stamp_problem"] = str(e)
    else:
        rep["stamp_ok"] = got == want
        if not rep["stamp_ok"]:
            rep["stamp_problem"] = (f"{STAMP_FILE} in the build tree is not the checkout's file with "
                                    f"STREAM_TO_SPEAKER_DRIVER_BUILD set to {int(stamped_build)}")
    others = [k for k in changed if k != STAMP_FILE]
    rep["identical"] = (bool(a) and rep["stamp_ok"] and not others
                        and not rep["only_in_source"] and not rep["only_in_build"])
    return rep


def print_tree_report(rep):
    print(f"== build tree {rep['build_root']} vs checkout {rep['source_root']} ({rep['files']} files) ==")
    for key in ("only_in_source", "only_in_build", "changed"):
        for path in rep[key]:
            print(f"  {key.replace('_', ' ')}: {clean_text(path)}")
    if "stamped_build" in rep:
        print(f"  {STAMP_FILE} stamped to build {rep['stamped_build']}, otherwise as the checkout: {rep['stamp_ok']}")
        if rep["stamp_problem"]:
            print(f"  {rep['stamp_problem']}")
    print(f"  identical: {rep['identical']}")


# ---------------------------------------------------------------------------
# How a release was built
# ---------------------------------------------------------------------------
CERTIFIED_BUILD_ROOT = "C:\\build\\src"
PDB_TAIL_RE = re.compile(r"(?P<root>[A-Za-z]:(?:\\[^\\/:*?\"<>|\x00-\x1f\x7f]+)+)"
                         r"\\driver\\x64\\Release\\StreamToSpeaker\.pdb", re.IGNORECASE)
HEX64_RE = re.compile(r"[0-9a-fA-F]{64}")
ASSET_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]*")
BAD_COMPONENT_CHARS_RE = re.compile(r'[\\/:*?"<>|\[\]\x00-\x1f\x7f]')


def valid_build_root(path):
    """Is `path` a plain absolute Windows directory at least two levels
    below a drive root (<drive>:\\a\\b, one trailing backslash tolerated)?

    The build step empties this directory before it copies the source in,
    so nothing shallower is accepted, and no component that Windows or
    PowerShell would read as something else: `.` / `..`, a leading space,
    a trailing dot or space, wildcard brackets, control characters."""
    if not isinstance(path, str):
        return False
    parts = (path[:-1] if path.endswith("\\") else path).split("\\")
    if len(parts) < 3 or not re.fullmatch(r"[A-Za-z]:", parts[0]):
        return False
    for comp in parts[1:]:
        if (not comp or comp in (".", "..") or comp[0] == " " or comp[-1] in ". "
                or BAD_COMPONENT_CHARS_RE.search(comp)):
            return False
    return True


def build_plan(manifest, tag, build_number=None):
    """What the rebuild has to repeat for the driver-v* release `tag`,
    derived from its manifest alone. Returns a dict with `ok`, `errors`
    and, when ok, the plan fields."""
    errors = []
    m = manifest if isinstance(manifest, dict) else {}
    if not isinstance(manifest, dict):
        errors.append("manifest.json is not a JSON object")
    certified, attested = m.get("certified") is True, m.get("attested") is True

    def need(field, what, pattern=None):
        v = m.get(field)
        if v is None or v == "" or isinstance(v, bool):
            errors.append(f"the manifest of {tag} records no {field} ({what})")
            return None
        if not isinstance(v, (str, int)) or (pattern and not pattern.fullmatch(str(v))):
            errors.append(f"the manifest of {tag} has a malformed {field}: {v!r}")
            return None
        return v

    version = need("driver_version", "the DriverVer the rebuilt INF must carry",
                   re.compile(r"\d+\.\d+\.\d+\.\d+"))
    build = need("driver_build", "the DriverBuildNumber the driver was built with", re.compile(r"\d+"))
    source_hash = need("source_hash", "the driver source the release was built from", HEX64_RE)
    signed_zip = need("signed_zip", "the package installers bundle", ASSET_RE)
    signed_zip_sha256 = need("signed_zip_sha256", "the hash of the package installers bundle", HEX64_RE)
    if build is not None and version and not version.endswith(f".{build}"):
        errors.append(f"driver_version {version} does not end in driver_build {build}")
    if build_number not in (None, "") and build is not None and str(build_number) != str(build):
        errors.append(f"build_number input ({build_number}) differs from the manifest's driver_build ({build})")

    def optional(field, check):
        """A field that may be absent; when present it must pass `check`
        (a compiled pattern to match in full, or a predicate)."""
        v = m.get(field)
        if v in (None, ""):
            return ""
        ok = isinstance(v, str) and (check(v) if callable(check) else bool(check.fullmatch(v)))
        if not ok:
            errors.append(f"the manifest of {tag} has a malformed {field}: {v!r}")
            return ""
        return v

    commit = m.get("commit")
    plan = {"tag": tag, "version": version, "build_number": str(build) if build is not None else None,
            "source_hash": (source_hash or "").lower(),
            "signed_zip": signed_zip or "",
            "signed_zip_sha256": (signed_zip_sha256 or "").lower(),
            # Shown only; the checkout is identified by its source hash.
            "commit": commit if isinstance(commit, str) and re.fullmatch(r"[0-9a-fA-F]{7,64}", commit) else ""}
    if certified and attested:
        errors.append(f"the manifest of {tag} claims to be both certified and attested")
        plan.update(kind="")
    elif certified:
        # Lab build: tree as committed, fixed build directory (a manifest
        # `build_root` is an attestation field and plays no part here). An
        # older certified manifest without lab_zip falls back to the
        # conventional asset name; the download then decides.
        plan.update(kind="certified", stamp_driver_h=False, reference_type="lab_zip",
                    reference_asset=optional("lab_zip", ASSET_RE) or (
                        f"StreamToSpeaker-Driver-{version}-lab-testsigned.zip" if version else None),
                    reference_sha256=optional("lab_zip_sha256", HEX64_RE).lower(),
                    manifest_build_root="")
    elif attested:
        cab = need("submission_cab", "the submission CAB holding the unsigned driver that was built", ASSET_RE)
        cab_sha = need("submission_cab_signed_sha256",
                       "the hash of the EV-signed submission CAB kept on the release", HEX64_RE)
        if "driver_h_stamped" in m and m["driver_h_stamped"] is not True:
            errors.append(f"the manifest of {tag} says driver_h_stamped={m['driver_h_stamped']!r}; "
                          "attested builds are expected to stamp driver.h")
        plan.update(kind="attested", stamp_driver_h=True, reference_type="submission_cab",
                    reference_asset=cab, reference_sha256=(cab_sha or "").lower(),
                    manifest_build_root=optional("build_root", valid_build_root))
    else:
        errors.append(f"the manifest of {tag} is neither certified nor attested: "
                      "there is no Microsoft-signed driver to verify")
        plan.update(kind="")
    plan["errors"] = errors
    plan["ok"] = not errors
    return plan


def print_plan(plan):
    print(f"== build plan for {clean_text(plan['tag'])} ==")
    for e in plan["errors"]:
        print(f"::error::{command_text(e)}")
    if not plan["ok"]:
        return
    print(f"  kind              : {plan['kind']}")
    print(f"  driver            : {plan['version']} (DriverBuildNumber {plan['build_number']})")
    print(f"  manifest commit   : {plan['commit'] or '(none or not a commit id)'}")
    print(f"  source hash       : {plan['source_hash']}")
    print(f"  shipped package   : {plan['signed_zip']} (sha256 {plan['signed_zip_sha256']})")
    print(f"  driver.h stamped  : {plan['stamp_driver_h']}")
    print(f"  reference package : {plan['reference_asset']} ({plan['reference_type']}"
          f"{', sha256 ' + plan['reference_sha256'] if plan['reference_sha256'] else ''})")
    if plan["kind"] == "certified":
        print(f"  build directory   : {CERTIFIED_BUILD_ROOT}")
    else:
        print("  build directory   : where the submitted .sys was linked "
              f"(manifest build_root: {plan['manifest_build_root'] or 'not recorded'})")


def pdb_build_root(pdb_path):
    """D:\\a\\x\\x\\driver\\x64\\Release\\StreamToSpeaker.pdb -> D:\\a\\x\\x, or None."""
    m = PDB_TAIL_RE.fullmatch(pdb_path or "")
    return m.group("root") if m else None


def same_path(a, b):
    return a.rstrip("\\/").replace("/", "\\").lower() == b.rstrip("\\/").replace("/", "\\").lower()


def resolve_build_root(plan, pdb_path, override=None):
    """Where to rebuild: {"root", "source", "warning", "error"} (`root` is
    None exactly when `error` is set). See the module docstring
    (`buildroot`)."""
    res = {"root": None, "source": None, "warning": None, "error": None}
    recorded = plan.get("manifest_build_root") or ""
    if override:
        res.update(root=override, source="build_root input")
    elif plan.get("kind") == "certified":
        res.update(root=CERTIFIED_BUILD_ROOT, source="certified lab build directory")
    elif plan.get("kind") == "attested":
        derived = pdb_build_root(pdb_path)
        if derived:
            res.update(root=derived, source="PDB path of the submitted .sys")
            if recorded and not same_path(recorded, derived):
                res["warning"] = (f"the manifest records build_root {recorded} but the submitted .sys was "
                                  f"linked at {derived}; building where the binary was linked")
        elif recorded:
            res.update(root=recorded, source="manifest build_root; the PDB path of the submitted .sys "
                                             "does not have the expected form")
        else:
            res["error"] = ("the submitted .sys records the PDB path "
                            f"{pdb_path!r}, which is not <root>\\driver\\x64\\Release\\StreamToSpeaker.pdb, "
                            "and the manifest records no build_root: cannot tell where the driver was built")
    else:
        res["error"] = f"no build plan for release kind {plan.get('kind')!r}"
    if res["root"] is not None and not valid_build_root(res["root"]):
        res.update(root=None, error=f"build root {res['root']!r} is not a plain directory path at least two "
                                    "levels below a drive root; refusing to build there")
    elif res["root"] is not None and res["root"].endswith("\\"):
        res["root"] = res["root"][:-1]
    return res


def reference_pdb_path(sys_path):
    with open(sys_path, "rb") as fh:
        pe = PE(pe_strip_signature(fh.read()))
    for e in pe.debug_entries():
        if e.get("pdb_path"):
            return e["pdb_path"]
    return ""


# ---------------------------------------------------------------------------
# Archives from a release
# ---------------------------------------------------------------------------
# Nothing here writes to a path taken from an archive: the wanted files are
# picked by base name and written under their canonical names, flat, into
# the output directory. An entry cannot escape it, whatever its path says.
MAX_MEMBER_BYTES = 64 << 20      # driver files are tens of kilobytes
MAX_FOLDER_BYTES = 256 << 20     # a cabinet folder (.sys + .inf + .pdb + .cat)
REFERENCE_FILES = ("StreamToSpeaker.sys", "StreamToSpeaker.inf")
REFERENCE_TYPES = ("lab_zip", "submission_cab")


class ArchiveError(ValueError):
    pass


def member_basename(name):
    return re.split(r"[\\/]", name)[-1]


def pick_zip_members(zip_path, names):
    """pick_members() over the files of a zip."""
    with zipfile.ZipFile(zip_path) as z:
        def reader(info):
            def read():
                with z.open(info) as fh:
                    blob = fh.read(MAX_MEMBER_BYTES + 1)
                if len(blob) > MAX_MEMBER_BYTES:
                    raise ArchiveError(f"{clean_text(info.filename)} is larger than {MAX_MEMBER_BYTES} bytes")
                return blob
            return read

        return pick_members([(info.filename, info.file_size, reader(info))
                             for info in z.infolist() if not info.is_dir()], names)


def cab_entries(data):
    """[(member name, size, read())] for the files of a single-volume
    Microsoft cabinet (stored or MSZIP folders, which is what makecab
    writes for the submission CAB; an Authenticode-signed cabinet keeps its
    signature in the header's reserved area, which is skipped)."""
    if data[:4] != b"MSCF":
        raise ArchiveError("not a cabinet (no MSCF signature)")
    try:
        (_sig, _r1, _cb_cabinet, _r2, coff_files, _r3, _vminor, _vmajor, n_folders, n_files,
         flags, _set_id, _i_cabinet) = struct.unpack_from("<4sIIIIIBBHHHHH", data, 0)
        pos = 36
        res_folder = res_data = 0
        if flags & 0x0004:                       # cfhdrRESERVE_PRESENT
            res_header, res_folder, res_data = struct.unpack_from("<HBB", data, pos)
            pos += 4 + res_header
        if flags & 0x0003:                       # previous / next cabinet
            raise ArchiveError("multi-volume cabinets are not supported")
        folders = []
        for _ in range(n_folders):
            coff_start, n_data, compression = struct.unpack_from("<IHH", data, pos)
            pos += 8 + res_folder
            folders.append((coff_start, n_data, compression & 0x000F))
        files = []
        pos = coff_files
        for _ in range(n_files):
            cb_file, uoff, i_folder, _date, _time, attribs = struct.unpack_from("<IIHHHH", data, pos)
            pos += 16
            end = data.index(b"\0", pos)
            name = data[pos:end].decode("utf-8" if attribs & 0x80 else "latin-1", "replace")
            pos = end + 1
            files.append((name, cb_file, uoff, i_folder))
    except (struct.error, ValueError) as e:
        if isinstance(e, ArchiveError):
            raise
        raise ArchiveError(f"malformed cabinet: {e}")

    cache = {}

    def folder_bytes(index):
        if index in cache:
            return cache[index]
        if index >= len(folders):
            raise ArchiveError("a file refers to a folder this cabinet does not hold")
        coff_start, n_data, compression = folders[index]
        if compression not in (0, 1):
            raise ArchiveError(f"unsupported cabinet compression type {compression}")
        out, previous, pos = bytearray(), b"", coff_start
        try:
            for _ in range(n_data):
                _checksum, cb_data, cb_uncompressed = struct.unpack_from("<IHH", data, pos)
                pos += 8 + res_data
                block = bytes(data[pos:pos + cb_data])
                if len(block) != cb_data:
                    raise ArchiveError("truncated cabinet data block")
                pos += cb_data
                if compression == 0:
                    chunk = block
                else:
                    # MSZIP: "CK" + one raw deflate stream of at most 32 KiB,
                    # with the previous block's output as its dictionary.
                    if block[:2] != b"CK":
                        raise ArchiveError("bad MSZIP block signature")
                    inflater = zlib.decompressobj(-15, zdict=previous) if previous else zlib.decompressobj(-15)
                    chunk = inflater.decompress(block[2:], 32769)
                    if len(chunk) > 32768 or inflater.unconsumed_tail:
                        raise ArchiveError("MSZIP block larger than 32 KiB")
                if len(chunk) != cb_uncompressed:
                    raise ArchiveError("cabinet data block size mismatch")
                out += chunk
                previous = chunk
                if len(out) > MAX_FOLDER_BYTES:
                    raise ArchiveError(f"cabinet folder larger than {MAX_FOLDER_BYTES} bytes")
        except (struct.error, zlib.error) as e:
            raise ArchiveError(f"malformed cabinet data: {e}")
        cache[index] = bytes(out)
        return cache[index]

    def reader(cb_file, uoff, i_folder):
        def read():
            blob = folder_bytes(i_folder)[uoff:uoff + cb_file]
            if len(blob) != cb_file:
                raise ArchiveError("a file extends past its folder's data")
            return blob
        return read

    return [(name, cb_file, reader(cb_file, uoff, i_folder)) for name, cb_file, uoff, i_folder in files]


def pick_members(entries, names):
    """From archive entries pick the wanted files by base name (matched
    case-insensitively, any directory): ({canonical name: (member, bytes)},
    missing names, duplicate members)."""
    wanted = {n.lower(): n for n in names}
    found, duplicates = {}, []
    for member, size, read in entries:
        canon = wanted.get(member_basename(member).lower())
        if not canon:
            continue
        if canon in found:
            duplicates.append(clean_text(member))
            continue
        if size > MAX_MEMBER_BYTES:
            raise ArchiveError(f"{clean_text(member)} is larger than {MAX_MEMBER_BYTES} bytes")
        found[canon] = (member, read())
    return found, [n for n in names if n not in found], duplicates


def write_members(found, out_dir):
    """Write picked members flat under their canonical names; the report rows."""
    os.makedirs(out_dir, exist_ok=True)
    rows = {}
    for canon, (member, data) in found.items():
        with open(os.path.join(out_dir, canon), "wb") as fh:
            fh.write(data)
        rows[canon] = {"member": clean_text(member), "size": len(data), "sha256": sha256(data)}
    return rows


def check_package(zip_path, expected_sha256, out_dir, names=PACKAGE_FILES):
    """Verify a package zip's SHA-256 and extract its driver files flat."""
    with open(zip_path, "rb") as fh:
        digest = sha256(fh.read())
    rep = {
        "zip": zip_path,
        "sha256": digest,
        "expected_sha256": (expected_sha256 or "").lower(),
        "sha256_ok": bool(expected_sha256) and digest == expected_sha256.lower(),
        "files": {},
        "missing": [],
        "duplicates": [],
        "error": None,
    }
    found = {}
    try:
        found, rep["missing"], rep["duplicates"] = pick_zip_members(zip_path, names)
    except (ArchiveError, zipfile.BadZipFile, OSError, zlib.error, RuntimeError, NotImplementedError) as e:
        rep["error"] = clean_text(f"could not read the zip: {e}")
    rep["ok"] = rep["sha256_ok"] and not rep["error"] and not rep["missing"] and not rep["duplicates"]
    if rep["ok"]:
        rep["files"] = write_members(found, out_dir)
    return rep


def print_package_report(rep):
    print(f"== package {rep['zip']} ==")
    print(f"  sha256   : {rep['sha256']}")
    print(f"  expected : {clean_text(rep['expected_sha256']) or '(none)'}  -> {'match' if rep['sha256_ok'] else 'MISMATCH'}")
    for name, f in rep["files"].items():
        print(f"  {name:22} {f['sha256']}  ({f['size']} bytes, {f['member']})")
    for name in rep["missing"]:
        print(f"  missing: {name}")
    for member in rep["duplicates"]:
        print(f"  duplicate: {member}")
    if rep["error"]:
        print(f"  {rep['error']}")
    print(f"  ok: {rep['ok']}")


def extract_reference(archive_path, kind, out_dir, expected_sha256=None, names=REFERENCE_FILES):
    """The reference .sys/.inf of a release, flat in out_dir: from the lab
    zip of a certified release or the submission CAB of an attested one.

    The archive's SHA-256 must equal `expected_sha256` when one is given; a
    submission CAB must always come with one (the manifest's
    `submission_cab_signed_sha256`), an older certified manifest may not
    record its lab zip's hash."""
    rep = {"archive": archive_path, "type": kind, "sha256": None,
           "expected_sha256": (expected_sha256 or "").lower(), "sha256_ok": False,
           "files": {}, "missing": [], "duplicates": [], "error": None, "ok": False}
    if kind not in REFERENCE_TYPES:
        rep["error"] = clean_text(f"unknown reference type {kind!r}")
        return rep
    try:
        with open(archive_path, "rb") as fh:
            data = fh.read()
    except OSError as e:
        rep["error"] = clean_text(f"could not read the archive: {e}")
        return rep
    rep["sha256"] = sha256(data)
    if expected_sha256:
        rep["sha256_ok"] = rep["sha256"] == expected_sha256.lower()
        if not rep["sha256_ok"]:
            rep["error"] = "the archive is not the one the manifest records (SHA-256 differs)"
            return rep
    elif kind == "submission_cab":
        rep["error"] = "no expected SHA-256 given for the submission CAB"
        return rep
    try:
        if kind == "submission_cab":
            found, rep["missing"], rep["duplicates"] = pick_members(cab_entries(data), names)
        else:
            found, rep["missing"], rep["duplicates"] = pick_zip_members(archive_path, names)
    except (ArchiveError, zipfile.BadZipFile, OSError, zlib.error, RuntimeError, NotImplementedError) as e:
        rep["error"] = clean_text(f"could not read the archive: {e}")
        return rep
    rep["ok"] = not rep["missing"] and not rep["duplicates"]
    if rep["ok"]:
        rep["files"] = write_members(found, out_dir)
    return rep


def print_reference_report(rep):
    print(f"== reference package {rep['archive']} ({clean_text(rep['type'])}) ==")
    print(f"  sha256   : {rep['sha256']}")
    if rep["expected_sha256"]:
        print(f"  expected : {clean_text(rep['expected_sha256'])}  -> {'match' if rep['sha256_ok'] else 'MISMATCH'}")
    else:
        print("  expected : (the manifest records none)")
    for name, f in rep["files"].items():
        print(f"  {name:22} {f['sha256']}  ({f['size']} bytes, {f['member']})")
    for name in rep["missing"]:
        print(f"  missing: {name}")
    for member in rep["duplicates"]:
        print(f"  duplicate: {member}")
    if rep["error"]:
        print(f"  {rep['error']}")
    print(f"  ok: {rep['ok']}")


def find_file(directory, name):
    """Path of `name` in `directory`, matched case-insensitively, or None."""
    if not os.path.isdir(directory):
        return None
    for entry in sorted(os.listdir(directory)):
        if entry.lower() == name.lower() and os.path.isfile(os.path.join(directory, entry)):
            return os.path.join(directory, entry)
    return None


def compare_files(dir_a, dir_b, names=PACKAGE_FILES):
    """Are the named files byte-identical in both directories?"""
    files = {}
    for name in names:
        row = {}
        for label, d in (("a", dir_a), ("b", dir_b)):
            p = find_file(d, name)
            if p:
                with open(p, "rb") as fh:
                    row[label] = sha256(fh.read())
            else:
                row[label] = None
        row["identical"] = row["a"] is not None and row["a"] == row["b"]
        files[name] = row
    return {"a": dir_a, "b": dir_b, "files": files,
            "identical": all(r["identical"] for r in files.values())}


def print_files_report(rep):
    print(f"== {rep['a']} vs {rep['b']} ==")
    for name, r in rep["files"].items():
        print(f"  {name:22} {'identical' if r['identical'] else 'DIFFERENT'}  "
              f"{r['a'] or '(missing)'} / {r['b'] or '(missing)'}")
    print(f"  identical: {rep['identical']}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("kind", choices=("sys", "inf", "tree", "package", "reference", "files", "plan", "buildroot"))
    ap.add_argument("released")
    ap.add_argument("fresh")
    ap.add_argument("--json", help="also write the full report here")
    ap.add_argument("--sha256", help="package / reference: the SHA-256 the release manifest records for the archive")
    ap.add_argument("--type", choices=REFERENCE_TYPES, help="reference: lab zip or submission CAB")
    ap.add_argument("--stamped-build", help="tree: driver.h in the build tree is stamped with this build number")
    ap.add_argument("--build-number", help="plan: optional build number that must equal the manifest's driver_build")
    ap.add_argument("--build-root", help="buildroot: build here instead of the derived directory")
    a = ap.parse_args(argv)
    # Whatever ends up in a message, printing it must not fail on the
    # console's encoding.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="backslashreplace")

    def dump(rep):
        if a.json:
            with open(a.json, "w", encoding="utf-8") as fh:
                json.dump(rep, fh, indent=2)

    # plan / buildroot print workflow commands of their own (::error::,
    # ::warning::); what they show of the release goes through clean_text.
    if a.kind == "plan":
        try:
            with open(a.released, "r", encoding="utf-8-sig") as fh:
                manifest = json.load(fh)
        except (OSError, ValueError) as e:
            print(f"::error::{command_text(f'could not read {a.released}: {e}')}")
            return 1
        rep = build_plan(manifest, a.fresh, a.build_number)
        print_plan(rep)
        dump(rep)
        if rep["ok"]:
            gh_output(kind=rep["kind"], version=rep["version"], build_number=rep["build_number"],
                      source_hash=rep["source_hash"], stamp_driver_h=rep["stamp_driver_h"],
                      reference_type=rep["reference_type"], reference_asset=rep["reference_asset"],
                      reference_sha256=rep["reference_sha256"], signed_zip=rep["signed_zip"],
                      signed_zip_sha256=rep["signed_zip_sha256"])
        return 0 if rep["ok"] else 1
    if a.kind == "buildroot":
        try:
            with open(a.released, "r", encoding="utf-8") as fh:
                plan = json.load(fh)
            pdb = reference_pdb_path(a.fresh)
        except (OSError, ValueError, struct.error, IndexError) as e:
            print(f"::error::{command_text(f'could not read the plan or the PDB path of {a.fresh}: {e}')}")
            return 1
        res = resolve_build_root(plan, pdb, a.build_root)
        print(f"reference .sys PDB path: [{clean_text(pdb)}]")
        if res["error"]:
            print(f"::error::{command_text(res['error'])}")
            return 1
        if res["warning"]:
            print(f"::warning::{command_text(res['warning'])}")
        print(f"build root: {res['root']} ({res['source']})")
        gh_output(build_root=res["root"])
        return 0

    # The other reports are built from release files and print no workflow
    # commands: the runner reads none from their output.
    def report():
        if a.kind == "package":
            rep = check_package(a.released, a.sha256, a.fresh)
            print_package_report(rep)
            dump(rep)
            return (0 if rep["ok"] else 1), {"package_ok": rep["ok"], "package_sha256": rep["sha256"]}
        if a.kind == "reference":
            rep = extract_reference(a.released, a.type, a.fresh, a.sha256)
            print_reference_report(rep)
            dump(rep)
            return (0 if rep["ok"] else 1), {"reference_ok": rep["ok"]}
        if a.kind == "files":
            rep = compare_files(a.released, a.fresh)
            print_files_report(rep)
            dump(rep)
            return (0 if rep["identical"] else 1), {"files_identical": rep["identical"]}
        if a.kind == "tree":
            rep = compare_trees(a.released, a.fresh, stamped_build=a.stamped_build)
            print_tree_report(rep)
            dump(rep)
            return (0 if rep["identical"] else 1), {}
        try:
            if a.kind == "sys":
                rep = compare_sys(a.released, a.fresh)
                print_sys_report(rep)
            else:
                rep = compare_inf(a.released, a.fresh)
                print_inf_report(rep)
        except (OSError, ValueError, struct.error, IndexError) as e:
            # No verdict (and no outputs): a file that cannot be read or
            # parsed is not evidence either way.
            print(f"error: {clean_text(f'could not compare {a.released} with {a.fresh}: {e}')}")
            return 2, {}
        dump(rep)
        return (0 if rep["normalised_identical"] else 1), {
            "raw_identical": rep["raw_identical"], "normalised_identical": rep["normalised_identical"],
            "toolset_identical": rep.get("toolset_identical", True)}

    with quarantined_output():
        code, outputs = report()
    gh_output(**outputs)
    return code


if __name__ == "__main__":
    sys.exit(main())
