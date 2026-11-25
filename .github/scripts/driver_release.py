#!/usr/bin/env python3
"""Every change the chain makes to a driver-v* release goes through here.

A driver-v* release is the record of one driver binary: manifest.json says
how it was built and what Microsoft did with it, and the files beside it
are what installers bundle and what the release gate compares against.
The release of a driver Microsoft certified or attested can't be made
again. Every job of this repository's workflows that writes a driver
release does it through this file, and test_driver_release.py tests each
rule under its own name:

1. A finished release is never changed. A release is finished when its
   manifest says `certified` or `attested`. Nothing is uploaded to it,
   replaced on it or deleted from it, and its notes are not edited.
   Writing bytes it already holds is not a change, and succeeds.
2. A workflow writes only a release whose current state is its to
   advance (the `*_problem` functions below); anything else is refused,
   naming the release.
3. A file the manifest records (with its SHA-256) is never replaced by
   other bytes.
4. Nothing is deleted before its replacement is completely uploaded, and
   a file already there with the same bytes is not uploaded again
   (release_assets.py). A writer first finishes what a stopped one left
   half done, but only when the release, as those renames would leave
   it, is one a write of these workflows would have made; a
   manifest.json.pending is never read as the manifest.
5. manifest.json is written last, in a call of its own, and only when
   every file it records is on the release.

Each write is decided on the release as it is at that moment: it is read
again just before, and the write is refused when the release is finished,
the tag names another release, or its assets changed since this run read
or wrote them. The workflows run every writing job in one concurrency
group (driver-release-writes), so two of their jobs never write at once.
What these rules can't cover: GitHub has no compare-and-swap for assets,
so an actor outside that group (a person in the web UI, or a run of an
older version of these workflows from a branch or tag that still carries
it) can still change the release in the moment between that last read
and the write; and older versions of the workflows, started from the
refs that carry them, don't follow these rules at all
(docs/driver-signing.md, "Start these workflows from main only").

Releases are always listed with every page.

Commands (each workflow step that writes a driver release runs one):

  submission-guard    Driver submission: is a WHQL-certified driver already
                      released for this driver source?  --source-hash H [--force]
  submission-check    Driver submission, before the build: may this run
                      write --tag (with --commit, --version)?
  submission-release  Driver submission: create the release (if needed)
                      and record the unsigned build in manifest.json.
  submission-signed   Driver submission, sign-cab: add the EV-signed CAB
                      and record its hash.
  submission-cleanup  Driver submission: delete older unsigned builds of
                      main that nothing was ever done with (keep_reason);
                      every other older release is kept and named.
  attested-fetch      Driver attested: pick the release, download the
                      returned zip and the submitted CAB (hash-checked).
  attested-record     Driver attested: add the canonical Signed.zip (and
                      its provenance bundle), then mark the release attested.
  fallback-check      build.yml, a tag with no Microsoft-signed driver for
                      its source hash: stop when a certified or attested
                      driver was built from the same driver/ + include/
                      trees (the hash changed, not the driver). Reads only.

Stdlib only; gh with GH_TOKEN does the requests.
"""
import argparse
import copy
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import release_assets  # noqa: E402
from release_assets import PENDING, ReleaseError, clean, command_line, gh, sha256  # noqa: E402

TAG_RE = re.compile(r"driver-v([0-9]+)\.([0-9]+)\.([0-9]+)\.([0-9]+)")
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+")
SHA_RE = re.compile(r"[0-9a-f]{64}")
COMMIT_RE = re.compile(r"[0-9a-f]{40}")
MANIFEST = "manifest.json"
MAIN = "refs/heads/main"


# ---------------------------------------------------------------------------
# What a manifest says
# ---------------------------------------------------------------------------
def version_of(tag):
    """(a, b, c, d) of a driver-v<a.b.c.d> tag, or None."""
    m = TAG_RE.fullmatch(tag or "")
    return tuple(int(g) for g in m.groups()) if m else None


def finished(m):
    """A release whose manifest says certified or attested is finished."""
    return isinstance(m, dict) and (bool(m.get("certified")) or bool(m.get("attested")))


def kind(m):
    return "certified" if isinstance(m, dict) and m.get("certified") else "attested"


def submission(m):
    """Whether the manifest is Driver submission's (an attestation round)."""
    return isinstance(m, dict) and bool(m.get("submission_cab"))


def recorded_files(m):
    """{asset name: sha256 or None} of the files `m` says its release holds."""
    out = {}
    if not isinstance(m, dict):
        return out
    for name_key, sha_key in (("signed_zip", "signed_zip_sha256"), ("lab_zip", "lab_zip_sha256"),
                              ("ms_zip_asset", "ms_zip_sha256")):
        if isinstance(m.get(name_key), str) and m[name_key]:
            out[m[name_key]] = m.get(sha_key)
    if m.get("submission_cab_signed_sha256") and isinstance(m.get("submission_cab"), str):
        out[m["submission_cab"]] = m["submission_cab_signed_sha256"]
    return out


# ---------------------------------------------------------------------------
# Who may write what (rule 2). Each returns why not, or None.
# ---------------------------------------------------------------------------
def finished_problem(tag, m):
    if finished(m):
        article = "an" if kind(m) == "attested" else "a"
        return (f"{tag} is {article} {kind(m)} driver release; a finished driver release is never changed "
                "(docs/driver-signing.md)")
    return None


