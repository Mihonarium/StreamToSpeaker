#!/usr/bin/env python3
"""Draft releases for build.yml's tag pipeline.

A version is assembled in a draft release and published only when it is
complete. Until then no release or release asset of the version is public
(the tag and the workflow run are): a draft is visible only to accounts
with push access, never to `releases/latest` (the app's update check), the
/releases/latest/download/ links or the public release list.

  release_draft.py version
      The version build.yml builds, from GITHUB_REF, GITHUB_EVENT_NAME,
      INPUT_VERSION and GITHUB_SHA: a v* tag is its version, a dispatch
      elsewhere may name one, anything else is 0.1.0-<short sha>. Only
      <major>.<minor>.<patch> with an optional -<pre-release> part passes
      (no +build metadata, nothing naming testsigned). Writes `version`
      and `viversion` (X.Y.Z.0) to GITHUB_OUTPUT.
  release_draft.py ensure  <tag> --repo R (--commit SHA | --target SHA)
      Create the draft for <tag> (generated notes) unless it exists, and
      wait until the release list shows it (the list lags behind a fresh
      release by a moment). --commit: the tag must exist and point at SHA,
      whether the draft is created now or already exists. The notes are
      generated for the commit and end with a line, hidden when GitHub
      shows them, that names the commit and holds their SHA-256. The
      notes of a draft that exists are replaced in one case only: they
      are still exactly the notes generated for another commit (the tag
      was moved since an earlier run made the draft). Notes a person
      changed are never replaced; when they were written for another
      commit, a warning says so. --target: for a tag that does not exist
      yet (it would be created at SHA on publishing). Fails when <tag> is
      already a published release or has more than one release.
  release_draft.py upload  <tag> --repo R --commit SHA --version V FILE...
      Put signed files on the draft (as `require` checks it), each .exe
      with its .sha256 sidecar, the setup exe also as the stable
      StreamToSpeakerSetup.exe alias unless it is a -testsigned fallback.
      Nothing on the draft is deleted before its replacement is completely
      uploaded (release_assets.py).
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
      late never takes releases/latest from a newer one; when another tag
      was published meanwhile with a higher version, that one gets latest
      back. A -testsigned fallback NAME, and a version with a pre-release
      part (0.1.11-rc.1), is published as a pre-release that is never made
      latest, so releases/latest (the app's update check) and the stable
      download link stay on the last full release.

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
import release_assets  # noqa: E402
from repro_compare import clean_text, command_text, gh_output  # noqa: E402

ALIAS = "StreamToSpeakerSetup.exe"
# <major>.<minor>.<patch>, optionally `-` and a pre-release suffix of
# dot-separated letters, digits and hyphens (semver). No build metadata
# (`+`, which GitHub may rename in asset names) and nothing naming
# testsigned (the -testsigned installer suffix is the build's to add).
VERSION_PATTERN = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
VERSION_RE = re.compile(VERSION_PATTERN)
TAG_RE = re.compile("v" + VERSION_PATTERN)
SHA_RE = re.compile(r"[0-9a-f]{64}")
BUNDLE_MEDIA_TYPE = "application/vnd.dev.sigstore.bundle"


# One error class for this script and the uploads it makes.
ReleaseError = release_assets.ReleaseError


def valid_version(version):
    return bool(isinstance(version, str) and VERSION_RE.fullmatch(version)
                and "testsigned" not in version.lower())


def pre_release(version):
    """Whether the version has a pre-release part (`0.1.11-rc.1`): such a
    release is published as a GitHub pre-release, never as latest."""
    return "-" in version.split("+", 1)[0]


def build_version(ref, event, value, sha):
    """(version, viversion) of a build.yml run: a v* tag is its version (a
    dispatch on a tag must leave the version input empty or give the same);
    a dispatch elsewhere may name one; otherwise 0.1.0-<short sha>.
    viversion is the numeric X.Y.Z.0 the installer's VersionInfo needs."""
    if ref.startswith("refs/tags/v"):
        version = ref[len("refs/tags/v"):]
        if event == "workflow_dispatch" and value and value != version:
            raise ReleaseError("a dispatch on a tag builds the tag's version; leave the version input empty")
    elif event == "workflow_dispatch" and value:
        version = value
    else:
        version = "0.1.0-" + sha[:7]
    if "testsigned" in version.lower():
        raise ReleaseError("the version may not name testsigned: the build adds -testsigned to the installer's "
                           "name itself when it bundles the test-signed driver")
    if not valid_version(version):
        raise ReleaseError(f"{clean_text(version)!r} is not a version this chain can publish: "
                           "<major>.<minor>.<patch>, optionally followed by - and a pre-release part of "
                           "letters, digits, dots and hyphens (from the tag or the version input)")
    return version, ".".join(version.split("-", 1)[0].split(".") + ["0"])


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
              ".[] | {id, tag_name, draft, prerelease, target_commitish, "
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
        raise ReleaseError(f"{tag} is already a published release; the assets and notes of a published release are never changed")
    return rel


