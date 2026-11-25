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
  inputs    check driver-certified.yml's dispatch inputs (INPUT_* env) and
            write the values every later step uses to GITHUB_OUTPUT
  hash      hashFiles('driver/**','include/**') exactly as the Windows
            runners compute it (see hash_files)
  fetch     poll a Partner Center submission until a signedPackage exists,
            download it (needs AZURE_* env like attest.py)
  verify    run checks 1-3 on a Microsoft zip + lab zip
  sys-match check 2 alone on two .sys files (used by driver-attested.yml
            to tie an attestation return to the submitted binary)
  package   verify, then write the canonical -Signed.zip locally (so its
            provenance can be attested before anything is published)
  release   put the canonical -Signed.zip that `package` made (--signed-zip:
            the very file, checked against Microsoft's zip and against the
            digest its provenance bundle names) and the bundle on the
            driver-v<ver> release (creates it when missing), then
            manifest.json, last. A certified release is never changed
            (driver_release.py); the same inputs again change nothing.
Stdlib only; release asset I/O goes through the gh CLI.
"""
import argparse
import hashlib
import io
import json
import os
import re
import struct
import sys
import time
import zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import attest  # noqa: E402
import driver_release  # noqa: E402

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


def pe_same_code(a, b):
    """True when two .sys files are the same code once signatures are
    stripped. Signing pads the image with up to 7 zero bytes so the
    certificate table starts 8-byte aligned; that padding survives the
    strip, so a zero tail shorter than 8 bytes is ignored."""
    sa, sb = pe_strip_signature(a), pe_strip_signature(b)
    if len(sa) < len(sb):
        sa, sb = sb, sa
    tail = sa[len(sb):]
    return sa[:len(sb)] == sb and len(tail) < 8 and not any(tail)


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
    if version:
        from repro_compare import driverver_problem  # repro_compare imports this module
        problem = driverver_problem(ms["streamtospeaker.inf"], version)
        if problem:
            problems.append(f"INF does not carry DriverVer {version}: {problem}")
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


# The "version made by" host recorded for each entry. zipfile takes it
# from the platform it runs on (0 on Windows, 3 elsewhere); pinned to 0, the
# host of every package made on Windows (driver-v1.1.1.211's published
# Signed.zip among them), so the same inputs give the same bytes anywhere.
# A published package keeps the host it was made with (driver-v1.1.0.209's,
# made on Linux, records 3): nothing rebuilds a published package, every
# check compares it with the SHA-256 its manifest records.
ZIP_HOST = 0


def canonical_zip(ms_zip):
    """Flat StreamToSpeaker.{inf,sys,cat} zip of Microsoft's package, with
    fixed timestamps, attributes and host, so the same input always gives
    the same bytes on every platform."""
    ms = read_package(ms_zip)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name in PACKAGE_FILES:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            info.create_system = ZIP_HOST
            z.writestr(info, ms[name.lower()])
    return buf.getvalue()


def signed_zip_problem(signed_zip, ms_zip):
    """None when `signed_zip` is a package of exactly the inf/sys/cat in
    Microsoft's zip, else what is wrong."""
    try:
        got, want = read_package(signed_zip), read_package(ms_zip)
    except (ValueError, zipfile.BadZipFile) as e:
        return f"the canonical package can't be read: {e}"
    differ = [n for n in PACKAGE_FILES if got[n.lower()] != want[n.lower()]]
    if differ:
        return f"the canonical package's {', '.join(differ)} differ from Microsoft's zip"
    return None


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
    """manifest.json for a certified driver-v release. It carries every
    attestation-manifest field the shared consumers read (attest.py,
    Get-AttestedDriver.ps1, the cleanup step), so they read it unchanged;
    the attestation-only build facts (`build_root`, `driver_h_stamped`)
    do not apply to a lab build and are not written."""
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
            # The seller ID in the dashboard's certificationReport URL
            # (DownloadCertificationReport/<seller>/<product>/<submission>);
            # the field name is kept for existing manifests.
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


# Plain names for the dashboard's OS codes in release notes ("v100" is
# Windows version 10.0, shared by Windows 10 and 11).
OS_NAMES = {
    "WINDOWS_v100_X64_25H2_FULL": "Windows 11, version 25H2 (x64)",
}


def os_label(code):
    name = OS_NAMES.get(code)
    return f"{name} (`{code}`)" if name else f"`{code}`"


def report_url(c):
    """The dashboard's public certification report link, or None."""
    if not (c.get("report_id") and c.get("product_id") and c.get("submission_id")):
        return None
    return ("https://partner.microsoft.com/en-us/dashboard/hardware/Driver/DownloadCertificationReport/"
            f"{c['report_id']}/{c['product_id']}/{c['submission_id']}")