def submission_build_problem(tag, view, m, commit):
    """Driver submission's build-cab may write `tag`: absent, or this
    commit's own submission whose CAB nobody has EV-signed or submitted."""
    if view is None:
        return None
    if finished_problem(tag, m):
        return finished_problem(tag, m)
    if view.get("draft"):
        return f"{tag} is a draft release; Driver submission makes none, so it is not this run's"
    if m is None:
        usable = [a for a in view.get("assets") or [] if a.get("state") == "uploaded"]
        if not usable and view.get("target_commitish") == commit:
            # An earlier attempt of this commit stopped right after creating
            # it, or before its first manifest was completely uploaded.
            return None
        if any(a.get("name") == MANIFEST + PENDING for a in usable):
            return (f"{tag} holds manifest.json.pending and no manifest.json: the write of its manifest was "
                    "interrupted; re-run the job that was writing it, which finishes the write first")
        return f"{tag} exists and holds no manifest.json of commit {commit}: it is not this run's to write"
    if not submission(m):
        return f"{tag} is not a Driver submission release (its manifest names no submission CAB)"
    if m.get("commit") != commit:
        return (f"{tag} holds the driver of commit {clean(m.get('commit'))}, not of {commit}: "
                "a driver build number names one binary")
    if m.get("submission_cab_signed_sha256"):
        return f"{tag} already holds the EV-signed CAB of this commit; there is nothing to build again"
    if m.get("ms_submission_id"):
        return f"{tag} has a Partner Center submission ({clean(m.get('ms_submission_id'))})"
    return None


def submission_numbering_problem(tag, releases):
    """A new driver build is numbered above every driver release there is,
    and its build number names no other release's driver."""
    new = version_of(tag)
    others = [r["tag_name"] for r in releases if version_of(r.get("tag_name")) and r["tag_name"] != tag]
    higher = sorted((t for t in others if version_of(t) >= new), key=version_of)
    if higher:
        return (f"{tag} would number this driver below {higher[-1]}, which already exists. Every new driver "
                f"build must be numbered above every driver release, so versions only go up and build=N in "
                f"the service log names one binary. Dispatch Driver submission with driver_build above "
                f"{version_of(higher[-1])[3]} (docs/driver-signing.md, \"Driver build numbers\").")
    same = [t for t in others if version_of(t)[3] == new[3]]
    if same:
        return (f"build {new[3]} already names the driver of {same[0]}; build=N in the service log must name "
                f"one binary. Dispatch Driver submission with another driver_build.")
    return None


def submission_signed_problem(tag, view, m, commit, cab_sha256, signed_sha256):
    """sign-cab may record its signed CAB: the release records exactly the
    unsigned CAB this run built and no signed one yet (or this one)."""
    if view is None:
        return f"there is no release {tag}"
    if finished_problem(tag, m):
        return finished_problem(tag, m)
    if not submission(m):
        return f"{tag} holds no Driver submission manifest"
    if m.get("commit") != commit:
        return f"{tag} holds the driver of commit {clean(m.get('commit'))}, not of {commit}"
    if m.get("submission_cab_sha256") != cab_sha256:
        return (f"{tag} records the unsigned CAB {clean(m.get('submission_cab_sha256'))}, not the one this run "
                f"had signed ({cab_sha256}): a later build of this commit replaced it")
    recorded = m.get("submission_cab_signed_sha256")
    if recorded and recorded != signed_sha256:
        return f"{tag} already records another EV-signed CAB ({clean(recorded)})"
    if m.get("ms_submission_id"):
        return f"{tag} has a Partner Center submission ({clean(m.get('ms_submission_id'))})"
    return None


def attested_problem(tag, m):
    """Driver attested may finish an attestation round Microsoft returned."""
    if finished_problem(tag, m):
        return finished_problem(tag, m)
    if not submission(m) or not m.get("submission_cab_signed_sha256"):
        return f"{tag} is not an attestation submission with an EV-signed CAB"
    return None


def attest_problem(tag, m):
    """Driver attest may advance an unfinished attestation round."""
    if finished_problem(tag, m):
        return finished_problem(tag, m)
    if not submission(m):
        return f"{tag} is not an attestation submission (its manifest names no submission CAB)"
    return None


def certified_problem(tag, m, new):
    """Driver certified may write a release prepared by hand for an HLK
    return, until it is certified (the same manifest again changes nothing)."""
    if m is not None and m == new:
        return None
    if finished_problem(tag, m):
        return finished_problem(tag, m) + "; this run's manifest differs from the one it holds"
    if submission(m):
        return (f"{tag} is an attestation submission (its manifest names a submission CAB); a certified "
                "driver gets a release of its own")
    if m is not None:
        return f"{tag} holds a manifest.json that is neither a certified one nor this run's"
    return None


# The fields an unsigned build's manifest has (submission-release writes
# them; older ones lack build_root and driver_h_stamped).
UNSIGNED_BUILD_FIELDS = {"schema", "driver_version", "driver_prefix", "driver_build", "commit", "source_hash",
                         "build_root", "driver_h_stamped", "submission_cab", "submission_cab_sha256",
                         "submission_cab_signed_sha256", "attested", "signed_zip", "signed_zip_sha256"}
UNSIGNED_BUILD_NEEDS = {"driver_version", "driver_build", "commit", "submission_cab", "submission_cab_sha256"}


