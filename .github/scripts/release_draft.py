#!/usr/bin/env python3
"""Draft releases for build.yml's tag pipeline.

A version is assembled in a draft release and published only when it is
complete. Until then no release or release asset of the version is public
(the tag and the workflow run are): a draft is visible only to accounts
with push access, never to `releases/latest` (the app's update check), the
/releases/latest/download/ links or the public release list.

  release_draft.py ensure  <tag> --repo R (--commit SHA | --target SHA)
      Create the draft for <tag> (generated notes) unless it exists, and
      wait until the release list shows it (the list lags behind a fresh
      release by a moment). --commit: the tag must exist and point at SHA,
      whether the draft is created now or already exists. --target: for a
      tag that does not exist yet (it would be created at SHA on
      publishing). Fails when <tag> is already a published release or has
      more than one release.
  release_draft.py require <tag> --repo R --commit SHA
      <tag> must be exactly one release, a draft, and the tag must point
      at SHA: a run whose tag has since moved to another commit neither
      uploads to the draft nor asks for a signature.
  release_draft.py prune   <tag> --repo R --commit SHA --version V --installer NAME [--provenance]
      Delete setup exe assets an earlier attempt left on the draft under
      another name (a -testsigned installer when NAME bundles a
      Microsoft-signed driver, or the other way round, with their
      sidecars and bundles, and the StreamToSpeakerSetup.exe alias when
      NAME is a -testsigned fallback). Nothing else is touched.
  release_draft.py check   <tag> --repo R --version V --installer NAME --dir DIR
                           --service-sha256 H --installer-sha256 H [--provenance]
      The draft holds exactly the assets of a complete release of V (the
      tag must be v<V>), every one fully uploaded: the service exe and the
      setup exe NAME, each with its .sha256 sidecar (and .sigstore.json
      provenance bundle with --provenance), plus the StreamToSpeakerSetup.exe
      alias and its sidecar unless NAME is a -testsigned fallback. They are
      downloaded into DIR and cross-checked: the service exe and the setup
      exe (and the alias) are exactly the signed bytes this run produced
      (the two hashes), every sidecar names its file and hash, every
      bundle's in-toto subject is its file's SHA-256. Writes the checked
      assets' ids as `asset_ids`. (The workflow then checks every .exe's
      Authenticode signature.)
  release_draft.py publish <tag> --repo R --release-id ID --asset-ids IDS --commit SHA
                           --version V --installer NAME [--provenance]
      Publish that draft, after checking once more that it is the draft
      that was checked, that it still holds exactly the assets that were
      checked (an asset replaced since gets a new id), and that the tag
      exists and points at SHA (publishing a draft whose tag is missing
      would recreate the tag at the release's target commit). It becomes
      the latest release only when no published release (other than a
      pre-release) has a version at least as high, so a release published
      late never takes releases/latest from a newer one. A -testsigned fallback
      NAME is published as a pre-release that is never made latest, so
      releases/latest (the app's update check) and the stable download
      link stay on the last release with a Microsoft-signed driver.

`ensure`, `require` and `check` write `release_id` to GITHUB_OUTPUT. Uses
the gh CLI with GH_TOKEN; drafts are visible only to a token with push
access (contents: write). Stdlib only.
"""
import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from repro_compare import clean_text, command_text, gh_output  # noqa: E402

ALIAS = "StreamToSpeakerSetup.exe"
TAG_RE = re.compile(r"v\d+\.\d+\.\d+[0-9A-Za-z.+-]*")
VERSION_RE = re.compile(r"\d+\.\d+\.\d+[0-9A-Za-z.+-]*")
SHA_RE = re.compile(r"[0-9a-f]{64}")
BUNDLE_MEDIA_TYPE = "application/vnd.dev.sigstore.bundle"


class ReleaseError(Exception):
    pass


def gh(*args):
    r = subprocess.run(["gh", *args], capture_output=True, text=True)
    if r.returncode != 0:
        raise ReleaseError(f"gh {' '.join(args[:2])} failed ({r.returncode}): {clean_text(r.stderr.strip(), 600)}")
    return r.stdout


# ---------------------------------------------------------------------------
# Finding the release
# ---------------------------------------------------------------------------
def list_releases(repo, run=gh):
    out = run("api", "--paginate", f"repos/{repo}/releases?per_page=100", "--jq",
              ".[] | {id, tag_name, draft, prerelease, "
              "assets: [.assets[] | {id, name, size, state}]} | @json")
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def releases_for(repo, tag, run=gh):
    return [r for r in list_releases(repo, run) if r.get("tag_name") == tag]