def release_notes(m):
    c = m["certification"]
    url = report_url(c)
    return "\n".join([
        f"WHQL-certified driver **{m['driver_version']}** (commit {m['commit']}).",
        "",
        f"`{m['signed_zip']}` is signed by Microsoft: it installs without test-signing mode, with Secure Boot on. "
        "WHQL-certified (HLK) for " + ", ".join(os_label(o) for o in c["os"]) + ".",
        "",
        "| | |",
        "| --- | --- |",
        f"| Driver package | `{m['signed_zip']}` |",
        f"| SHA256 | `{m['signed_zip_sha256']}` |",
        f"| Driver source hash | `{m['source_hash']}` |",
        f"| Partner Center product / submission | `{c['product_id']}` / `{c['submission_id']}` |",
        f"| Certification report | {f'[report]({url})' if url else 'n/a'} |",
        "",
        "Verified in CI: `signtool verify /kp` against the re-issued catalog, a *Microsoft Windows Hardware "
        "Compatibility Publisher* signature on the `.sys` (nested behind the test signature it carried during HLK testing — expected for "
        "HLK submissions), `.sys` code byte-identical to the HLK-tested binary after stripping signatures, "
        "INF byte-identical to the tested one and carrying DriverVer " + m["driver_version"] + ".",
        "",
        "Installer builds bundle this driver automatically while `driver/**` + `include/**` still hash to the "
        f"source hash above, in preference to any attestation-signed build. `{m['lab_zip']}` is the package "
        f"exactly as HLK-tested (test-signed); `{m['ms_zip_asset']}` is the zip Microsoft returned. "
        "Certification is per binary: any driver change needs a new HLK run.",
    ])


def publish_release(release, manifest, files, notes, bundle=None):
    """Put `files` ({name: bytes}: Microsoft's zip, the lab package, the
    canonical Signed.zip) with their .sha256 sidecars and the provenance
    bundle on `release` (a driver_release.DriverRelease), edit its notes,
    and write manifest.json last. Microsoft's zip and the lab package are
    what this run checked: when the release holds them already they must
    be those bytes, and they are not uploaded again. A stopped replacement
    of a file this job replaces (the Signed.zip, a .sha256, the bundle) is
    made again. This job leaves no manifest half written (a certified
    release's is one upload, onto a release that has none) and never
    replaces one of the two inputs, so a pending copy of those stops it."""
    inputs = (manifest["lab_zip"], manifest["ms_zip_asset"])
    sidecars = [f"{name}.sha256" for name in files]
    bundle_name = f"{manifest['signed_zip']}.sigstore.json"
    release.begin(files=[n for n in files if n not in inputs] + sidecars + ([bundle_name] if bundle else []))
    for name, data in files.items():
        if name in inputs and release.rel.asset(name):
            release.require(name, data)
        else:
            release.put(data, name)
    for name, data in files.items():
        release.put(f"{hashlib.sha256(data).hexdigest()}  {name}".encode("ascii"), f"{name}.sha256")
    if bundle:
        release.put_file(bundle, bundle_name)
    release.edit(f"Driver {manifest['driver_version']} — WHQL-certified", notes)
    release.write_manifest(manifest)


def download_asset(repo, tag, name):
    return attest.Release(repo, tag).download_asset(name)


# ---------------------------------------------------------------------------
# Dispatch inputs
# ---------------------------------------------------------------------------
# Each value must have a strict form, matched in full: [0-9] rather than \d
# (which also matches non-ASCII digits), and fullmatch, so a trailing
# newline is refused too. Spaces and tabs around a value (a paste) are
# dropped first, except around the version, which also names the run's
# concurrency group and is refused instead. What passes can't change the
# meaning of a script line or of a GITHUB_OUTPUT line.
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+")
ID_RE = re.compile(r"[0-9]{1,20}")
SIGNATURE_RE = re.compile(r"[A-Za-z0-9_]{1,64}")
ASSET_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,195}\.zip")
COMMIT_RE = re.compile(r"[0-9a-f]{40}")