def keep_reason(view, m):
    """None when the cleanup may delete this older release: an unsigned
    build that nothing was ever done with, shown by what it holds (exactly
    a completely uploaded manifest.json) and by its manifest (only the
    fields an unsigned build writes; nothing signed, submitted or
    attested; the commit it was built from is its target). Otherwise why
    it is kept. (Whether that commit is in main's history is checked
    separately.)"""
    if view.get("draft"):
        return "a draft"
    assets = view.get("assets") or []
    names = sorted(a.get("name") for a in assets)
    if names != [MANIFEST]:
        return f"holds {', '.join(clean(n) for n in names) or 'no file'}, not only manifest.json"
    if assets[0].get("state") != "uploaded":
        return "its manifest.json was never completely uploaded"
    if not isinstance(m, dict):
        return "no readable manifest.json"
    unknown = sorted(set(m) - UNSIGNED_BUILD_FIELDS)
    if unknown:
        return f"its manifest records {', '.join(clean(k) for k in unknown)}"
    missing = sorted(UNSIGNED_BUILD_NEEDS - set(m))
    if missing:
        return f"its manifest lacks {', '.join(missing)}"
    if m.get("submission_cab_signed_sha256") or m.get("attested") or m.get("signed_zip") or m.get("signed_zip_sha256"):
        return "its manifest records a signature or a signed package"
    if not (isinstance(m.get("commit"), str) and COMMIT_RE.fullmatch(m["commit"])):
        return "its manifest names no commit"
    if view.get("target_commitish") != m["commit"]:
        return f"its target ({clean(view.get('target_commitish'))}) is not the commit it records"
    return None


