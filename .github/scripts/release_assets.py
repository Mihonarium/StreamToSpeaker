#!/usr/bin/env python3
"""Put files on a GitHub release without ever leaving it with less than it had.

gh's `--clobber` upload deletes the old asset and then uploads the new
one, so an upload that fails or is cancelled half way leaves the release
without the file. Every upload the chain makes goes through `Release.put`
instead:

  * a file the release already holds with the same bytes is not uploaded
    again;
  * a new name is uploaded as it is;
  * a replacement goes up as `<name>.pending`; only once it is completely
    uploaded is the old asset deleted and the new one renamed to `<name>`.

Every write is decided on the release as it is at that moment: before it,
the release is read again by id, and the write is refused when its assets
(id, name, state, size) are not what this object last saw or made, that is,
when someone else changed the release. An asset's bytes can't change
without a new id, so that list covers the contents too. Between that read
and the write itself (one API call) GitHub offers no lock; the workflows
keep their writers in one concurrency group so that only an actor outside
them can act in that moment.

A run that stops half way leaves `<name>.pending`. `Release.finish_interrupted`,
which writers call first, rolls a completely uploaded one forward (deletes
`<name>` if it is there, renames the pending one: the writer had decided)
and deletes one whose upload never completed (it holds nothing usable).
`put` never deletes a pending asset: one it meets belongs to another writer.

Temporary files live in folders that are removed when each step ends.
Stdlib only; gh with GH_TOKEN does the requests.
"""
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

PENDING = ".pending"
# Asset names the chain uses: no glob characters (gh release download
# takes a pattern), nothing GitHub would rename.
NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,240}")
ONE_JQ = ("{id, tag_name, draft, prerelease, created_at, target_commitish, "
          "assets: [.assets[] | {id, name, size, state}]}")
LIST_JQ = f".[] | {ONE_JQ} | @json"
# A release created a moment ago can be missing from the list for a while.
LIST_WAITS = (1, 2, 4, 8, 15, 30)


class ReleaseError(Exception):
    pass


def clean(value, limit=400):
    """Text from gh or a release as one line the runner can't read as a
    workflow command (repro_compare.clean_text; imported here, since
    repro_compare imports certified, which comes back here)."""
    from repro_compare import clean_text
    return clean_text(value, limit)


def command_line(message):
    """The message of an ::error:: / ::warning:: this chain prints itself."""
    return clean(message, 3000).replace("%", "%25")


def gh(*args):
    r = subprocess.run(["gh", *args], capture_output=True, text=True)
    if r.returncode != 0:
        raise ReleaseError(f"gh {' '.join(args[:2])} failed ({r.returncode}): {clean(r.stderr.strip())}")
    return r.stdout


def list_releases(repo, run=gh):
    """Every release of `repo`, all pages, as the API lists them (newest
    first)."""
    out = run("api", "--paginate", f"repos/{repo}/releases?per_page=100", "--jq", LIST_JQ)
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def check_name(name):
    if not isinstance(name, str) or not NAME_RE.fullmatch(name):
        raise ReleaseError(f"{clean(name)!r} is not an asset name this chain uses "
                           "(letters, digits and . _ + -)")
    return name


def fingerprint(view):
    """What a write checks the release against: its id and every asset's
    id, name, state and size."""
    return (view.get("id"), tuple(sorted((a.get("id"), a.get("name"), a.get("state"), a.get("size"))
                                         for a in view.get("assets") or [])))


