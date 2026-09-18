#!/usr/bin/env python3
"""WHQL-certified driver path: fetch, verify and publish a certified package.

Driven by driver-certified.yml (companion of attest.py, whose Hardware API
helpers it reuses). A WHQL/HLK submission is NOT the attestation flow:
the tested binaries come from an HLK lab run (test-signed, packed into an
.hlkx by HLK Studio), Microsoft re-signs the .sys (nested WHQL signature —
the lab's test signature stays the primary embedded one) and re-issues the
catalog, and the INF comes back byte-identical. So verification here is:

  1. INF in Microsoft's zip == INF in the lab package (byte-identical);
  2. the .sys in both packages is the same code: identical after stripping
     the Authenticode certificate table (what `signtool remove /s` does);
  3. the Microsoft Windows Hardware Compatibility Publisher appears in the
     .sys's certificate table and in the re-issued .cat.
The Windows job additionally runs signtool (/kp against the catalog, and
/all on the embedded signatures). The published manifest carries
`certified: true` plus the certification IDs; Get-AttestedDriver.ps1
prefers it over attested manifests with the same source hash.

Subcommands:
  hash      hashFiles('driver/**','include/**') exactly as the Windows
            runners compute it (see hash_files)
  fetch     poll a Partner Center submission until a signedPackage exists,
            download it (needs AZURE_* env like attest.py)
  verify    run checks 1-3 on a Microsoft zip + lab zip
  release   build the canonical -Signed.zip + manifest.json and put them
            on the driver-v<ver> release (creates it when missing)
Stdlib only; release asset I/O goes through the gh CLI.
"""
import argparse
import hashlib
import io
import json
import os
import struct
import subprocess
import sys
import time
import zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import attest  # noqa: E402

MS_PUBLISHER = b"Microsoft Windows Hardware Compatibility Publisher"
PACKAGE_FILES = ("StreamToSpeaker.inf", "StreamToSpeaker.sys", "StreamToSpeaker.cat")


# ---------------------------------------------------------------------------
# hashFiles
# ---------------------------------------------------------------------------
def hash_files(root, dirs=("driver", "include")):
    """Reproduce GitHub's hashFiles('driver/**','include/**') on a Windows runner.

    actions/toolkit (packages/glob/src/internal-hash-files.ts): sha256 of
    every matched file, digests concatenated in traversal order, sha256 of
    that. Traversal (internal-globber.ts) is a depth-first walk in readdir
    order, which on NTFS is the upcase-table collation — approximated by
    sorting on name.upper(). Windows runners check out with
    core.autocrlf=true, so text files stored with LF hash as CRLF (git only
    converts files with no CR and no NUL). Validated against the
    driver-v1.1.0.204 manifest: this function over `git archive cde07512
    driver include` reproduces its source_hash exactly; neither byte-order
    traversal nor raw LF bytes do.
    """
    def crlf(b):
        if b"\0" in b[:8000] or b"\r" in b:
            return b
        return b.replace(b"\n", b"\r\n")

    def walk(rel):
        out = []
        for name in sorted(os.listdir(os.path.join(root, rel)), key=str.upper):
            p = os.path.join(rel, name)
            if os.path.isdir(os.path.join(root, p)):
                out.extend(walk(p))
            else:
                out.append(p)
        return out

    result = hashlib.sha256()
    count = 0
    for d in dirs:
        if not os.path.isdir(os.path.join(root, d)):
            continue
        for f in walk(d):
            with open(os.path.join(root, f), "rb") as fh:
                result.update(hashlib.sha256(crlf(fh.read())).digest())
            count += 1
    return result.hexdigest() if count else ""


# ---------------------------------------------------------------------------
# Package inspection (pure Python, runs anywhere)
# ---------------------------------------------------------------------------
def pe_strip_signature(data):
    """Return `data` without its Authenticode certificate table.

    Equivalent to `signtool remove /s` for comparison purposes: the table
    (IMAGE_DIRECTORY_ENTRY_SECURITY, always at the end of the file) is cut
    off, its directory entry and the PE CheckSum are zeroed. Two builds of
    the same code strip to identical bytes regardless of who signed them.
    """
    if data[:2] != b"MZ":
        raise ValueError("not a PE file")
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew:e_lfanew + 4] != b"PE\0\0":
        raise ValueError("bad PE header")
    opt = e_lfanew + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    checksum_off = opt + 64
    dir_off = opt + (112 if magic == 0x20B else 96)
    sec_off = dir_off + 4 * 8
    va, size = struct.unpack_from("<II", data, sec_off)
    out = bytearray(data)
    if size:
        if va + size != len(data):
            raise ValueError(f"certificate table not at end of file ({va}+{size} != {len(data)})")
        del out[va:]
    out[sec_off:sec_off + 8] = b"\0" * 8
    out[checksum_off:checksum_off + 4] = b"\0" * 4
    return bytes(out)