def check_inputs(env):
    """driver-certified.yml's dispatch inputs, from the INPUT_* variables in
    `env` (and GITHUB_SHA, the dispatched commit): (values, problems).
    `values` holds what every later step of the workflow uses; it is only
    meaningful when `problems` is empty."""
    get = lambda k: env.get(k, "").strip(" \t")
    problems, v = [], {}
    version = env.get("INPUT_VERSION", "")
    if not VERSION_RE.fullmatch(version):
        problems.append(f"version must be MAJOR.MINOR.BUILD.REVISION, got {version!r}")
        version = "<version>"
    v["version"] = version
    tag = get("INPUT_TAG") or f"driver-v{version}"
    if tag != f"driver-v{version}":
        problems.append(f"tag must be empty or driver-v{version}, got {tag!r}")
    v["tag"] = tag
    commit = get("INPUT_COMMIT") or get("GITHUB_SHA")
    if not COMMIT_RE.fullmatch(commit):
        problems.append(f"commit must be a full commit SHA (40 lowercase hex digits), got {commit!r}")
    v["commit"] = commit
    for key, name in (("INPUT_PRODUCT", "product_id"), ("INPUT_SUBMISSION", "submission_id"),
                      ("INPUT_REPORT", "report_id")):
        val = get(key)
        if val and not ID_RE.fullmatch(val):
            problems.append(f"{name} must be a number, got {val!r}")
        v[name] = val
    for key, name, default in (("INPUT_MS_ZIP", "ms_zip", f"StreamToSpeaker-Driver-{version}-Microsoft.zip"),
                               ("INPUT_LAB_ZIP", "lab_zip", f"StreamToSpeaker-Driver-{version}-lab-testsigned.zip")):
        val = get(key)
        if val and not ASSET_RE.fullmatch(val):
            problems.append(f"{name}_asset must be a .zip asset name (letters, digits, . _ + -), got {val!r}")
        v[name] = val or default
    signed = f"StreamToSpeaker-Driver-{version}-Signed.zip"
    # Compared without case: the Windows runner's disk doesn't tell them apart.
    if len({n.lower() for n in (v["ms_zip"], v["lab_zip"], signed)}) < 3:
        problems.append(f"ms_zip_asset, lab_zip_asset and the canonical {signed} must be three different assets, "
                        f"got ms_zip_asset={v['ms_zip']!r}, lab_zip_asset={v['lab_zip']!r}")
    if get("INPUT_MS_ZIP"):
        v["mode"] = "asset"
    elif v["product_id"] and v["submission_id"]:
        v["mode"] = "api"
    else:
        v["mode"] = ""
        problems.append("give product_id + submission_id (Hardware API) or ms_zip_asset "
                        "(hand-downloaded zip on the release)")
    signatures = [s.strip(" \t") for s in get("INPUT_OS").split(",") if s.strip(" \t")]
    bad = [s for s in signatures if not SIGNATURE_RE.fullmatch(s)]
    if bad or not signatures:
        problems.append(f"os must be comma-separated requestedSignatures, got {get('INPUT_OS')!r}")
    v["os"] = ",".join(signatures)
    dry = get("INPUT_DRY_RUN")
    if dry not in ("true", "false"):
        problems.append(f"dry_run must be true or false, got {dry!r}")
    v["dry_run"] = "true" if dry == "true" else "false"
    if v["dry_run"] == "true" and v["mode"] == "api":
        # A dry run holds no credential: it checks a zip on the release.
        problems.append("a dry run reads nothing from Partner Center: download Microsoft's zip from the dashboard, "
                        "attach it to the release and give its name as ms_zip_asset")
    return v, problems


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------
def cmd_inputs(a):
    values, problems = check_inputs(os.environ)
    for p in problems:
        print(f"::error::{p}")
    if problems:
        return 1
    out_path = os.environ.get("GITHUB_OUTPUT")
    lines = "".join(f"{k}={values[k]}\n" for k in sorted(values))
    if out_path:
        with open(out_path, "a", encoding="utf-8") as f:
            f.write(lines)
    print(lines, end="")
    return 0


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


def cmd_sys_match(a):
    with open(a.signed, "rb") as f:
        signed = f.read()
    with open(a.submitted, "rb") as f:
        submitted = f.read()
    try:
        same = pe_same_code(signed, submitted)
    except ValueError as e:
        print(f"::error::.sys could not be parsed: {e}")
        return 1
    if not same:
        print("::error::the returned .sys is not the submitted .sys (code differs after stripping signatures)")
        return 1
    print("returned .sys is the submitted .sys (identical after stripping signatures)")
    return 0


def checked_package(a):
    """verify_packages on --ms-zip / --lab-zip, then the canonical zip:
    (ms, lab, signed), or None after printing the problems."""
    with open(a.ms_zip, "rb") as f:
        ms = f.read()
    with open(a.lab_zip, "rb") as f:
        lab = f.read()
    problems = verify_packages(ms, lab, a.version)
    for p in problems:
        print(f"::error::{p}")
    return None if problems else (ms, lab, canonical_zip(ms))


def cmd_package(a):
    """The canonical Signed.zip, written to --out-dir before anything is
    published, so the provenance step can attest it first."""
    checked = checked_package(a)
    if not checked:
        return 1
    name = f"StreamToSpeaker-Driver-{a.version}-Signed.zip"
    os.makedirs(a.out_dir, exist_ok=True)
    path = os.path.abspath(os.path.join(a.out_dir, name))
    with open(path, "wb") as f:
        f.write(checked[2])
    driver_release.outputs(signed_zip=path, signed_zip_name=name,
                           signed_zip_sha256=hashlib.sha256(checked[2]).hexdigest())
    return 0


