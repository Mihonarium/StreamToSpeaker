#!/usr/bin/env python3
"""Compare a released driver package against a fresh build of the same source.

Used by driver-reproducibility.yml. Pure Python, no third-party modules.

  repro_compare.py sys  <released.sys> <fresh.sys>   [--json out.json]
  repro_compare.py inf  <released.inf> <fresh.inf>   [--json out.json]
  repro_compare.py tree <checkout>     <build_root>  [--json out.json]
  repro_compare.py package <package.zip> <out_dir> --sha256 <expected> [--json out.json]
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

`files` checks that the .sys/.inf/.cat in two directories are
byte-identical (e.g. what an installer put on disk vs the package it is
supposed to bundle).

Exit status is 0 when the normalised comparison matches, 1 otherwise.
When GITHUB_OUTPUT is set, `raw_identical`, `normalised_identical` and
`toolset_identical` (true/false) are appended to it.
"""
import argparse
import hashlib
import json
import os
import re
import struct
import sys
import zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from certified import PACKAGE_FILES, pe_strip_signature  # noqa: E402

IMAGE_DEBUG_TYPE = {
    0: "UNKNOWN", 1: "COFF", 2: "CODEVIEW", 3: "FPO", 4: "MISC", 5: "EXCEPTION",
    6: "FIXUP", 7: "OMAP_TO_SRC", 8: "OMAP_FROM_SRC", 9: "BORLAND", 10: "RESERVED10",
    11: "CLSID", 12: "VC_FEATURE", 13: "POGO", 14: "ILTCG", 15: "MPX", 16: "REPRO",
    17: "SPGO", 20: "EX_DLLCHARACTERISTICS",
}


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
            name = data[off:off + 8].rstrip(b"\0").decode("ascii", "replace")
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
            "debug": [{k: v for k, v in e.items() if k not in ("entry_off", "raw_off")} for e in self.debug_entries()],
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
    if not path:
        return
    with open(path, "a", encoding="utf-8") as fh:
        for k, v in kv.items():
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
    return {
        "released": {"path": released_path, "encoding": enc_r, "driverver": driverver_r},
        "fresh": {"path": fresh_path, "encoding": enc_f, "driverver": driverver_f},
        "raw_identical": text_r == text_f and enc_r == enc_f,
        "normalised_identical": masked_r == masked_f and enc_r == enc_f,
        "differing_lines": [{"line": n, "released": a, "fresh": b} for n, a, b in diffs[:50]],
    }


def print_inf_report(rep):
    print("== StreamToSpeaker.inf (DriverVer date masked) ==")
    for label in ("released", "fresh"):
        d = rep[label]
        print(f"  {label:8}: {d['path']}  [{d['encoding']}]  {d['driverver']}")
    print(f"  raw identical        : {rep['raw_identical']}")
    print(f"  normalised identical : {rep['normalised_identical']}")
    for d in rep["differing_lines"]:
        print(f"    line {d['line']}: released={d['released']!r} fresh={d['fresh']!r}")


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


def compare_trees(source_root, build_root, subdirs=("driver", "include")):
    """Is the tree msbuild is about to compile byte-identical to the checkout
    whose hashFiles() matched the release manifest? Catches any step that
    rewrites a source file (e.g. stamping driver.h) between the identity
    guard and the build."""
    a, b = tree_files(source_root, subdirs), tree_files(build_root, subdirs)
    changed = sorted(k for k in a.keys() & b.keys() if a[k] != b[k])
    return {
        "source_root": source_root,
        "build_root": build_root,
        "files": len(a),
        "only_in_source": sorted(a.keys() - b.keys()),
        "only_in_build": sorted(b.keys() - a.keys()),
        "changed": changed,
        "identical": bool(a) and a == b,
    }


def print_tree_report(rep):
    print(f"== build tree {rep['build_root']} vs checkout {rep['source_root']} ({rep['files']} files) ==")
    for key in ("only_in_source", "only_in_build", "changed"):
        for path in rep[key]:
            print(f"  {key.replace('_', ' ')}: {path}")
    print(f"  identical: {rep['identical']}")


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
    }
    wanted = {n.lower(): n for n in names}
    found = {}
    with zipfile.ZipFile(zip_path) as z:
        for info in z.infolist():
            if info.is_dir():
                continue
            canon = wanted.get(os.path.basename(info.filename).lower())
            if not canon:
                continue
            if canon in found:
                rep["duplicates"].append(info.filename)
                continue
            found[canon] = (info.filename, z.read(info))
    rep["missing"] = [n for n in names if n not in found]
    if rep["sha256_ok"] and not rep["missing"] and not rep["duplicates"]:
        os.makedirs(out_dir, exist_ok=True)
        for canon, (member, data) in found.items():
            with open(os.path.join(out_dir, canon), "wb") as fh:
                fh.write(data)
            rep["files"][canon] = {"member": member, "size": len(data), "sha256": sha256(data)}
    rep["ok"] = rep["sha256_ok"] and not rep["missing"] and not rep["duplicates"]
    return rep


def print_package_report(rep):
    print(f"== package {rep['zip']} ==")
    print(f"  sha256   : {rep['sha256']}")
    print(f"  expected : {rep['expected_sha256'] or '(none)'}  -> {'match' if rep['sha256_ok'] else 'MISMATCH'}")
    for name, f in rep["files"].items():
        print(f"  {name:22} {f['sha256']}  ({f['size']} bytes, {f['member']})")
    for name in rep["missing"]:
        print(f"  missing: {name}")
    for member in rep["duplicates"]:
        print(f"  duplicate: {member}")
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
    ap.add_argument("kind", choices=("sys", "inf", "tree", "package", "files"))
    ap.add_argument("released")
    ap.add_argument("fresh")
    ap.add_argument("--json", help="also write the full report here")
    ap.add_argument("--sha256", help="package: the SHA-256 the release manifest records for the zip")
    a = ap.parse_args(argv)

    def dump(rep):
        if a.json:
            with open(a.json, "w", encoding="utf-8") as fh:
                json.dump(rep, fh, indent=2)

    if a.kind == "package":
        rep = check_package(a.released, a.sha256, a.fresh)
        print_package_report(rep)
        dump(rep)
        gh_output(package_ok=rep["ok"], package_sha256=rep["sha256"])
        return 0 if rep["ok"] else 1
    if a.kind == "files":
        rep = compare_files(a.released, a.fresh)
        print_files_report(rep)
        dump(rep)
        gh_output(files_identical=rep["identical"])
        return 0 if rep["identical"] else 1
    if a.kind == "sys":
        rep = compare_sys(a.released, a.fresh)
        print_sys_report(rep)
    elif a.kind == "inf":
        rep = compare_inf(a.released, a.fresh)
        print_inf_report(rep)
    else:
        rep = compare_trees(a.released, a.fresh)
        print_tree_report(rep)
        if a.json:
            with open(a.json, "w", encoding="utf-8") as fh:
                json.dump(rep, fh, indent=2)
        return 0 if rep["identical"] else 1
    if a.json:
        with open(a.json, "w", encoding="utf-8") as fh:
            json.dump(rep, fh, indent=2)
    gh_output(raw_identical=rep["raw_identical"], normalised_identical=rep["normalised_identical"],
              toolset_identical=rep.get("toolset_identical", True))
    return 0 if rep["normalised_identical"] else 1


if __name__ == "__main__":
    sys.exit(main())