# The last line of notes this chain generated. GitHub hides it when it
# shows the notes; a maintainer sees it in the edit box, so it says what it
# is for.
NOTES_LINE = ("<!-- Notes generated for commit {commit} (sha256 {digest}). "
              "Edit freely: the release workflow never replaces notes that were changed. -->")
NOTES_LINE_RE = re.compile(r"<!-- Notes generated for commit ([0-9a-f]{40}) \(sha256 ([0-9a-f]{64})\)\.[^\n]*?-->")


def notes_digest(notes):
    """The SHA-256 of `notes` as a reader sees them: GitHub's editor saves
    CRLF line ends, and blanks at a line's end or around the notes don't
    show."""
    lines = notes.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    return hashlib.sha256("\n".join(line.rstrip() for line in lines).strip().encode("utf-8")).hexdigest()


def with_marker(notes, commit):
    """`notes`, generated for `commit`, with the line that says so."""
    return f"{notes}\n\n{NOTES_LINE.format(commit=commit, digest=notes_digest(notes))}"


def generated_for(body):
    """(commit, unchanged) when a draft's notes `body` carry this chain's
    line: the commit they were generated for, and whether they are still
    exactly the generated ones (the line last, the notes above it as they
    were). None without the line: written by hand, or the line was removed
    or changed."""
    found = list(NOTES_LINE_RE.finditer(body or ""))
    if not found:
        return None
    line = found[-1]
    unchanged = not body[line.end():].strip() and notes_digest(body[:line.start()]) == line.group(2)
    return line.group(1), unchanged


def generated_notes(repo, tag, commit, run=gh):
    """GitHub's generated notes for `tag` at `commit`, with_marker."""
    notes = json.loads(run("api", "--method", "POST", f"repos/{repo}/releases/generate-notes",
                           "-f", f"tag_name={tag}", "-f", f"target_commitish={commit}"))
    return with_marker(notes.get("body") or "", commit)


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
        rel = require_draft(repo, tag, run, sleep)
        full = json.loads(run("api", f"repos/{repo}/releases/{int(rel['id'])}"))
        made = generated_for(full.get("body"))
        recorded = str(full.get("target_commitish") or "")
        if commit and made is None and recorded != commit and re.fullmatch(r"[0-9a-f]{40}", recorded):
            # The notes carry no line that says which commit they were
            # written for (a person wrote them, or deleted the line). What
            # is left to go by is the commit this chain recorded when it
            # made the draft: another one means the tag was moved since.
            # The notes are a person's, so they stay; the run says so once,
            # and records the commit the tag points at now.
            print("::warning::" + command_text(
                f"The draft for {tag} was made for commit {recorded}, and the tag points at {commit} now. Its "
                "release notes are kept as they are: check that they describe this release before it is "
                "published."))
            run("api", "--method", "PATCH", f"repos/{repo}/releases/{int(rel['id'])}",
                "-f", f"target_commitish={commit}")
            rel = require_draft(repo, tag, run, sleep)
        if commit and made and made[0] != commit:
            # The tag was moved since an earlier run made this draft: its
            # notes were generated for another commit. Notes nobody changed
            # are generated again; notes a person changed are theirs.
            fields = ["-f", f"target_commitish={commit}"]
            if made[1]:
                fields += ["-f", f"body={generated_notes(repo, tag, commit, run)}"]
                print(f"the draft's notes were generated for commit {made[0]} and nobody changed them: "
                      f"replaced with notes generated for {commit}")
            else:
                print("::warning::" + command_text(
                    f"The draft's release notes were written for commit {made[0]} and changed by hand. The tag "
                    f"{tag} points at {commit} now. The notes are kept as they are: check that they describe "
                    "this release before it is published."))
            run("api", "--method", "PATCH", f"repos/{repo}/releases/{int(rel['id'])}", *fields)
            rel = require_draft(repo, tag, run, sleep)
        return rel, False
    created = json.loads(run("api", "--method", "POST", f"repos/{repo}/releases",
                             "-f", f"tag_name={tag}", "-f", f"name={tag}", "-f", f"target_commitish={sha}",
                             "-f", f"body={generated_notes(repo, tag, sha, run)}", "-F", "draft=true"))
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
    if not valid_version(version):
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
    # A test-signed fallback needs testsigning mode, and a pre-release
    # version (0.1.11-rc.1) is not for everyone: both are GitHub
    # pre-releases, never the latest release, so the update check and the
    # stable download link keep pointing at the last full release.
    prerelease = testsigned(installer) or pre_release(version)
    # GitHub makes a newly published release the latest one unless told
    # otherwise; a version published after a higher one (a hotfix of an
    # older line, a chain finished late) must not take that place.
    newer = newer_releases(list_releases(repo, run), tag)
    latest = not prerelease and not newer
    out = run("api", "--method", "PATCH", f"repos/{repo}/releases/{rel['id']}",
              "-F", "draft=false", "-F", f"prerelease={str(prerelease).lower()}",
              "-f", f"make_latest={str(latest).lower()}")
    result = json.loads(out)
    if result.get("draft") is not False or result.get("tag_name") != tag or result.get("prerelease") is not prerelease:
        raise ReleaseError(f"publishing {tag} did not take effect")
    if latest:
        # Another tag's chain can publish between the look at the list
        # above and this PATCH, both asking to be latest. Whichever
        # finished last, the highest version gets latest back.
        rels = list_releases(repo, run)
        newer = newer_releases(rels, tag)
        if newer:
            top = next(r for r in rels if r.get("tag_name") == newer[-1])
            run("api", "--method", "PATCH", f"repos/{repo}/releases/{int(top['id'])}", "-f", "make_latest=true")
    return result, newer