# The release list can lag behind a release created a moment ago.
LIST_WAITS = (1, 2, 4, 8, 15, 30)


def require_tag_at(repo, tag, commit, run=gh):
    """The tag must exist and point at `commit` (a full sha)."""
    if not re.fullmatch(r"[0-9a-f]{40}", commit or ""):
        raise ReleaseError("a full commit sha is needed to check where the tag points")
    pointed = tag_commit(repo, tag, run)
    if pointed != commit:
        raise ReleaseError(f"the tag {tag} points at {pointed}, not at {commit}, the commit this run builds")


def require_draft(repo, tag, run=gh, sleep=time.sleep, waits=(2, 5, 10), commit=None):
    """The one release carrying `tag`, which must be a draft. A release
    that is not listed yet is looked for again a few times. With `commit`,
    the tag must also point at that commit."""
    if commit is not None:
        require_tag_at(repo, tag, commit, run)
    rels = releases_for(repo, tag, run)
    for w in waits:
        if rels:
            break
        sleep(w)
        rels = releases_for(repo, tag, run)
    if not rels:
        raise ReleaseError(f"there is no release for {tag}")
    if len(rels) > 1:
        raise ReleaseError(f"{len(rels)} releases carry the tag {tag}; delete all but one draft")
    rel = rels[0]
    if rel.get("draft") is not True:
        raise ReleaseError(f"{tag} is already a published release; a published release is never modified")
    return rel


def ensure_draft(repo, tag, commit=None, target=None, run=gh, sleep=time.sleep):
    """(release, created): the draft for `tag`, created when missing."""
    if bool(commit) == bool(target):
        raise ReleaseError("ensure needs exactly one of --commit (existing tag) and --target (new tag)")
    sha = commit or target
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ReleaseError("ensure needs a full commit sha")
    # Also when the draft exists: a run whose tag has moved must not take
    # over the draft of the commit the tag points at now.
    if commit:
        require_tag_at(repo, tag, commit, run)
    rels = releases_for(repo, tag, run)
    if rels:
        return require_draft(repo, tag, run, sleep), False
    created = json.loads(run("api", "--method", "POST", f"repos/{repo}/releases",
                             "-f", f"tag_name={tag}", "-f", f"name={tag}", "-f", f"target_commitish={sha}",
                             "-F", "draft=true", "-F", "generate_release_notes=true"))
    if created.get("draft") is not True or created.get("tag_name") != tag:
        raise ReleaseError(f"creating the draft for {tag} returned an unexpected release")
    for w in (0,) + LIST_WAITS:
        sleep(w)
        rels = releases_for(repo, tag, run)
        if rels:
            break
    if [r.get("id") for r in rels] != [created["id"]]:
        raise ReleaseError(f"the release list shows {len(rels)} release(s) for {tag} instead of the new "
                           f"draft {created['id']} alone")
    return require_draft(repo, tag, run, sleep), True


# ---------------------------------------------------------------------------
# What a complete release holds
# ---------------------------------------------------------------------------
def require_version(tag, version):
    if tag != f"v{version}":
        raise ReleaseError(f"version {clean_text(version)!r} is not the version of the tag {tag}")


def testsigned(installer):
    """Whether the setup exe bundles the test-signed fallback driver."""
    return installer.endswith("-testsigned.exe")


def expected_assets(version, installer, provenance):
    if not VERSION_RE.fullmatch(version or ""):
        raise ReleaseError(f"malformed version {clean_text(version)!r}")
    if installer not in (f"StreamToSpeakerSetup-{version}.exe", f"StreamToSpeakerSetup-{version}-testsigned.exe"):
        raise ReleaseError(f"{clean_text(installer)!r} is not the setup exe of version {version}")
    service = f"stream-to-speaker-{version}.exe"
    names = {service, service + ".sha256", installer, installer + ".sha256"}
    if provenance:
        names |= {service + ".sigstore.json", installer + ".sigstore.json"}
    # The stable download link must never lead to a test-signed fallback.
    if not testsigned(installer):
        names |= {ALIAS, ALIAS + ".sha256"}
    return names