# ---------------------------------------------------------------------------
# One driver release
# ---------------------------------------------------------------------------
class DriverRelease:
    """One driver-v* release under the rules in the module docstring. Every
    write is decided on the release as it is at that moment: immediately
    before it the release is read again, and the write is refused when the
    release is finished, when the tag names another release now, or when
    its assets changed since this object last read or wrote them
    (release_assets.Release). Its manifest is the manifest.json asset with
    that asset id, so an unchanged asset list means an unchanged manifest."""

    def __init__(self, repo, tag, run=gh, sleep=time.sleep, workdir=None):
        if not version_of(tag):
            raise ReleaseError(f"{clean(tag)!r} is not a driver-v<a.b.c.d> tag")
        self.repo, self.tag, self.run, self.sleep, self.workdir = repo, tag, run, sleep, workdir
        self.rel = release_assets.Release.find(repo, tag, run, sleep=sleep, workdir=workdir)
        self._manifest = self._read_manifest()
        self.healed = []

    @property
    def view(self):
        return self.rel.view if self.rel else None

    @property
    def manifest(self):
        """A copy of what the release's manifest.json holds (None without a
        completely uploaded one): changing it changes nothing until
        write_manifest. A manifest.json.pending is never read as it."""
        return copy.deepcopy(self._manifest)

    @property
    def interrupted(self):
        """manifest.json is missing (or never finished uploading) and a
        complete manifest.json.pending is there: a write of the manifest
        stopped between the delete and the rename."""
        return bool(self.rel) and not self.rel.complete(MANIFEST) and bool(self.rel.complete(MANIFEST + PENDING))

    def _read_manifest(self):
        if not self.rel or not self.rel.complete(MANIFEST):
            return None
        try:
            m = json.loads(self.rel.download(MANIFEST).decode("utf-8-sig"))
        except ValueError:
            raise ReleaseError(f"the manifest.json of {self.tag} is not valid JSON")
        if not isinstance(m, dict):
            raise ReleaseError(f"the manifest.json of {self.tag} is not a JSON object")
        return m

    def download(self, name):
        return self.rel.download(name)

    def _same_release(self, doing):
        if not self.rel:
            raise ReleaseError(f"there is no release {self.tag}")
        by_tag = json.loads(self.run("api", f"repos/{self.repo}/releases/tags/{self.tag}"))
        if by_tag.get("id") != self.rel.view.get("id"):
            raise ReleaseError(f"the tag {self.tag} names another release now; not {doing}")

    def _current(self, doing):
        """Immediately before a write (see the class docstring)."""
        self._same_release(doing)
        self.rel.check_unchanged(doing)
        problem = finished_problem(self.tag, self._manifest)
        if problem:
            raise ReleaseError(f"{problem}; not {doing}")

    def begin(self):
        """Before the first write: finish what a stopped writer left half
        done. A complete `<name>.pending` is rolled forward (a manifest.json
        one whatever it says, last), a broken one deleted; this completes
        writes an earlier run had decided. It happens only when the
        release, as these renames would leave it, is one a write of these
        workflows would have made (_finish_plan); otherwise nothing is
        changed and a person has to look. A finished release is not
        touched at all."""
        if not self.rel:
            raise ReleaseError(f"there is no release {self.tag}")
        if finished(self._manifest):
            return []
        self._same_release("finishing an interrupted write")
        problem, drop = self._finish_plan()
        if problem:
            raise ReleaseError(
                f"{self.tag} holds .pending files that a write of these workflows would not have left ({problem}); "
                "nothing was changed. A person has to look at it (docs/driver-signing.md, \"A release shows an asset "
                "named <name>.pending\")")
        healed = self.rel.finish_interrupted(drop=drop, last={MANIFEST})
        for line in healed:
            print(f"{self.tag}: {line}")
        if healed:
            self._manifest = self._read_manifest()
        self.healed += healed
        return healed

    def _finish_plan(self):
        """(problem or None, names whose pending copy is dropped), checked
        against the release as finishing the interrupted writes would leave
        it, before anything is written:
          - a complete manifest.json.pending is a JSON object that still
            records every file manifest.json records, with the same SHA-256;
          - every file the manifest that ends up in place records is there
            after the renames, completely uploaded, in the bytes it records;
          - a pending copy of a recorded file has exactly those bytes and
            is either the only copy (it is renamed) or the same bytes as
            the file (it is deleted), so none is left beside the file. Any
            other copy would replace a recorded file, which a write of
            these workflows never does, and the release is left for a
            person."""
        rel = self.rel
        pending = {a["name"][:-len(PENDING)]: a for a in rel.assets()
                   if (a.get("name") or "").endswith(PENDING) and a.get("state") == "uploaded"}
        if not pending:
            return None, set()
        current = recorded_files(self._manifest)
        final = self._manifest
        if MANIFEST in pending:
            try:
                final = json.loads(rel.download(MANIFEST + PENDING).decode("utf-8-sig"))
            except (ValueError, UnicodeDecodeError):
                return "manifest.json.pending is not valid JSON", set()
            if not isinstance(final, dict):
                return "manifest.json.pending is not a JSON object", set()
            new = recorded_files(final)
            for name, digest in current.items():
                if name not in new:
                    return f"manifest.json.pending no longer records {clean(name)}, which manifest.json records", set()
                if digest and str(new[name] or "").lower() != str(digest).lower():
                    return f"manifest.json.pending records {clean(name)} with another sha256", set()
        recorded = dict(current, **recorded_files(final))
        drop = set()
        for name, digest in recorded.items():
            source = name + PENDING if name in pending else name
            if not rel.complete(source):
                return f"the manifest would record {clean(name)}, which the release does not hold", set()
            if name in pending and not digest:
                return f"{clean(name)}.pending would replace {clean(name)}, which the manifest records", set()
            after = sha256(rel.download(source))
            if digest and after != str(digest).lower():
                return (f"{clean(source)} does not have the sha256 {clean(digest)} the manifest records for "
                        f"{clean(name)}"), set()
            if name in pending and rel.complete(name):
                if sha256(rel.download(name)) != after:
                    return f"{clean(name)}.pending would replace {clean(name)}, which the manifest records", set()
                drop.add(name)
        return None, drop

    def begin_if_interrupted(self):
        """Finish an interrupted manifest write before what the release
        records is judged (nothing else is touched)."""
        if self.interrupted:
            self.begin()

    def put(self, data, name):
        """Rules 1, 3 and 4 for one file."""
        if not self.rel:
            raise ReleaseError(f"there is no release {self.tag}")
        if self.rel.complete(name) and self.rel.holds(name, data):
            return "unchanged"
        self._current(f"writing {name}")
        recorded = recorded_files(self._manifest)
        if name in recorded and self.rel.asset(name):
            raise ReleaseError(f"the manifest of {self.tag} records {name} (sha256 {clean(recorded[name])}); "
                               "a recorded file is never replaced")
        return self.rel.put(data, name)

    def put_file(self, path, name=None):
        with open(path, "rb") as fh:
            return self.put(fh.read(), name or os.path.basename(path))

    def require(self, name, data):
        """The release must already hold `data` as `name` (an input this
        run read from it, such as a hand-attached package)."""
        if not self.rel or not self.rel.holds(name, data):
            raise ReleaseError(f"{self.tag} does not hold {name} with the bytes this run checked")

    def edit(self, title, notes):
        self._current("editing its notes")
        self.rel.edit(title, notes)

    def write_manifest(self, m):
        """Rules 1 and 5, then rule 4 for manifest.json itself."""
        if not self.rel:
            raise ReleaseError(f"there is no release {self.tag}")
        if self._manifest == m and self.rel.complete(MANIFEST):
            # The manifest this object knows is m; it is still the
            # release's when nothing changed since.
            self.rel.check_unchanged("writing manifest.json")
            return "unchanged"
        self._current("writing manifest.json")
        missing = [n for n in recorded_files(m) if not self.rel.complete(n)]
        if missing:
            raise ReleaseError(f"manifest.json would record {', '.join(missing)}, which {self.tag} does not hold")
        data = (json.dumps(m, indent=2) + "\n").encode("ascii")
        result = self.rel.put(data, MANIFEST)
        # The record of what this run wrote, for the case where something
        # outside the writers' group replaced it a moment later.
        summary(f"<details><summary>manifest.json written to {self.tag}</summary>\n\n```json\n"
                f"{data.decode('ascii').rstrip()}\n```\n\n</details>")
        # A copy: a caller that goes on changing its dict must not change
        # what this object knows the release holds.
        self._manifest = json.loads(data)
        return result

    def delete(self):
        self._current("deleting the release")
        self.rel.delete()

    @classmethod
    def create(cls, repo, tag, target, title, notes, run=gh, sleep=time.sleep, workdir=None):
        release_assets.Release.create(repo, tag, target, title, notes, run=run, sleep=sleep, workdir=workdir)
        return cls(repo, tag, run=run, sleep=sleep, workdir=workdir)


def driver_releases(repo, run=gh):
    """Every driver-v* release (not drafts), newest first."""
    rels = [r for r in release_assets.list_releases(repo, run)
            if version_of(r.get("tag_name")) and not r.get("draft")]
    return sorted(rels, key=lambda r: r.get("created_at") or "", reverse=True)


