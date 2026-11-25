#!/usr/bin/env python3
"""Put files on a GitHub release without ever leaving it with less than it had.

gh's `--clobber` upload deletes the old asset and then uploads the new
one, so an upload that fails or is cancelled half way leaves the release
without the file. Every upload the chain makes goes through
`Release.put` instead:

  * a file the release already holds with the same bytes is not uploaded
    again;
  * a new name is uploaded as it is;
  * a replacement goes up as `<name>.pending`; only once it is completely
    uploaded is the old asset deleted and the new one renamed to `<name>`.

A run stopped between that delete and the rename leaves the new bytes as
`<name>.pending`. `Release.finish_interrupted`, which every writer calls
first, renames such an asset (the old one is gone), or deletes it when
the old asset is still there (the replacement never happened).

Stdlib only; gh with GH_TOKEN does the requests.
"""
import hashlib
import json
import os
import re
import shutil
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


class Release:
    """One release, read and written through gh."""

    def __init__(self, repo, view, run=gh, workdir=None, sleep=time.sleep):
        self.repo, self.view, self.run, self.sleep = repo, view, run, sleep
        self.tag = view["tag_name"]
        self.workdir = workdir or tempfile.mkdtemp(prefix="release-assets-")

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
        workdir = kw.pop("workdir", None) or tempfile.mkdtemp(prefix="release-assets-")
        notes_file = os.path.join(workdir, "release-notes.md")
        with open(notes_file, "w", encoding="utf-8") as fh:
            fh.write(notes)
        run("release", "create", tag, "--repo", repo, "--prerelease", "--target", target,
            "--title", title, "--notes-file", notes_file)
        for w in (0,) + LIST_WAITS:
            sleep(w)
            rel = cls.find(repo, tag, run, workdir=workdir, sleep=sleep, **kw)
            if rel:
                return rel
        raise ReleaseError(f"created {tag}, but the release list does not show it")

    def refresh(self):
        self.view = json.loads(self.run("api", f"repos/{self.repo}/releases/{int(self.view['id'])}",
                                        "--jq", ONE_JQ))
        return self.view

    # -- reading --------------------------------------------------------------
    def assets(self):
        return list(self.view.get("assets") or [])

    def asset(self, name):
        hits = [a for a in self.assets() if a.get("name") == name]
        return hits[0] if hits else None

    def download(self, name):
        """The bytes of asset `name`."""
        check_name(name if not name.endswith(PENDING) else name[:-len(PENDING)])
        directory = tempfile.mkdtemp(prefix="download-", dir=self.workdir)
        path = os.path.join(directory, name)
        self.run("release", "download", self.tag, "--repo", self.repo, "--pattern", name,
                 "--output", path)
        with open(path, "rb") as fh:
            data = fh.read()
        shutil.rmtree(directory, ignore_errors=True)
        return data

    def holds(self, name, data):
        """Whether asset `name` is completely uploaded and is `data`."""
        a = self.asset(name)
        return bool(a) and a.get("state") == "uploaded" and a.get("size") == len(data) \
            and sha256(self.download(name)) == sha256(data)

    # -- writing --------------------------------------------------------------
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
                           f"({clean(last)}); re-run the job to finish it")

    def _upload(self, data, name):
        directory = tempfile.mkdtemp(prefix="upload-", dir=self.workdir)
        path = os.path.join(directory, name)
        with open(path, "wb") as fh:
            fh.write(data)
        try:
            self.run("release", "upload", self.tag, path, "--repo", self.repo)
        finally:
            shutil.rmtree(directory, ignore_errors=True)
        self.refresh()
        a = self.asset(name)
        if not a or a.get("state") != "uploaded" or a.get("size") != len(data):
            raise ReleaseError(f"{self.tag}: the upload of {name} did not complete")
        return a

    def finish_interrupted(self, only=None):
        """Finish or undo replacements a stopped run left half done (see
        the module docstring). `only`: just this asset name."""
        changed = []
        for a in self.assets():
            name = a.get("name") or ""
            if not name.endswith(PENDING) or (only and name != only + PENDING):
                continue
            final = name[:-len(PENDING)]
            if self.asset(final):
                self._delete_asset(a)
                changed.append(f"removed {name}, an upload that never replaced {final}")
            else:
                self._rename(a, final)
                changed.append(f"finished replacing {final} (renamed {name})")
        if changed:
            self.refresh()
        return changed

    def put(self, data, name):
        """Make the release hold `data` as `name`: 'unchanged', 'added' or
        'replaced'."""
        check_name(name)
        old = self.asset(name)
        if old and old.get("state") == "uploaded" and self.holds(name, data):
            return "unchanged"
        if old and old.get("state") != "uploaded":
            # A broken upload of an earlier run holds nothing usable.
            self._delete_asset(old)
            self.refresh()
            old = None
        if not old:
            self._upload(data, name)
            return "added"
        stale = self.asset(name + PENDING)
        if stale:
            self._delete_asset(stale)
        new = self._upload(data, name + PENDING)
        self._delete_asset(old)
        self._rename(new, name)
        self.refresh()
        a = self.asset(name)
        if not a or a.get("size") != len(data):
            raise ReleaseError(f"{self.tag}: replacing {name} did not complete")
        return "replaced"

    def put_file(self, path, name=None):
        with open(path, "rb") as fh:
            return self.put(fh.read(), name or os.path.basename(path))

    def edit(self, title, notes):
        notes_file = os.path.join(self.workdir, "release-notes.md")
        with open(notes_file, "w", encoding="utf-8") as fh:
            fh.write(notes)
        self.run("release", "edit", self.tag, "--repo", self.repo, "--title", title, "--notes-file", notes_file)

    def delete(self):
        self.run("release", "delete", self.tag, "--repo", self.repo, "--cleanup-tag", "--yes")