def pe_certificate_table(data):
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    opt = e_lfanew + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    sec_off = opt + (112 if magic == 0x20B else 96) + 4 * 8
    va, size = struct.unpack_from("<II", data, sec_off)
    return data[va:va + size]


def read_package(zip_bytes):
    """{basename.lower(): bytes} for the inf/sys/cat of a driver package zip.

    Tolerates any directory layout (Microsoft returns drivers/<guid>/…, the
    lab zip is flat, our canonical zip is flat) but refuses ambiguity.
    """
    out = {}
    with zipfile.ZipFile(io.BytesIO(zip_bytes)) as z:
        for info in z.infolist():
            base = os.path.basename(info.filename).lower()
            if base in (n.lower() for n in PACKAGE_FILES):
                if base in out:
                    raise ValueError(f"zip contains more than one {base}")
                out[base] = z.read(info)
    missing = [n for n in PACKAGE_FILES if n.lower() not in out]
    if missing:
        raise ValueError(f"zip is missing {', '.join(missing)}")
    return out


def verify_packages(ms_zip, lab_zip, version=None):
    """Checks 1-3 from the module docstring. Returns a list of findings
    (strings) — empty means the Microsoft zip is the certified lab package."""
    problems = []
    ms = read_package(ms_zip)
    lab = read_package(lab_zip)
    if ms["streamtospeaker.inf"] != lab["streamtospeaker.inf"]:
        problems.append("INF differs between Microsoft's zip and the lab package")
    if version and f",{version}".encode() not in ms["streamtospeaker.inf"]:
        problems.append(f"INF does not carry DriverVer {version}")
    try:
        if pe_strip_signature(ms["streamtospeaker.sys"]) != pe_strip_signature(lab["streamtospeaker.sys"]):
            problems.append(".sys code differs between Microsoft's zip and the lab package (after stripping signatures)")
    except ValueError as e:
        problems.append(f".sys could not be parsed: {e}")
    if MS_PUBLISHER not in pe_certificate_table(ms["streamtospeaker.sys"]):
        problems.append("Microsoft's .sys carries no Microsoft Windows Hardware Compatibility Publisher signature")
    if MS_PUBLISHER not in ms["streamtospeaker.cat"]:
        problems.append("Microsoft's .cat is not signed by the Microsoft Windows Hardware Compatibility Publisher")
    return problems


def canonical_zip(ms_zip):
    """Flat StreamToSpeaker.{inf,sys,cat} zip of Microsoft's package, with
    fixed timestamps so the same input always gives the same bytes."""
    ms = read_package(ms_zip)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name in PACKAGE_FILES:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            z.writestr(info, ms[name.lower()])
    return buf.getvalue()


# ---------------------------------------------------------------------------
# Partner Center
# ---------------------------------------------------------------------------
def fetch_signed_package(api, product_id, submission_id, now=time.time,
                         sleep=time.sleep, deadline_s=2700, poll_s=60):
    """Poll until the submission is completed with a signedPackage; return
    (zip bytes, submission JSON). Raises RuntimeError on failure/timeout."""
    start = now()
    while True:
        sub = api.get_submission(product_id, submission_id)
        wf = sub.get("workflowStatus") or {}
        state = wf.get("state")
        if state == "failed" or sub.get("commitStatus") == "commitFailed":
            raise RuntimeError(f"submission failed at step {wf.get('currentStep')}: {wf.get('messages')}")
        url = attest.find_download(sub, "signedPackage")
        if state == "completed" and url:
            return api.download(url), sub
        if now() - start > deadline_s:
            raise RuntimeError(f"poll deadline exceeded at step {wf.get('currentStep')} (state {state})")
        print(f"submission at {wf.get('currentStep')} ({state}); waiting")
        sleep(poll_s)