def manifests(repo, rels, run=gh):
    """(release, manifest) for each release that has a manifest.json. A
    manifest that can't be read raises: no caller may take it for
    "no manifest"."""
    out = []
    for r in rels:
        names = {a.get("name") for a in r.get("assets") or [] if a.get("state") == "uploaded"}
        if MANIFEST not in names:
            if MANIFEST + PENDING in names:
                raise ReleaseError(
                    f"{r['tag_name']} holds manifest.json.pending and no manifest.json: the write of its manifest "
                    "was interrupted, so what the release records can't be read. Re-run the job that was writing "
                    "it (docs/driver-signing.md, \"A release shows an asset named <name>.pending\")")
            continue
        rel = release_assets.Release(repo, r, run)
        try:
            m = json.loads(rel.download(MANIFEST).decode("utf-8-sig"))
        except (ReleaseError, ValueError) as e:
            raise ReleaseError(f"the manifest.json of {r['tag_name']} cannot be read ({clean(e)})")
        out.append((r, m))
    return out


def unfinished_submission(repo, run=gh, commit=None, need=None):
    """The newest driver release (by creation) that is an unfinished
    attestation round, optionally of `commit` and passing `need(release)`;
    (tag, manifest) or (None, None)."""
    for r in driver_releases(repo, run):
        if need and not need(r):
            continue
        found = manifests(repo, [r], run)
        if not found:
            continue
        ((_, m),) = found
        if finished(m) or not submission(m) or (commit and m.get("commit") != commit):
            continue
        return r["tag_name"], m
    return None, None


# ---------------------------------------------------------------------------
# Words people read: release notes and run summaries, from one place
# ---------------------------------------------------------------------------
def attest_next_step(tag, auto_attest):
    if auto_attest:
        return ("**Driver attest** starts by itself when Driver submission finishes (the repository variable "
                "`AUTO_ATTEST` was `true` when this release was made). It submits the signed CAB to Microsoft, "
                "attaches the zip Microsoft returns and marks this driver attested. If it goes red, run it from "
                f"`main` with `tag` = `{tag}`: it resumes where it stopped.")
    return ("Run **Driver attest** from `main` (Actions → Driver attest → Run workflow) with `tag` = "
            f"`{tag}`. It does not start by itself: the repository variable `AUTO_ATTEST` was not `true` when "
            "this release was made. It submits the signed CAB to Microsoft, attaches the zip Microsoft returns "
            "and marks this driver attested. If it goes red, run it again: it resumes where it stopped.")


def submission_notes(tag, version, commit, source_hash, cab_sha256, auto_attest):
    return "\n".join([
        f"Microsoft attestation submission for driver **{version}** (commit {commit}).",
        "",
        f"Driver source hash: `{source_hash}`",
        "",
        "**Status: awaiting Microsoft attestation.** Runbook: `docs/driver-signing.md`, Flow A.",
        "",
        "1. Approve the EV-signing run for the submission CAB in the signing repo. Its run name must show "
        f"sha256 `{cab_sha256}`. CI then adds the EV-signed CAB to this release.",
        f"2. {attest_next_step(tag, auto_attest)}",
    ])


def signed_summary(tag, cab_name, signed_sha256, auto_attest):
    return "\n".join([
        "## Submission CAB EV-signed",
        "",
        f"`{cab_name}` on release `{tag}` is the EV-signed CAB (sha256 `{signed_sha256}`).",
        "",
        f"**Next:** {attest_next_step(tag, auto_attest)}",
    ])


def attested_notes(m, version):
    return "\n".join([
        f"Microsoft-attested driver **{version}** (commit {m.get('commit')}).",
        "",
        f"Install `StreamToSpeaker-Driver-{version}-Signed.zip` on stock Windows 10 1809+ / Windows 11 — Secure "
        "Boot on, no test-signing mode. (Attestation-signed drivers do not load on Windows Server 2016+.)",
        "",
        "| | |",
        "| --- | --- |",
        f"| Driver package | `{m['signed_zip']}` |",
        f"| SHA256 | `{m['signed_zip_sha256']}` |",
        f"| Driver source hash | `{m.get('source_hash')}` |",
        f"| Partner Center product / submission | `{m.get('ms_product_id')}` / `{m.get('ms_submission_id')}` |",
        "",
        "Verified in CI: `signtool verify /kp` against the catalog and the embedded `.sys` signature, signer chain "
        "*Microsoft Windows Hardware Compatibility Publisher*, INF byte-identical to the submitted one and "
        f"carrying DriverVer {version}, `.sys` identical to the submitted one after stripping signatures.",
        "",
        "Installer builds bundle this driver automatically while `driver/**` + `include/**` still hash to the "
        "source hash above. The other assets here are the submission trail: the EV-signed CAB sent to "
        "Microsoft, and the zip Microsoft returned.",
    ])


def summary(text):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(text + "\n")


def outputs(**kv):
    path = os.environ.get("GITHUB_OUTPUT")
    lines = "".join(f"{k}={v}\n" for k, v in kv.items())
    if any("\n" in str(v) or "\r" in str(v) for v in kv.values()):
        raise ReleaseError("a step output may not contain a line break")
    if path:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(lines)
    print(lines, end="")


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------
def require_main(ref):
    if ref != MAIN:
        raise ReleaseError(f"driver releases are written only by runs on main, not {clean(ref)}; "
                           "a run from another branch builds the CAB and stops")