def asset_problems(release, expected):
    """What keeps the release's asset list from being exactly `expected`."""
    assets = release.get("assets") or []
    names = [a.get("name") for a in assets]
    have = set(names)
    problems = [f"missing: {n}" for n in sorted(expected - have)]
    problems += [f"unexpected asset: {clean_text(n)}" for n in sorted(have - expected, key=str)]
    if len(names) != len(have):
        problems.append("duplicate asset names")
    for a in assets:
        if a.get("name") in expected and a.get("state") != "uploaded":
            problems.append(f"not completely uploaded: {a.get('name')} (state {clean_text(a.get('state'))})")
        elif a.get("name") in expected and not a.get("size"):
            problems.append(f"empty: {a.get('name')}")
    return problems


def asset_ids(release, expected):
    """The ids of the release's expected assets, sorted, comma-separated.
    GitHub gives a re-uploaded asset a new id."""
    return ",".join(str(i) for i in sorted(int(a["id"]) for a in release.get("assets") or []
                                           if a.get("name") in expected))


def version_key(tag):
    """Sort key of a v<major>.<minor>.<patch>[suffix] tag, semver-like: a
    pre-release suffix (`-rc.1`, or one without a hyphen) sorts before the
    plain version, its dot-separated parts numerically when numeric; build
    metadata (`+...`) is ignored. None for any other tag."""
    m = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)([0-9A-Za-z.+-]*)", tag or "")
    if not m:
        return None
    pre = m.group(4).split("+", 1)[0]
    pre = pre[1:] if pre.startswith("-") else pre
    tail = (1,) if not pre else (0,) + tuple((0, int(p), "") if p.isdigit() else (1, 0, p)
                                              for p in pre.split("."))
    return (int(m.group(1)), int(m.group(2)), int(m.group(3))) + (tail,)


def newer_releases(releases, tag):
    """Published releases (pre-releases aside) whose version is not lower
    than `tag`'s: while there is one, publishing `tag` must not make it
    the latest release."""
    key = version_key(tag)
    if key is None:
        raise ReleaseError(f"{tag} is not a v<major>.<minor>.<patch> tag")
    tags = [r.get("tag_name") for r in releases
            if r.get("draft") is False and r.get("prerelease") is not True and r.get("tag_name") != tag]
    return sorted((t for t in tags if version_key(t) is not None and version_key(t) >= key), key=version_key)


def stale_setup_assets(release, version, installer, provenance):
    """Setup exe assets (with sidecars and bundles) and alias assets on the
    release that a complete release with `installer` does not hold: what an
    earlier attempt that built another kind of installer left behind."""
    expected = expected_assets(version, installer, provenance)
    setup = re.compile(r"StreamToSpeakerSetup(-" + re.escape(version) + r"(-testsigned)?)?\.exe"
                       r"(\.sha256|\.sigstore\.json)?")
    return [a for a in release.get("assets") or []
            if a.get("name") not in expected and setup.fullmatch(str(a.get("name")))]


def tag_commit(repo, tag, run=gh):
    """The commit the tag points at (annotated tags peeled)."""
    obj = json.loads(run("api", f"repos/{repo}/git/ref/tags/{tag}"))["object"]
    for _ in range(5):
        if obj.get("type") == "commit":
            return obj["sha"]
        if obj.get("type") != "tag":
            break
        obj = json.loads(run("api", f"repos/{repo}/git/tags/{obj['sha']}"))["object"]
    raise ReleaseError(f"the tag {tag} does not resolve to a commit")


def sha256_file(path):
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()


def sidecar_problem(path, name, digest):
    """A .sha256 sidecar is `<hex>  <name>` for exactly its file."""
    try:
        with open(path, "rb") as fh:
            parts = fh.read().decode("ascii").split()
    except (OSError, UnicodeDecodeError) as e:
        return f"{name}.sha256 cannot be read: {clean_text(e)}"
    if len(parts) != 2 or parts[0].lower() != digest or parts[1] != name:
        return f"{name}.sha256 does not say `{digest}  {name}`"
    return None