# ---------------------------------------------------------------------------
# Manifest + release
# ---------------------------------------------------------------------------
def build_manifest(version, commit, source_hash, product_id, submission_id,
                   os_list, report_id, ms_zip, lab_zip, signed_zip):
    """manifest.json for a certified driver-v release. Field set is a
    superset of the attestation manifest so every consumer (attest.py,
    Get-AttestedDriver.ps1, the cleanup step) reads it unchanged."""
    prefix, build = version.rsplit(".", 1)
    ms_name = f"StreamToSpeaker-Driver-{version}-Microsoft.zip"
    lab_name = f"StreamToSpeaker-Driver-{version}-lab-testsigned.zip"
    signed_name = f"StreamToSpeaker-Driver-{version}-Signed.zip"
    return {
        "schema": 1,
        "driver_version": version,
        "driver_prefix": prefix,
        "driver_build": int(build),
        "commit": commit,
        "source_hash": source_hash,
        "submission_cab": None,
        "submission_cab_sha256": None,
        "submission_cab_signed_sha256": None,
        "attested": False,
        "certified": True,
        "certification": {
            "product_id": str(product_id),
            "submission_id": str(submission_id),
            "os": list(os_list),
            "report_id": int(report_id) if report_id else None,
        },
        "signed_zip": signed_name,
        "signed_zip_sha256": hashlib.sha256(signed_zip).hexdigest(),
        "ms_zip_asset": ms_name,
        "ms_zip_sha256": hashlib.sha256(ms_zip).hexdigest(),
        "lab_zip": lab_name,
        "lab_zip_sha256": hashlib.sha256(lab_zip).hexdigest(),
        "ms_product_id": str(product_id),
        "ms_submission_id": str(submission_id),
        "ms_state": "done",
    }


def release_notes(m):
    c = m["certification"]
    return "\n".join([
        f"WHQL-certified driver **{m['driver_version']}** (commit {m['commit']}).",
        "",
        f"Install `{m['signed_zip']}` on stock Windows 10 1809+ / Windows 11 — Secure Boot on, no test-signing mode. "
        "WHQL-certified (HLK) for " + ", ".join(c["os"]) + ".",
        "",
        "| | |",
        "| --- | --- |",
        f"| Driver package | `{m['signed_zip']}` |",
        f"| SHA256 | `{m['signed_zip_sha256']}` |",
        f"| Driver source hash | `{m['source_hash']}` |",
        f"| Partner Center product / submission | `{c['product_id']}` / `{c['submission_id']}` |",
        f"| Certification report | `{c['report_id']}` |",
        "",
        "Verified in CI: `signtool verify /kp` against the re-issued catalog, a *Microsoft Windows Hardware "
        "Compatibility Publisher* signature on the `.sys` (nested behind the lab's test signature — expected for "
        "HLK submissions), `.sys` code byte-identical to the lab-tested binary after stripping signatures, "
        "INF byte-identical to the tested one and carrying DriverVer " + m["driver_version"] + ".",
        "",
        "Installer builds bundle this driver automatically while `driver/**` + `include/**` still hash to the "
        f"source hash above, in preference to any attestation-signed build. `{m['lab_zip']}` is the package "
        f"exactly as tested in the lab (test-signed); `{m['ms_zip_asset']}` is the zip Microsoft returned. "
        "Certification is per binary: any driver change needs a new HLK run.",
    ])


def gh(*args, **kw):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True, **kw).stdout


def ensure_release(repo, tag, version, commit):
    r = subprocess.run(["gh", "release", "view", tag, "--repo", repo], capture_output=True, text=True)
    if r.returncode == 0:
        return False
    gh("release", "create", tag, "--repo", repo, "--prerelease", "--target", commit,
       "--title", f"Driver {version} — WHQL-certified", "--notes", f"Certified driver {version}: assets being published.")
    return True


def publish_release(repo, tag, manifest, files, notes):
    """Upload `files` ({name: bytes}) + manifest.json to the release and set
    the notes. Everything is --clobber so re-runs converge."""
    names = []
    for name, data in files.items():
        with open(name, "wb") as f:
            f.write(data)
        names.append(name)
        sha_name = f"{name}.sha256"
        with open(sha_name, "w") as f:
            f.write(f"{hashlib.sha256(data).hexdigest()}  {name}")
        names.append(sha_name)
    with open("manifest.json", "w") as f:
        json.dump(manifest, f, indent=2)
    names.append("manifest.json")
    gh("release", "upload", tag, "--repo", repo, "--clobber", *names)
    with open("release-notes.md", "w") as f:
        f.write(notes)
    gh("release", "edit", tag, "--repo", repo, "--prerelease",
       "--title", f"Driver {manifest['driver_version']} — WHQL-certified", "--notes-file", "release-notes.md")