def cmd_submission_guard(a, run):
    if not SHA_RE.fullmatch(a.source_hash or ""):
        raise ReleaseError("--source-hash must be 64 lowercase hex digits (hashFiles output)")
    hit = None
    for r, m in manifests(a.repo, driver_releases(a.repo, run), run):
        if m.get("certified") and m.get("source_hash") == a.source_hash:
            hit = r["tag_name"]
            break
    if hit and not a.force:
        print(f"::notice::Certified driver {hit} already matches driver source hash {a.source_hash}: no "
              "attestation build or submission. Installers bundle the certified package. (Dispatch with "
              "force=true to build anyway.)")
        outputs(proceed="false")
    else:
        print(f"force=true: building although certified driver {hit} matches this source." if hit else
              "No certified driver matches this driver source: proceeding with the attestation build.")
        outputs(proceed="true")


def cmd_submission_check(a, run):
    require_main(a.ref)
    if not COMMIT_RE.fullmatch(a.commit or ""):
        raise ReleaseError("--commit must be a full commit SHA")
    dr = DriverRelease(a.repo, a.tag, run, sleep=a.sleep)
    problem = submission_build_problem(a.tag, dr.view, dr.manifest, a.commit) or \
        submission_numbering_problem(a.tag, driver_releases(a.repo, run))
    if problem:
        raise ReleaseError(problem)
    print(f"{a.tag}: {'an earlier attempt of this commit, not signed yet' if dr.view else 'new'}")


def cmd_submission_release(a, run):
    require_main(a.ref)
    for value, pattern, what in ((a.commit, COMMIT_RE, "--commit"), (a.source_hash, SHA_RE, "--source-hash"),
                                 (a.cab_sha256, SHA_RE, "--cab-sha256"), (a.version, VERSION_RE, "--version")):
        if not pattern.fullmatch(value or ""):
            raise ReleaseError(f"{what} is malformed: {clean(value)!r}")
    if a.tag != f"driver-v{a.version}" or not a.version.startswith(a.prefix + ".") or \
            a.version != f"{a.prefix}.{a.build}":
        raise ReleaseError(f"tag {clean(a.tag)}, version {clean(a.version)}, prefix {clean(a.prefix)} and build "
                           f"{clean(a.build)} do not agree")
    release_assets.check_name(a.cab_name)
    auto = a.auto_attest == "true"
    dr = DriverRelease(a.repo, a.tag, run, sleep=a.sleep)
    if dr.view is not None:
        dr.begin_if_interrupted()
    problem = submission_build_problem(a.tag, dr.view, dr.manifest, a.commit) or \
        submission_numbering_problem(a.tag, driver_releases(a.repo, run))
    if problem:
        raise ReleaseError(problem)
    title = f"Driver {a.version} — awaiting Microsoft attestation"
    notes = submission_notes(a.tag, a.version, a.commit, a.source_hash, a.cab_sha256, auto)
    if dr.view is None:
        dr = DriverRelease.create(a.repo, a.tag, a.commit, title, notes, run=run, sleep=a.sleep)
    else:
        dr.begin()
        dr.edit(title, notes)
    manifest = {
        "schema": 1,
        "driver_version": a.version,
        "driver_prefix": a.prefix,
        "driver_build": int(a.build),
        "commit": a.commit,
        "source_hash": a.source_hash,
        # How this binary was built, for driver-reproducibility.yml: the
        # linker embeds the PDB path, so a rebuild has to happen in the
        # same directory, with driver.h stamped the same way.
        "build_root": a.build_root,
        "driver_h_stamped": True,
        "submission_cab": a.cab_name,
        "submission_cab_sha256": a.cab_sha256,
        "submission_cab_signed_sha256": None,
        "attested": False,
        "signed_zip": None,
        "signed_zip_sha256": None,
    }
    dr.write_manifest(manifest)
    print(f"{a.tag} records the unsigned build (CAB sha256 {a.cab_sha256}); waiting for its EV signature")


def cmd_submission_signed(a, run):
    require_main(a.ref)
    if not os.path.isfile(a.signed):
        raise ReleaseError(f"no signed CAB at {clean(a.signed)}")
    with open(a.signed, "rb") as fh:
        data = fh.read()
    if sha256(data) != a.signed_sha256:
        raise ReleaseError(f"{clean(a.signed)} does not hash to the signed sha256 {clean(a.signed_sha256)}")
    dr = DriverRelease(a.repo, a.tag, run, sleep=a.sleep)
    if dr.view is not None:
        dr.begin_if_interrupted()
    problem = submission_signed_problem(a.tag, dr.view, dr.manifest, a.commit, a.cab_sha256, a.signed_sha256)
    if problem:
        raise ReleaseError(problem)
    dr.begin()
    name = os.path.basename(a.signed)
    if name != dr.manifest.get("submission_cab"):
        raise ReleaseError(f"the signed file is {clean(name)}, but {a.tag} records {clean(dr.manifest.get('submission_cab'))}")
    dr.put(data, name)
    dr.put(f"{a.signed_sha256}  {name}".encode("ascii"), name + ".sha256")
    dr.write_manifest(dict(dr.manifest, submission_cab_signed_sha256=a.signed_sha256))
    summary(signed_summary(a.tag, name, a.signed_sha256, a.auto_attest == "true"))
    print(f"{a.tag} holds the EV-signed CAB {name} (sha256 {a.signed_sha256})")


def in_history(repo, commit, head, run):
    """Whether `commit` is `head` or one of its ancestors."""
    status = json.loads(run("api", f"repos/{repo}/compare/{commit}...{head}")).get("status")
    return status in ("ahead", "identical")