def bundle_problem(path, name, digest):
    """A provenance bundle must be a Sigstore bundle whose in-toto
    statement names this file's SHA-256 as a subject."""
    try:
        with open(path, "rb") as fh:
            bundle = json.load(fh)
        if not str(bundle.get("mediaType", "")).startswith(BUNDLE_MEDIA_TYPE):
            return f"{name}.sigstore.json is not a Sigstore bundle"
        statement = json.loads(base64.b64decode(bundle["dsseEnvelope"]["payload"], validate=True))
        subjects = statement.get("subject") or []
        digests = {str((s.get("digest") or {}).get("sha256", "")).lower() for s in subjects if isinstance(s, dict)}
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as e:
        return f"{name}.sigstore.json cannot be read as a provenance bundle: {clean_text(e)}"
    if digest not in digests:
        return f"{name}.sigstore.json does not attest this file (no subject with its SHA-256)"
    return None


def file_problems(directory, version, installer, provenance, signed=None):
    """Cross-check the downloaded assets. (problems, {exe name: sha256})
    `signed`: {exe name: sha256} of the bytes this run signed; each of
    those exes (and the alias, as the setup exe) must hash to its value."""
    expected = expected_assets(version, installer, provenance)
    present = set(os.listdir(directory))
    problems = [f"not downloaded: {n}" for n in sorted(expected - present)]
    problems += [f"unexpected file: {clean_text(n)}" for n in sorted(present - expected)]
    if problems:
        return problems, {}
    service = f"stream-to-speaker-{version}.exe"
    exes = [service, installer] + ([ALIAS] if ALIAS in expected else [])
    digests = {}
    for name in exes:
        digests[name] = sha256_file(os.path.join(directory, name))
        p = sidecar_problem(os.path.join(directory, name + ".sha256"), name, digests[name])
        if p:
            problems.append(p)
        if provenance and name != ALIAS:
            p = bundle_problem(os.path.join(directory, name + ".sigstore.json"), name, digests[name])
            if p:
                problems.append(p)
    if ALIAS in digests and digests[ALIAS] != digests[installer]:
        problems.append(f"{ALIAS} is not {installer} byte for byte")
    for name, want in (signed or {}).items():
        if digests.get(name) != want:
            problems.append(f"{name} is not the file this run signed (sha256 {want})")
    return problems, digests


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------
def check(repo, tag, version, installer, provenance, directory, service_sha256, installer_sha256, run=gh):
    require_version(tag, version)
    for h in (service_sha256, installer_sha256):
        if not SHA_RE.fullmatch(h or ""):
            raise ReleaseError("check needs the sha256 of the service exe and of the setup exe this run signed")
    rel = require_draft(repo, tag, run)
    expected = expected_assets(version, installer, provenance)
    problems = asset_problems(rel, expected)
    if problems:
        raise ReleaseError(f"the draft {tag} is not complete: " + "; ".join(problems))
    os.makedirs(directory, exist_ok=True)
    if os.listdir(directory):
        raise ReleaseError(f"{directory} is not empty")
    run("release", "download", tag, "--repo", repo, "--dir", directory)
    signed = {f"stream-to-speaker-{version}.exe": service_sha256, installer: installer_sha256}
    problems, digests = file_problems(directory, version, installer, provenance, signed)
    if problems:
        raise ReleaseError(f"the assets of the draft {tag} do not agree: " + "; ".join(problems))
    return rel, digests


def publish(repo, tag, release_id, checked_ids, commit, version, installer, provenance, run=gh):
    require_version(tag, version)
    rel = require_draft(repo, tag, run)
    if str(rel.get("id")) != str(release_id):
        raise ReleaseError(f"the draft for {tag} is release {rel.get('id')}, not the checked release {release_id}")
    expected = expected_assets(version, installer, provenance)
    problems = asset_problems(rel, expected)
    if problems:
        raise ReleaseError(f"the draft {tag} is not complete: " + "; ".join(problems))
    if asset_ids(rel, expected) != checked_ids:
        raise ReleaseError(f"assets of the draft {tag} were replaced after they were checked")
    if not re.fullmatch(r"[0-9a-f]{40}", commit or ""):
        raise ReleaseError("publish needs the full commit sha the release was built from")
    pointed = tag_commit(repo, tag, run)
    if pointed != commit:
        raise ReleaseError(f"the tag {tag} points at {pointed}, not at {commit} the release was built from")
    # A test-signed fallback needs testsigning mode: a pre-release, never
    # the latest release, so the update check and the stable download link
    # keep pointing at the last release with a Microsoft-signed driver.
    fallback = testsigned(installer)
    # GitHub makes a newly published release the latest one unless told
    # otherwise; a version published after a higher one (a hotfix of an
    # older line, a chain finished late) must not take that place.
    newer = newer_releases(list_releases(repo, run), tag)
    latest = not fallback and not newer
    out = run("api", "--method", "PATCH", f"repos/{repo}/releases/{rel['id']}",
              "-F", "draft=false", "-F", f"prerelease={str(fallback).lower()}",
              "-f", f"make_latest={str(latest).lower()}")
    result = json.loads(out)
    if result.get("draft") is not False or result.get("tag_name") != tag or result.get("prerelease") is not fallback:
        raise ReleaseError(f"publishing {tag} did not take effect")
    return result, newer