def prune(repo, tag, commit, version, installer, provenance, run=gh):
    """Delete what stale_setup_assets names from the draft; their names."""
    require_version(tag, version)
    rel = require_draft(repo, tag, run, commit=commit)
    stale = stale_setup_assets(rel, version, installer, provenance)
    for a in stale:
        run("api", "--method", "DELETE", f"repos/{repo}/releases/assets/{int(a['id'])}")
    return rel, [a["name"] for a in stale]


def upload(repo, tag, commit, version, paths, run=gh, sleep=time.sleep):
    """Put signed files on the draft of `tag` at `commit`
    (release_assets.put: nothing is deleted before its replacement is
    up). Each .exe gets its .sha256 sidecar; the setup exe, unless it is a
    -testsigned fallback, also the stable StreamToSpeakerSetup.exe alias
    and its sidecar (expected_assets decides). Only files a release of
    `version` holds are taken."""
    exes = {f"stream-to-speaker-{version}.exe", f"StreamToSpeakerSetup-{version}.exe",
            f"StreamToSpeakerSetup-{version}-testsigned.exe"}
    for path in paths:
        name = os.path.basename(path)
        if not (name in exes or (name.endswith(".sigstore.json") and name[:-len(".sigstore.json")] in exes)):
            raise ReleaseError(f"{clean_text(name)} is not a file a release of {clean_text(version)} holds")
    view = require_draft(repo, tag, run, sleep, commit=commit)
    rel = release_assets.Release(repo, view, run, sleep=sleep)
    for line in rel.finish_interrupted():
        print(line)
    done = []
    for path in paths:
        name = os.path.basename(path)
        with open(path, "rb") as fh:
            data = fh.read()
        items = [(name, data)]
        if name.endswith(".exe"):
            digest = hashlib.sha256(data).hexdigest()
            items.append((name + ".sha256", f"{digest}  {name}".encode("ascii")))
            if name.startswith("StreamToSpeakerSetup-"):
                if ALIAS in expected_assets(version, name, False):
                    items += [(ALIAS, data), (ALIAS + ".sha256", f"{digest}  {ALIAS}".encode("ascii"))]
                else:
                    print(f"::warning::{name} bundles the test-signed driver: no stable {ALIAS} link on this release")
        for item, item_data in items:
            result = rel.put(item_data, item)
            print(f"{item}: {result}")
            done.append(item)
    return done


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("command", choices=("version", "ensure", "require", "upload", "prune", "check", "publish"))
    ap.add_argument("tag", nargs="?")
    ap.add_argument("files", nargs="*", help="upload: the signed files")
    ap.add_argument("--repo")
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
    if a.command == "version":
        try:
            version, viversion = build_version(os.environ.get("GITHUB_REF", ""), os.environ.get("GITHUB_EVENT_NAME", ""),
                                               os.environ.get("INPUT_VERSION", ""), os.environ.get("GITHUB_SHA", ""))
        except ReleaseError as e:
            print(f"::error::{command_text(e)}")
            return 1
        print(f"version={version}  viversion={viversion}")
        gh_output(version=version, viversion=viversion)
        return 0
    if not a.tag or not TAG_RE.fullmatch(a.tag) or not valid_version(a.tag[1:]):
        print(f"::error::{command_text(f'{a.tag} is not a v<major>.<minor>.<patch> tag this chain can publish')}")
        return 1
    if not a.repo:
        print("::error::--repo is required")
        return 1
    if a.files and a.command != "upload":
        print("::error::only upload takes files")
        return 1
    try:
        if a.command == "upload":
            if not (a.commit and a.version and a.files):
                raise ReleaseError("upload needs --commit, --version and the files")
            require_version(a.tag, a.version)
            upload(a.repo, a.tag, a.commit, a.version, a.files)
            return 0
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