def cmd_submission_cleanup(a, run):
    """Delete older builds of main that nothing was ever done with
    (keep_reason). Everything else is kept and named in the run summary; a
    read that fails or is incomplete keeps the release."""
    require_main(a.ref)
    current = version_of(a.tag)
    if not current:
        raise ReleaseError(f"{clean(a.tag)!r} is not a driver-v<a.b.c.d> tag")
    if not COMMIT_RE.fullmatch(a.commit or ""):
        raise ReleaseError("--commit must be this run's full commit SHA")
    try:
        rels = driver_releases(a.repo, run)
    except ReleaseError as e:
        print(f"::warning::cleanup deleted nothing: the release list could not be read ({clean(e)})")
        summary("## Older driver builds\n\nNothing deleted: the release list could not be read.")
        return
    kept = []
    for r in rels:
        if r["tag_name"] == a.tag or not version_of(r["tag_name"]) or version_of(r["tag_name"]) >= current:
            continue
        try:
            dr = DriverRelease(a.repo, r["tag_name"], run, sleep=a.sleep)
            reason = keep_reason(dr.view, dr.manifest)
            if reason is None and not in_history(a.repo, dr.manifest["commit"], a.commit, run):
                reason = f"its commit {clean(dr.manifest['commit'])} is not in the history of {a.commit}"
        except ReleaseError as e:
            reason = f"it could not be read ({clean(e, 200)})"
        if reason:
            kept.append((r["tag_name"], reason))
            continue
        print(f"Deleting {r['tag_name']}: an older unsigned build of main that nothing was done with, "
              f"superseded by {a.tag}")
        dr.delete()
    if kept:
        summary("\n".join(["## Older driver builds kept", "", "| Release | Why it is kept |", "| --- | --- |"]
                          + [f"| `{t}` | {why} |" for t, why in kept]))
        for t, why in kept:
            print(f"kept {t}: {why}")


def ms_zips(view):
    """The zips Microsoft returned on an attestation release: every .zip
    except the canonical Signed.zip."""
    return [a["name"] for a in view.get("assets") or [] if (a.get("name") or "").endswith(".zip")
            and not re.fullmatch(r"StreamToSpeaker-Driver-.*-Signed\.zip", a["name"])]


def cmd_attested_fetch(a, run):
    if a.tag:
        if not version_of(a.tag):
            raise ReleaseError(f"unexpected tag input format: {clean(a.tag)}")
        tag = a.tag
    else:
        tag, _ = unfinished_submission(a.repo, run, need=lambda r: bool(ms_zips(r)))
        if not tag:
            raise ReleaseError("no unfinished driver-v* release with an attached Microsoft zip; attach the "
                               "signed zip from Partner Center to its driver-v release first")
    dr = DriverRelease(a.repo, tag, run, sleep=a.sleep)
    if dr.view is None:
        raise ReleaseError(f"release {tag} not found")
    if dr.interrupted:
        raise ReleaseError(f"{tag} holds manifest.json.pending and no manifest.json: an earlier run's last write "
                           "was interrupted. Run Driver attested for it again with dry_run off: it finishes that "
                           "write first")
    m = dr.manifest
    if not a.dry_run:
        problem = attested_problem(tag, m)
        if problem:
            raise ReleaseError(problem)
    if not submission(m) or not SHA_RE.fullmatch(str(m.get("submission_cab_signed_sha256") or "")):
        raise ReleaseError(f"{tag} records no EV-signed submission CAB to compare Microsoft's return with")
    zips = ms_zips(dr.view)
    if len(zips) != 1:
        raise ReleaseError(f"expected exactly one Microsoft-signed zip on {tag}, found {len(zips)}: attach exactly "
                           "the zip Partner Center returned")
    release_assets.check_name(zips[0])
    os.makedirs(a.dir, exist_ok=True)
    cab_name = release_assets.check_name(m["submission_cab"])
    cab = dr.download(cab_name)
    if sha256(cab) != m["submission_cab_signed_sha256"]:
        raise ReleaseError(f"{cab_name} on {tag} hashes to {sha256(cab)}, not to the EV-signed CAB the manifest "
                           f"records ({m['submission_cab_signed_sha256']}): it is not the CAB that was submitted")
    paths = {}
    for name, data in ((cab_name, cab), (zips[0], dr.download(zips[0])),
                       (MANIFEST, (json.dumps(m, indent=2) + "\n").encode())):
        paths[name] = os.path.abspath(os.path.join(a.dir, name))
        with open(paths[name], "wb") as fh:
            fh.write(data)
        print(f"  {name}: {len(data)} bytes, sha256 {sha256(data)}")
    outputs(tag=tag, version=".".join(map(str, version_of(tag))), ms_zip=paths[zips[0]], ms_zip_name=zips[0],
            cab=paths[cab_name])