class Release:
    """One release, read and written through gh."""

    def __init__(self, repo, view, run=gh, sleep=time.sleep, workdir=None):
        self.repo, self.view, self.run, self.sleep = repo, view, run, sleep
        self.tag = view["tag_name"]
        # Where the temporary folders go (default: the system's).
        self.workdir = workdir
        self._known = fingerprint(view)

    # -- finding it ---------------------------------------------------------
    @classmethod
    def find(cls, repo, tag, run=gh, **kw):
        """The one release carrying `tag`, or None."""
        found = [r for r in list_releases(repo, run) if r.get("tag_name") == tag]
        if len(found) > 1:
            raise ReleaseError(f"{len(found)} releases carry the tag {tag}; delete all but one")
        return cls(repo, found[0], run, **kw) if found else None

    @classmethod
    def create(cls, repo, tag, target, title, notes, run=gh, sleep=time.sleep, **kw):
        """Create `tag` as a pre-release at `target` and wait until the
        release list shows it."""
        with tempfile.TemporaryDirectory(prefix="release-assets-", dir=kw.get("workdir")) as tmp:
            notes_file = os.path.join(tmp, "release-notes.md")
            with open(notes_file, "w", encoding="utf-8") as fh:
                fh.write(notes)
            run("release", "create", tag, "--repo", repo, "--prerelease", "--target", target,
                "--title", title, "--notes-file", notes_file)
        for w in (0,) + LIST_WAITS:
            sleep(w)
            rel = cls.find(repo, tag, run, sleep=sleep, **kw)
            if rel:
                return rel
        raise ReleaseError(f"created {tag}, but the release list does not show it")

    def refresh(self):
        self.view = json.loads(self.run("api", f"repos/{self.repo}/releases/{int(self.view['id'])}",
                                        "--jq", ONE_JQ))
        return self.view

    def _check(self, expect, doing):
        """Read the release again; refuse unless its assets are `expect`."""
        self.refresh()
        now = fingerprint(self.view)
        if now != expect:
            def names(fp):
                return {(n, i) for i, n, _s, _z in fp[1]}
            gone = sorted(n for n, _ in names(expect) - names(now))
            new = sorted(n for n, _ in names(now) - names(expect))
            raise ReleaseError(f"{self.tag} changed since this run last read it (no longer there: "
                               f"{', '.join(gone) or 'nothing'}; new: {', '.join(new) or 'nothing'}; or an "
                               f"upload changed state); not {doing}: another run or a person is writing it")
        return now

    def check_unchanged(self, doing):
        """Before a write: the release is as this object last saw or made it."""
        return self._check(self._known, doing)

    # -- reading --------------------------------------------------------------
    def assets(self):
        return list(self.view.get("assets") or [])

    def asset(self, name):
        hits = [a for a in self.assets() if a.get("name") == name]
        return hits[0] if hits else None

    def complete(self, name):
        """The asset `name` when it is completely uploaded, else None."""
        a = self.asset(name)
        return a if a and a.get("state") == "uploaded" else None

    def download(self, name):
        """The bytes of asset `name`."""
        check_name(name)
        with tempfile.TemporaryDirectory(prefix="release-assets-", dir=self.workdir) as tmp:
            path = os.path.join(tmp, name)
            self.run("release", "download", self.tag, "--repo", self.repo, "--pattern", name,
                     "--output", path)
            with open(path, "rb") as fh:
                return fh.read()

    def holds(self, name, data):
        """Whether asset `name` is completely uploaded and is `data`."""
        a = self.complete(name)
        return bool(a) and a.get("size") == len(data) and sha256(self.download(name)) == sha256(data)

    # -- writing --------------------------------------------------------------
    def _without(self, fp, asset_id):
        return (fp[0], tuple(x for x in fp[1] if x[0] != asset_id))

    def _delete_asset(self, a):
        self.run("api", "--method", "DELETE", f"repos/{self.repo}/releases/assets/{int(a['id'])}")

    def _rename(self, a, name):
        last = None
        for w in (0, 2, 5):
            self.sleep(w)
            try:
                self.run("api", "--method", "PATCH", f"repos/{self.repo}/releases/assets/{int(a['id'])}",
                         "-f", f"name={name}")
                return
            except Exception as e:  # retried; the last failure is raised
                last = e
        raise ReleaseError(f"{self.tag}: {a.get('name')} is uploaded but could not be renamed to {name} "
                           f"({clean(last)}); the next run that writes this release finishes the rename")

    def _upload(self, data, name):
        with tempfile.TemporaryDirectory(prefix="release-assets-", dir=self.workdir) as tmp:
            path = os.path.join(tmp, name)
            with open(path, "wb") as fh:
                fh.write(data)
            self.run("release", "upload", self.tag, path, "--repo", self.repo)
        self.refresh()
        a = self.asset(name)
        if not a or a.get("state") != "uploaded" or a.get("size") != len(data):
            raise ReleaseError(f"{self.tag}: the upload of {name} did not complete")
        return a

    def _after(self, expect, added=None, removed=None, renamed=None, doing=""):
        """Read the release again and require exactly `expect` changed by
        this run's last call (an added asset, a removed id, a rename)."""
        self.refresh()
        now = fingerprint(self.view)
        want = list(expect[1])
        if removed is not None:
            want = [x for x in want if x[0] != removed]
        if renamed is not None:
            want = [(i, renamed[1] if i == renamed[0] else n, st, z) for i, n, st, z in want]
        if added is not None:
            want.append((added["id"], added["name"], added["state"], added["size"]))
        if now != (expect[0], tuple(sorted(want))):
            raise ReleaseError(f"{self.tag} changed while this run was {doing}: another run or a person is "
                               "writing it; stopped")
        return now

    def finish_interrupted(self, keep=()):
        """Finish replacements a stopped run left half done (see the module
        docstring). Names in `keep` are never replaced, and a pending
        upload over one of them is left as it is."""
        changed = []
        known = self.check_unchanged("finishing an interrupted write")
        for a in self.assets():
            name = a.get("name") or ""
            if not name.endswith(PENDING):
                continue
            final = name[:-len(PENDING)]
            if a.get("state") != "uploaded":
                self._delete_asset(a)
                known = self._after(known, removed=a["id"], doing="removing a broken upload")
                changed.append(f"removed {name}, an upload that never completed")
                continue
            old = self.asset(final)
            if old and final in keep:
                changed.append(f"left {name}: {final} is recorded and is not replaced")
                continue
            if old:
                self._delete_asset(old)
                known = self._after(known, removed=old["id"], doing=f"finishing the replacement of {final}")
            self._rename(a, final)
            known = self._after(known, renamed=(a["id"], final), doing=f"finishing the replacement of {final}")
            changed.append(f"finished replacing {final} (an earlier run had uploaded {name})")
        self._known = known
        return changed

    def put(self, data, name):
        """Make the release hold `data` as `name`: 'unchanged', 'added' or
        'replaced'."""
        check_name(name)
        known = self.check_unchanged(f"writing {name}")
        if self.asset(name + PENDING):
            raise ReleaseError(f"{self.tag} holds {name + PENDING}: another run is replacing {name}, or one "
                               "stopped half way (the next run's first step finishes it); not writing it")
        old = self.asset(name)
        if old and old.get("state") == "uploaded" and self.holds(name, data):
            return "unchanged"
        if old and old.get("state") != "uploaded":
            # A broken upload of an earlier run holds nothing usable.
            self._delete_asset(old)
            known = self._after(known, removed=old["id"], doing=f"removing a broken upload of {name}")
            old = None
        if not old:
            new = self._upload(data, name)
            self._known = self._after(known, added=new, doing=f"uploading {name}")
            return "added"
        new = self._upload(data, name + PENDING)
        known = self._after(known, added=new, doing=f"uploading {name}")
        self._delete_asset(old)
        known = self._after(known, removed=old["id"], doing=f"replacing {name}")
        self._rename(new, name)
        self._known = self._after(known, renamed=(new["id"], name), doing=f"replacing {name}")
        return "replaced"

    def put_file(self, path, name=None):
        with open(path, "rb") as fh:
            return self.put(fh.read(), name or os.path.basename(path))

    def edit(self, title, notes):
        self.check_unchanged("editing the notes")
        with tempfile.TemporaryDirectory(prefix="release-assets-", dir=self.workdir) as tmp:
            notes_file = os.path.join(tmp, "release-notes.md")
            with open(notes_file, "w", encoding="utf-8") as fh:
                fh.write(notes)
            self.run("release", "edit", self.tag, "--repo", self.repo, "--title", title, "--notes-file", notes_file)

    def delete(self):
        self.check_unchanged("deleting the release")
        self.run("release", "delete", self.tag, "--repo", self.repo, "--cleanup-tag", "--yes")