def download_asset(repo, tag, name):
    return attest.Release(repo, tag).download_asset(name)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------
def cmd_hash(a):
    print(hash_files(a.root))


def cmd_fetch(a):
    token = attest.get_token(os.environ["AZURE_TENANT_ID"], os.environ["AZURE_CLIENT_ID"],
                             os.environ["AZURE_CLIENT_SECRET"])
    data, sub = fetch_signed_package(attest.HardwareApi(token), a.product_id, a.submission_id)
    with open(a.out, "wb") as f:
        f.write(data)
    redacted = {k: v for k, v in sub.items() if k != "downloads"}
    print(f"signed package: {len(data)} bytes sha256 {hashlib.sha256(data).hexdigest()}")
    print(json.dumps(redacted, indent=2))


def cmd_verify(a):
    with open(a.ms_zip, "rb") as f:
        ms = f.read()
    with open(a.lab_zip, "rb") as f:
        lab = f.read()
    problems = verify_packages(ms, lab, a.version)
    for p in problems:
        print(f"::error::{p}")
    if problems:
        return 1
    print("Microsoft's zip is the certified lab package: INF identical, .sys code identical, "
          "Microsoft Windows Hardware Compatibility Publisher on .sys and .cat")
    return 0


def cmd_release(a):
    with open(a.ms_zip, "rb") as f:
        ms = f.read()
    with open(a.lab_zip, "rb") as f:
        lab = f.read()
    problems = verify_packages(ms, lab, a.version)
    if problems:
        for p in problems:
            print(f"::error::{p}")
        return 1
    signed = canonical_zip(ms)
    m = build_manifest(a.version, a.commit, a.source_hash, a.product_id, a.submission_id,
                       [s.strip() for s in a.os.split(",") if s.strip()], a.report_id, ms, lab, signed)
    tag = a.tag or f"driver-v{a.version}"
    created = ensure_release(a.repo, tag, a.version, a.commit)
    publish_release(a.repo, tag, m, {m["ms_zip_asset"]: ms, m["lab_zip"]: lab, m["signed_zip"]: signed},
                    release_notes(m))
    print(f"{'created' if created else 'updated'} {tag}: {m['signed_zip']} sha256 {m['signed_zip_sha256']}")
    out_path = os.environ.get("GITHUB_OUTPUT")
    if out_path:
        with open(out_path, "a") as f:
            f.write(f"tag={tag}\nsigned_zip={m['signed_zip']}\nsigned_zip_sha256={m['signed_zip_sha256']}\n")
    attest.summary("\n".join([
        f"## Driver {a.version} certified ✅", "", "| | |", "| --- | --- |", f"| Release | `{tag}` |",
        f"| Canonical package | `{m['signed_zip']}` |", f"| SHA256 | `{m['signed_zip_sha256']}` |",
        f"| Source hash | `{m['source_hash']}` |", "",
        "Installer builds bundle this driver while `driver/**` + `include/**` still hash to the source hash.",
    ]))
    return 0


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("hash"); s.add_argument("--root", default="."); s.set_defaults(fn=cmd_hash)
    s = sub.add_parser("fetch")
    s.add_argument("--product-id", required=True); s.add_argument("--submission-id", required=True)
    s.add_argument("--out", required=True); s.set_defaults(fn=cmd_fetch)
    s = sub.add_parser("verify")
    s.add_argument("--ms-zip", required=True); s.add_argument("--lab-zip", required=True)
    s.add_argument("--version"); s.set_defaults(fn=cmd_verify)
    s = sub.add_parser("release")
    s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    s.add_argument("--tag"); s.add_argument("--version", required=True)
    s.add_argument("--commit", required=True); s.add_argument("--source-hash", required=True)
    s.add_argument("--product-id", required=True); s.add_argument("--submission-id", required=True)
    s.add_argument("--os", default="WINDOWS_v100_X64_25H2_FULL", help="comma-separated requestedSignatures")
    s.add_argument("--report-id", default=None)
    s.add_argument("--ms-zip", required=True); s.add_argument("--lab-zip", required=True)
    s.set_defaults(fn=cmd_release)
    a = p.parse_args(argv)
    return a.fn(a) or 0


if __name__ == "__main__":
    sys.exit(main())