def cmd_attested_record(a, run):
    dr = DriverRelease(a.repo, a.tag, run, sleep=a.sleep)
    if dr.view is not None:
        dr.begin()
        if dr.healed and finished(dr.manifest):
            # An earlier run had written everything; finishing its last
            # write (the manifest's) finished the release.
            print(f"{a.tag} is attested: an earlier run's last write was interrupted and is finished now")
            return
    problem = attested_problem(a.tag, dr.manifest)
    if problem:
        raise ReleaseError(problem)
    if not dr.rel.asset(a.ms_zip_name):
        raise ReleaseError(f"{a.tag} no longer holds {clean(a.ms_zip_name)}")
    version = ".".join(map(str, version_of(a.tag)))
    zip_name = os.path.basename(a.signed_zip)
    if zip_name != f"StreamToSpeaker-Driver-{version}-Signed.zip":
        raise ReleaseError(f"the canonical package of {a.tag} is StreamToSpeaker-Driver-{version}-Signed.zip, "
                           f"not {clean(zip_name)}")
    with open(a.signed_zip, "rb") as fh:
        data = fh.read()
    digest = sha256(data)
    if a.bundle:
        from release_draft import bundle_problem  # imports repro_compare, which comes back here
        problem = bundle_problem(a.bundle, zip_name, digest)
        if problem:
            raise ReleaseError(f"not publishing a file its provenance doesn't name: {problem}")
    dr.put(data, zip_name)
    dr.put(f"{digest}  {zip_name}".encode("ascii"), zip_name + ".sha256")
    if a.bundle:
        dr.put_file(a.bundle, zip_name + ".sigstore.json")
    m = dict(dr.manifest, attested=True, signed_zip=zip_name, signed_zip_sha256=digest,
             ms_zip_asset=a.ms_zip_name)
    dr.edit(f"Driver {version} — Microsoft-attested", attested_notes(m, version))
    dr.write_manifest(m)
    summary("\n".join([f"## Driver {version} attested ✅", "", "| | |", "| --- | --- |", f"| Release | `{a.tag}` |",
                       f"| Canonical package | `{zip_name}` |", f"| SHA256 | `{digest}` |", "",
                       "Installer builds bundle this driver while `driver/**` + `include/**` still hash to the "
                       "manifest's source_hash."]))
    print(f"{a.tag} is attested: {zip_name} sha256 {digest}")


def driver_trees(repo, commit, run):
    """{"driver": sha, "include": sha} of the commit's top-level trees."""
    tree = json.loads(run("api", f"repos/{repo}/git/trees/{commit}"))
    return {e.get("path"): e.get("sha") for e in tree.get("tree") or [] if e.get("path") in ("driver", "include")}


def cmd_fallback_check(a, run):
    """A tag run's driver lookup found no Microsoft-signed driver for this
    source hash. Before the release falls back to the test-signed driver:
    if a certified or attested driver was built from the same driver/ and
    include/ trees, the hash computation changed, not the driver, and the
    fallback would be wrong. Otherwise say what the newest one records."""
    if not SHA_RE.fullmatch(a.source_hash or "") or not COMMIT_RE.fullmatch(a.commit or ""):
        raise ReleaseError("--source-hash and --commit must be full hex digests")
    signed = [(r, m) for r, m in manifests(a.repo, driver_releases(a.repo, run), run) if finished(m)]
    mine = driver_trees(a.repo, a.commit, run)
    for r, m in signed:
        commit = m.get("commit")
        if not (isinstance(commit, str) and COMMIT_RE.fullmatch(commit)):
            continue
        try:
            theirs = driver_trees(a.repo, commit, run)
        except ReleaseError as e:
            print(f"::warning::could not read the trees of {r['tag_name']}'s commit {commit} ({clean(e)}); "
                  "not compared")
            continue
        if theirs == mine and mine:
            raise ReleaseError(
                f"{r['tag_name']} was built from the same driver/ and include/ trees as this commit (its commit "
                f"{commit}), but records the driver source hash {clean(m.get('source_hash'))}, and this run computed "
                f"{a.source_hash}. The hash computation changed, not the driver (runner image, .gitattributes, "
                "checkout settings): no test-signed fallback. See docs/driver-signing.md, \"Release says "
                "-testsigned\".")
    if signed:
        r, m = signed[0]
        print(f"::warning::No Microsoft-signed driver matches this tag's driver source hash {a.source_hash}. The "
              f"newest {kind(m)} driver release is {r['tag_name']} (driver source hash "
              f"{clean(m.get('source_hash'))}, commit {clean(m.get('commit'))}); driver/ or include/ differ from it.")


def main(argv=None, run=gh, sleep=time.sleep):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    def command(name, fn, *args):
        s = sub.add_parser(name)
        s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
        s.add_argument("--ref", default=os.environ.get("GITHUB_REF"), help="the ref the run is on")
        for arg in args:
            # attested-fetch picks the release itself when no --tag is given.
            s.add_argument(arg, required=not (name == "attested-fetch" and arg == "--tag"))
        s.set_defaults(fn=fn)
        return s

    s = command("submission-guard", cmd_submission_guard, "--source-hash")
    s.add_argument("--force", action="store_true")
    command("submission-check", cmd_submission_check, "--tag", "--commit")
    s = command("submission-release", cmd_submission_release, "--tag", "--commit", "--version", "--prefix",
                "--build", "--source-hash", "--build-root", "--cab-name", "--cab-sha256")
    s.add_argument("--auto-attest", default="false")
    s = command("submission-signed", cmd_submission_signed, "--tag", "--commit", "--cab-sha256", "--signed",
                "--signed-sha256")
    s.add_argument("--auto-attest", default="false")
    command("submission-cleanup", cmd_submission_cleanup, "--tag", "--commit")
    s = command("attested-fetch", cmd_attested_fetch, "--tag", "--dir")
    s.add_argument("--dry-run", action="store_true")
    command("fallback-check", cmd_fallback_check, "--source-hash", "--commit")
    s = command("attested-record", cmd_attested_record, "--tag", "--signed-zip", "--ms-zip-name")
    s.add_argument("--bundle")
    a = ap.parse_args(argv)
    if not a.repo:
        print("::error::no --repo (GITHUB_REPOSITORY)")
        return 1
    a.sleep = sleep
    try:
        a.fn(a, run)
    except ReleaseError as e:
        print(f"::error::{command_line(e)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
