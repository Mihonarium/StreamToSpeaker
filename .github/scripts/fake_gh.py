"""A stand-in for the gh CLI over a set of fake GitHub releases, for the
tests of release_assets.py, driver_release.py, attest.py and certified.py.

`FakeReleases(...)` is called the way the scripts call `gh` (`run(*args)`,
returning stdout as text, raising on failure). It keeps each release's
assets as bytes, records every call, can fail or stop a chosen call to
show what a failure leaves behind, and serves the release list a page at
a time when a caller forgets `--paginate`.
"""
import hashlib
import json
import os
import re
import shutil
import subprocess


class GhFailed(Exception):
    """What a failing gh command raises (the scripts' own error classes
    wrap their gh calls; the fake raises this one)."""


class Stop(BaseException):
    """A cancelled job: the run stops in the middle of a call. Not an
    Exception, so no `except Exception` in the code under test catches it."""


PAGE = 100


class FakeReleases:
    def __init__(self, repo="owner/repo", error=GhFailed):
        self.repo = repo
        self.error = error
        self.releases = []
        self.calls = []
        self.next_id = 1000
        # (predicate over args, exception) checked before every call.
        self.failures = []
        # Asset names whose `gh release upload --clobber` fails after gh
        # deleted the old asset and before the new one is up: what gh
        # does when its upload half fails (cli/cli, shared/upload.go).
        self.clobber_upload_fails = set()
        # tag name -> the commit it points at (git/ref/tags)
        self.tag_commits = {}
        self.tags = {}

    # -- building the fake --------------------------------------------------
    def _id(self):
        self.next_id += 1
        return self.next_id

    def add_release(self, tag, assets=None, draft=False, prerelease=True, target="0" * 40,
                    created_at="2026-01-01T00:00:00Z", notes="", title=None):
        rel = {"id": self._id(), "tag_name": tag, "draft": draft, "prerelease": prerelease,
               "created_at": created_at, "target_commitish": target, "assets": [],
               "body": notes, "name": title or tag}
        self.releases.append(rel)
        for name, data in (assets or {}).items():
            self.add_asset(rel, name, data)
        return rel

    def add_asset(self, rel, name, data, state="uploaded"):
        if isinstance(data, (dict, list)):
            data = json.dumps(data).encode()
        rel["assets"].append({"id": self._id(), "name": name, "size": len(data), "state": state,
                              "data": data})

    def release(self, tag):
        found = [r for r in self.releases if r["tag_name"] == tag]
        return found[0] if found else None

    def files(self, tag):
        """{asset name: bytes} of the release."""
        rel = self.release(tag)
        return {a["name"]: a["data"] for a in rel["assets"]} if rel else None

    def manifest(self, tag):
        data = (self.files(tag) or {}).get("manifest.json")
        return None if data is None else json.loads(data)

    def fail_when(self, predicate, exc=None):
        """Make the first call matching `predicate(args)` fail (once)."""
        self.failures.append([predicate, exc])

    # -- what was done ------------------------------------------------------
    def writes(self):
        """The calls that change a release."""
        return [c for c in self.calls if c[:2] in (("release", "upload"), ("release", "create"),
                                                    ("release", "delete"), ("release", "edit"))
                or (c[0] == "api" and len(c) > 2 and c[1] == "--method" and c[2] in ("POST", "PATCH", "DELETE"))]

    @staticmethod
    def upload_files(args):
        """The file paths of a `release upload` call."""
        files, rest = [], list(args[3:])
        while rest:
            a = rest.pop(0)
            if a in ("--repo", "-R"):
                rest.pop(0)
            elif not a.startswith("-"):
                files.append(a)
        return files

    def upload_calls(self):
        """The asset names of each `release upload` call, in order."""
        return [[os.path.basename(p) for p in self.upload_files(c)] for c in self.calls
                if c[:2] == ("release", "upload")]

    def uploaded_names(self):
        return [n for names in self.upload_calls() for n in names]

    # -- gh -------------------------------------------------------------------
    def __call__(self, *args):
        args = tuple(str(a) for a in args)
        self.calls.append(args)
        for f in self.failures:
            if f[0] is not None and f[0](args):
                exc = f[1] or self.error(f"gh {' '.join(args[:2])} failed (1): simulated failure")
                f[0] = None
                raise exc
        if args[0] == "api":
            return self._api(args)
        if args[0] == "release":
            return self._release(args)
        raise AssertionError(f"unexpected gh call {args}")

    @staticmethod
    def _opt(args, name):
        return args[args.index(name) + 1] if name in args else None

    @staticmethod
    def _view(rel):
        return {k: v for k, v in rel.items() if k != "assets"} | {
            "assets": [{k: v for k, v in a.items() if k != "data"} for a in rel["assets"]]}

    def _by_id(self, rid):
        return next((r for r in self.releases if r["id"] == rid), None)

    def _asset_by_id(self, aid):
        for r in self.releases:
            for a in r["assets"]:
                if a["id"] == aid:
                    return r, a
        return None, None

    def _api(self, args):
        method = "GET"
        rest = list(args[1:])
        if rest[:1] == ["--method"]:
            method, rest = rest[1], rest[2:]
        paginate = "--paginate" in rest
        rest = [a for a in rest if a != "--paginate"]
        path = rest[0]
        fields = dict(rest[i + 1].split("=", 1) for i, a in enumerate(rest) if a in ("-f", "-F"))
        m = re.fullmatch(rf"repos/{re.escape(self.repo)}/releases\?per_page=(\d+)", path)
        if m and method == "GET":
            rels = sorted(self.releases, key=lambda r: r["created_at"], reverse=True)
            if not paginate:
                rels = rels[:min(int(m.group(1)), PAGE)]
            if "--jq" in rest:
                return "".join(json.dumps(self._view(r)) + "\n" for r in rels)
            return json.dumps([self._view(r) for r in rels])
        m = re.fullmatch(rf"repos/{re.escape(self.repo)}/releases/(\d+)", path)
        if m:
            rel = self._by_id(int(m.group(1)))
            if rel is None:
                raise self.error("gh api failed (1): Not Found (HTTP 404)")
            if method == "GET":
                return json.dumps(self._view(rel))
            if method == "PATCH":
                for k, v in fields.items():
                    rel[k] = {"true": True, "false": False}.get(v, v) if k in ("draft", "prerelease") else v
                return json.dumps(self._view(rel))
        m = re.fullmatch(rf"repos/{re.escape(self.repo)}/releases/assets/(\d+)", path)
        if m:
            rel, a = self._asset_by_id(int(m.group(1)))
            if a is None:
                raise self.error("gh api failed (1): Not Found (HTTP 404)")
            if method == "DELETE":
                rel["assets"].remove(a)
                return ""
            if method == "PATCH":
                new = fields["name"]
                if any(x["name"] == new for x in rel["assets"] if x is not a):
                    raise self.error("gh api failed (1): Validation Failed: already_exists (HTTP 422)")
                a["name"] = new
                return json.dumps({k: v for k, v in a.items() if k != "data"})
        m = re.fullmatch(rf"repos/{re.escape(self.repo)}/git/ref/tags/(.+)", path)
        if m:
            commit = self.tag_commits.get(m.group(1))
            if commit is None:
                raise self.error("gh api failed (1): Not Found (HTTP 404)")
            return json.dumps({"object": {"type": "commit", "sha": commit}})
        m = re.fullmatch(rf"repos/{re.escape(self.repo)}/git/trees/([0-9a-f]+)", path)
        if m:
            tree = self.tags.get(("tree", m.group(1)))
            if tree is None:
                raise self.error("gh api failed (1): Not Found (HTTP 404)")
            return json.dumps({"tree": [{"path": p, "sha": s, "type": "tree"} for p, s in tree.items()]})
        raise AssertionError(f"unexpected gh api call {args}")

    def _release(self, args):
        cmd, tag = args[1], args[2]
        rel = self.release(tag)
        if cmd == "view":
            if rel is None:
                raise self.error("gh release view failed (1): release not found")
            return json.dumps(self._view(rel))
        if cmd == "download":
            if rel is None:
                raise self.error("gh release download failed (1): release not found")
            pattern, out = self._opt(args, "--pattern"), self._opt(args, "--output")
            hits = [a for a in rel["assets"] if a["name"] == pattern]
            if not hits:
                raise self.error("gh release download failed (1): no assets match the file pattern")
            with open(out, "wb") as fh:
                fh.write(hits[0]["data"])
            return ""
        if cmd == "upload":
            if rel is None:
                raise self.error("gh release upload failed (1): release not found")
            for path in self.upload_files(args):
                name = os.path.basename(path)
                if "--clobber" in args:
                    rel["assets"] = [a for a in rel["assets"] if a["name"] != name]
                    if name in self.clobber_upload_fails:
                        raise self.error(f"gh release upload failed (1): uploading {name}: HTTP 502")
                elif any(a["name"] == name for a in rel["assets"]):
                    raise self.error(f"gh release upload failed (1): asset under the same name already exists: [{name}]")
                with open(path, "rb") as fh:
                    self.add_asset(rel, name, fh.read())
            return ""
        if cmd == "create":
            if rel is not None:
                raise self.error("gh release create failed (1): a release with the same tag name already exists")
            notes_file = self._opt(args, "--notes-file")
            notes = open(notes_file, encoding="utf-8").read() if notes_file else self._opt(args, "--notes") or ""
            self.add_release(tag, prerelease="--prerelease" in args, target=self._opt(args, "--target") or "",
                             created_at="2099-01-01T00:00:%02dZ" % (len(self.releases) % 60),
                             notes=notes, title=self._opt(args, "--title"))
            return ""
        if cmd == "edit":
            if rel is None:
                raise self.error("gh release edit failed (1): release not found")
            notes_file = self._opt(args, "--notes-file")
            if notes_file:
                rel["body"] = open(notes_file, encoding="utf-8").read()
            if "--title" in args:
                rel["name"] = self._opt(args, "--title")
            return ""
        if cmd == "delete":
            if rel is None:
                raise self.error("gh release delete failed (1): release not found")
            self.releases.remove(rel)
            return ""
        raise AssertionError(f"unexpected gh call {args}")


def uploading(name):
    """A fail_when predicate: a `release upload` call that carries `name`."""
    return lambda args: args[:2] == ("release", "upload") and name in [
        os.path.basename(p) for p in FakeReleases.upload_files(args)]


def subprocess_run(fake):
    """A replacement for subprocess.run that sends `gh ...` commands to
    `fake`, for code that runs gh itself."""
    def run(cmd, *args, check=False, capture_output=False, text=False, **kw):
        if list(cmd[:1]) != ["gh"]:
            raise AssertionError(f"unexpected command {cmd}")
        try:
            out, code, err = fake(*cmd[1:]), 0, ""
        except GhFailed as e:
            out, code, err = "", 1, str(e)
        if check and code:
            raise subprocess.CalledProcessError(code, cmd, out, err)
        if not text:
            out, err = out.encode(), err.encode()
        return subprocess.CompletedProcess(cmd, code, out, err)
    return run


def sha(data):
    return hashlib.sha256(data).hexdigest()


def write(directory, name, data):
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, name)
    with open(path, "wb") as fh:
        fh.write(data)
    return path


def rmtree(path):
    shutil.rmtree(path, ignore_errors=True)