def prune(repo, tag, commit, version, installer, provenance, run=gh):
    """Delete what stale_setup_assets names from the draft; their names."""
    require_version(tag, version)
    rel = require_draft(repo, tag, run, commit=commit)
    stale = stale_setup_assets(rel, version, installer, provenance)
    for a in stale:
        run("api", "--method", "DELETE", f"repos/{repo}/releases/assets/{int(a['id'])}")
    return rel, [a["name"] for a in stale]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("command", choices=("ensure", "require", "prune", "check", "publish"))
    ap.add_argument("tag")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--target", help="ensure: commit for a tag that does not exist yet")
    ap.add_argument("--version")
    ap.add_argument("--installer")
    ap.add_argument("--provenance", action="store_true")
    ap.add_argument("--dir")
    ap.add_argument("--release-id")
    ap.add_argument("--asset-ids", help="publish: what check wrote as asset_ids")
    ap.add_argument("--commit", help="ensure / require / prune / publish: the commit the tag must point at")
    ap.add_argument("--service-sha256", help="check: sha256 of the signed service exe this run produced")
    ap.add_argument("--installer-sha256", help="check: sha256 of the signed setup exe this run produced")
    a = ap.parse_args(argv)
    if not TAG_RE.fullmatch(a.tag):
        print(f"::error::{command_text(f'{a.tag} is not a v<major>.<minor>.<patch> tag')}")
        return 1
    try:
        if a.command == "ensure":
            rel, created = ensure_draft(a.repo, a.tag, commit=a.commit, target=a.target)
            print(f"draft release {rel['id']} for {a.tag} ({'created' if created else 'already there'})")
        elif a.command == "require":
            if not a.commit:
                raise ReleaseError("require needs --commit")
            rel = require_draft(a.repo, a.tag, commit=a.commit)
            print(f"draft release {rel['id']} for {a.tag} at {a.commit}")
        elif a.command == "prune":
            if not (a.commit and a.version and a.installer):
                raise ReleaseError("prune needs --commit, --version and --installer")
            rel, removed = prune(a.repo, a.tag, a.commit, a.version, a.installer, a.provenance)
            for name in removed:
                print(f"removed {clean_text(name)}, left on the draft by an earlier attempt")
            if not removed:
                print("no assets of an earlier attempt to remove")
        elif a.command == "check":
            if not (a.version and a.installer and a.dir and a.service_sha256 and a.installer_sha256):
                raise ReleaseError("check needs --version, --installer, --dir, --service-sha256 and --installer-sha256")
            rel, digests = check(a.repo, a.tag, a.version, a.installer, a.provenance, a.dir,
                                 a.service_sha256.lower(), a.installer_sha256.lower())
            print(f"draft release {rel['id']} for {a.tag} is complete:")
            for name, digest in sorted(digests.items()):
                print(f"  {digest}  {name}")
            gh_output(asset_ids=asset_ids(rel, expected_assets(a.version, a.installer, a.provenance)))
        else:
            if not (a.version and a.installer and a.release_id and a.asset_ids and a.commit):
                raise ReleaseError("publish needs --version, --installer, --release-id, --asset-ids and --commit")
            result, newer = publish(a.repo, a.tag, a.release_id, a.asset_ids, a.commit, a.version, a.installer,
                                    a.provenance)
            if testsigned(a.installer):
                kind = "as a pre-release (test-signed driver), not as latest"
            elif newer:
                kind = "not as latest"
                note = f"{a.tag} is published but not made the latest release: {newer[-1]} has a version at least as high"
                print(f"::warning::{command_text(note)}")
            else:
                kind = "as the latest release"
            print(f"published {a.tag} {kind}: {clean_text(result.get('html_url', ''))}")
            return 0
    except ReleaseError as e:
        print(f"::error::{command_text(e)}")
        return 1
    gh_output(release_id=rel["id"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