def cmd_release(a):
    """Publish the canonical package the verify job made (--signed-zip, the
    file its provenance was made for), never one made again here."""
    checked = checked_package(a)
    if not checked:
        return 1
    ms, lab, _ = checked
    with open(a.signed_zip, "rb") as f:
        signed = f.read()
    problem = signed_zip_problem(signed, ms)
    if problem:
        raise driver_release.ReleaseError(problem)
    if a.bundle:
        from release_draft import bundle_problem  # imports repro_compare, which imports this module
        problem = bundle_problem(a.bundle, os.path.basename(a.signed_zip), hashlib.sha256(signed).hexdigest())
        if problem:
            raise driver_release.ReleaseError(f"not publishing a file its provenance doesn't name: {problem}")
    m = build_manifest(a.version, a.commit, a.source_hash, a.product_id, a.submission_id,
                       [s.strip() for s in a.os.split(",") if s.strip()], a.report_id, ms, lab, signed)
    tag = a.tag or f"driver-v{a.version}"
    release = driver_release.DriverRelease(a.repo, tag)
    if release.view is not None:
        # manifest.json.pending and no manifest.json: another job's stopped
        # write, which this one does not finish; it stops here.
        release.begin_if_interrupted()
    problem = driver_release.certified_problem(tag, release.manifest, m)
    if problem:
        raise driver_release.ReleaseError(problem)
    if release.manifest == m:
        print(f"{tag} is already certified with exactly this manifest; nothing to change")
    else:
        if release.view is None:
            release = driver_release.DriverRelease.create(
                a.repo, tag, a.commit, f"Driver {a.version} — WHQL-certified",
                f"Certified driver {a.version}: assets being published.")
        publish_release(release, m, {m["ms_zip_asset"]: ms, m["lab_zip"]: lab, m["signed_zip"]: signed},
                        release_notes(m), a.bundle)
        print(f"published {tag}: {m['signed_zip']} sha256 {m['signed_zip_sha256']}")
    out_path = os.environ.get("GITHUB_OUTPUT")
    if out_path:
        with open(out_path, "a", encoding="utf-8") as f:
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
    s = sub.add_parser("inputs", help="check driver-certified.yml's dispatch inputs (INPUT_* env)")
    s.set_defaults(fn=cmd_inputs)
    s = sub.add_parser("hash"); s.add_argument("--root", default="."); s.set_defaults(fn=cmd_hash)
    s = sub.add_parser("fetch")
    s.add_argument("--product-id", required=True); s.add_argument("--submission-id", required=True)
    s.add_argument("--out", required=True); s.set_defaults(fn=cmd_fetch)
    s = sub.add_parser("verify")
    s.add_argument("--ms-zip", required=True); s.add_argument("--lab-zip", required=True)
    s.add_argument("--version"); s.set_defaults(fn=cmd_verify)
    s = sub.add_parser("sys-match", help="a signed .sys is the given unsigned/other-signed .sys")
    s.add_argument("--signed", required=True); s.add_argument("--submitted", required=True)
    s.set_defaults(fn=cmd_sys_match)
    s = sub.add_parser("package", help="verify, then write the canonical Signed.zip to --out-dir")
    s.add_argument("--ms-zip", required=True); s.add_argument("--lab-zip", required=True)
    s.add_argument("--version", required=True); s.add_argument("--out-dir", required=True)
    s.set_defaults(fn=cmd_package)
    s = sub.add_parser("release")
    s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    s.add_argument("--signed-zip", required=True,
                   help="the canonical package `package` made, published as it is (its provenance names it)")
    s.add_argument("--bundle", help="provenance bundle of the Signed.zip, published as <Signed.zip>.sigstore.json")
    s.add_argument("--tag"); s.add_argument("--version", required=True)
    s.add_argument("--commit", required=True); s.add_argument("--source-hash", required=True)
    s.add_argument("--product-id", required=True); s.add_argument("--submission-id", required=True)
    s.add_argument("--os", default="WINDOWS_v100_X64_25H2_FULL", help="comma-separated requestedSignatures")
    s.add_argument("--report-id", default=None)
    s.add_argument("--ms-zip", required=True); s.add_argument("--lab-zip", required=True)
    s.set_defaults(fn=cmd_release)
    a = p.parse_args(argv)
    try:
        return a.fn(a) or 0
    except driver_release.ReleaseError as e:
        print(f"::error::{driver_release.command_line(e)}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
