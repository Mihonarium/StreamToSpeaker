import base64
import hashlib
import json
import os
import shutil
import tempfile
import unittest

import release_draft as rd

REPO = "Mihonarium/StreamToSpeaker"
V = "0.1.11"
COMMIT = "c0ffee" + "0" * 34
OTHER = "badc0de" + "0" * 33
INSTALLER = f"StreamToSpeakerSetup-{V}.exe"
SERVICE = f"stream-to-speaker-{V}.exe"


def asset(name, state="uploaded", size=10):
    return {"id": int(hashlib.sha256(name.encode()).hexdigest()[:6], 16), "name": name, "size": size, "state": state}


def release(tag, rid=7, draft=True, names=()):
    return {"id": rid, "tag_name": tag, "draft": draft, "prerelease": False, "assets": [asset(n) for n in names]}


class FakeGh:
    """Stands in for the gh CLI: `releases` is what the API lists; calls
    are recorded; `files` maps asset name -> bytes for downloads."""

    def __init__(self, releases, files=None, tags=None, list_lag=0):
        self.releases = releases
        self.files = files or {}
        # tag name -> commit sha, or ("tag", annotated tag object sha, commit sha)
        self.tags = {"v0.1.11": COMMIT} if tags is None else tags
        self.calls = []
        # How many list calls after a create still miss the new release.
        self.list_lag = list_lag
        self.hidden = []

    def __call__(self, *args):
        self.calls.append(args)
        if args[:2] == ("api", "--paginate"):
            if self.hidden and self.list_lag <= 0:
                self.releases.extend(self.hidden)
                self.hidden = []
            self.list_lag -= 1
            return "".join(json.dumps(r) + "\n" for r in self.releases)
        if args[:3] == ("api", "--method", "POST"):
            fields = dict(a.split("=", 1) for a in args if "=" in a)
            rel = release(fields["tag_name"], rid=99)
            rel["target_commitish"] = fields["target_commitish"]
            self.hidden.append(rel)
            return json.dumps(rel)
        if args[:2] == ("release", "download"):
            directory = args[args.index("--dir") + 1]
            for name, data in self.files.items():
                with open(os.path.join(directory, name), "wb") as fh:
                    fh.write(data)
            return ""
        if args[0] == "api" and "/git/ref/tags/" in args[1]:
            target = self.tags.get(args[1].rsplit("/", 1)[1])
            if target is None:
                raise rd.ReleaseError("gh api failed (1): Not Found (HTTP 404)")
            if isinstance(target, tuple):
                return json.dumps({"object": {"type": "tag", "sha": target[1]}})
            return json.dumps({"object": {"type": "commit", "sha": target}})
        if args[0] == "api" and "/git/tags/" in args[1]:
            sha = args[1].rsplit("/", 1)[1]
            commit = next(t[2] for t in self.tags.values() if isinstance(t, tuple) and t[1] == sha)
            return json.dumps({"object": {"type": "commit", "sha": commit}})
        if args[:3] == ("api", "--method", "DELETE"):
            aid = int(args[3].rsplit("/", 1)[1])
            for r in self.releases:
                r["assets"] = [a for a in r["assets"] if a["id"] != aid]
            return ""
        if args[:3] == ("api", "--method", "PATCH"):
            rid = int(args[3].rsplit("/", 1)[1])
            rel = next(r for r in self.releases if r["id"] == rid)
            fields = dict(a.split("=", 1) for a in args if "=" in a)
            rel["draft"] = fields["draft"] == "true"
            rel["prerelease"] = fields.get("prerelease", "false") == "true"
            return json.dumps({"id": rid, "draft": rel["draft"], "prerelease": rel["prerelease"],
                               "tag_name": rel["tag_name"], "html_url": "https://x"})
        raise AssertionError(f"unexpected gh call {args}")


def bundle_for(data, name):
    statement = {"_type": "https://in-toto.io/Statement/v1",
                 "subject": [{"name": name, "digest": {"sha256": hashlib.sha256(data).hexdigest()}}],
                 "predicateType": "https://slsa.dev/provenance/v1", "predicate": {}}
    return json.dumps({"mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
                       "verificationMaterial": {},
                       "dsseEnvelope": {"payload": base64.b64encode(json.dumps(statement).encode()).decode(),
                                        "payloadType": "application/vnd.in-toto+json", "signatures": []}}).encode()


def signed_hashes(files, installer=INSTALLER):
    """What sign-service and sign-release report for the exes in `files`."""
    return hashlib.sha256(files[SERVICE]).hexdigest(), hashlib.sha256(files[installer]).hexdigest()


def complete_files(installer=INSTALLER, provenance=True):
    svc, setup = b"signed service exe", b"signed setup exe"
    files = {SERVICE: svc, installer: setup,
             SERVICE + ".sha256": f"{hashlib.sha256(svc).hexdigest()}  {SERVICE}".encode(),
             installer + ".sha256": f"{hashlib.sha256(setup).hexdigest()}  {installer}".encode()}
    if provenance:
        files[SERVICE + ".sigstore.json"] = bundle_for(svc, SERVICE)
        files[installer + ".sigstore.json"] = bundle_for(setup, installer)
    if not installer.endswith("-testsigned.exe"):
        files[rd.ALIAS] = setup
        files[rd.ALIAS + ".sha256"] = f"{hashlib.sha256(setup).hexdigest()}  {rd.ALIAS}".encode()
    return files


class FindingTheDraft(unittest.TestCase):
    def test_require_one_draft(self):
        gh = FakeGh([release("v0.1.10", rid=1, draft=False), release("v0.1.11", rid=2)])
        self.assertEqual(rd.require_draft(REPO, "v0.1.11", gh)["id"], 2)

    def test_require_refuses_missing_published_and_ambiguous(self):
        with self.assertRaisesRegex(rd.ReleaseError, "no release"):
            rd.require_draft(REPO, "v0.1.11", FakeGh([release("v0.1.10")]), sleep=lambda s: None)
        with self.assertRaisesRegex(rd.ReleaseError, "already a published release"):
            rd.require_draft(REPO, "v0.1.11", FakeGh([release("v0.1.11", draft=False)]))
        with self.assertRaisesRegex(rd.ReleaseError, "2 releases carry"):
            rd.require_draft(REPO, "v0.1.11", FakeGh([release("v0.1.11", rid=1), release("v0.1.11", rid=2)]))

    def creates(self, gh):
        return [c for c in gh.calls if c[:3] == ("api", "--method", "POST")]

    def test_ensure_creates_once_for_the_tagged_commit(self):
        gh = FakeGh([release("v0.1.10", rid=1, draft=False)])
        rel, created = rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
        self.assertTrue(created)
        self.assertEqual(rel["id"], 99)
        create = self.creates(gh)
        self.assertEqual(len(create), 1)
        for field in ("tag_name=v0.1.11", "draft=true", "generate_release_notes=true", f"target_commitish={COMMIT}"):
            self.assertIn(field, create[0])
        # A re-run finds the draft and creates nothing.
        rel, created = rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
        self.assertFalse(created)
        self.assertEqual(len(self.creates(gh)), 1)

    def test_ensure_waits_for_the_release_list(self):
        waited = []
        gh = FakeGh([], list_lag=3)
        rel, created = rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=waited.append)
        self.assertEqual((rel["id"], created), (99, True))
        self.assertGreaterEqual(len([w for w in waited if w]), 2)
        self.assertEqual(len(self.creates(gh)), 1)

    def test_ensure_fails_when_the_draft_never_shows(self):
        gh = FakeGh([], list_lag=100)
        with self.assertRaisesRegex(rd.ReleaseError, "shows 0 release"):
            rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)

    def test_ensure_refuses_a_tag_at_another_commit_or_missing(self):
        gh = FakeGh([], tags={"v0.1.11": OTHER})
        with self.assertRaisesRegex(rd.ReleaseError, "points at"):
            rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
        gh = FakeGh([], tags={})
        with self.assertRaisesRegex(rd.ReleaseError, "404"):
            rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
        self.assertFalse(self.creates(gh))

    def test_ensure_refuses_an_existing_draft_when_the_tag_moved(self):
        # A run whose tag has since been moved to another commit must not
        # take over the draft of the commit the tag points at now.
        for tags in ({"v0.1.11": OTHER}, {}):
            gh = FakeGh([release("v0.1.11", rid=5)], tags=tags)
            with self.assertRaises(rd.ReleaseError):
                rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
            self.assertFalse(self.creates(gh))
        gh = FakeGh([release("v0.1.11", rid=5)])
        rel, created = rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
        self.assertEqual((rel["id"], created), (5, False))
        self.assertTrue([c for c in gh.calls if c[0] == "api" and "/git/ref/tags/v0.1.11" in c[1]])

    def test_require_with_a_commit_checks_the_tag(self):
        gh = FakeGh([release("v0.1.11", rid=5)])
        self.assertEqual(rd.require_draft(REPO, "v0.1.11", gh, commit=COMMIT)["id"], 5)
        gh = FakeGh([release("v0.1.11", rid=5)], tags={"v0.1.11": OTHER})
        with self.assertRaisesRegex(rd.ReleaseError, "points at"):
            rd.require_draft(REPO, "v0.1.11", gh, commit=COMMIT)
        gh = FakeGh([release("v0.1.11", rid=5)], tags={})
        with self.assertRaisesRegex(rd.ReleaseError, "404"):
            rd.require_draft(REPO, "v0.1.11", gh, commit=COMMIT)
        with self.assertRaisesRegex(rd.ReleaseError, "full commit sha"):
            rd.require_draft(REPO, "v0.1.11", FakeGh([release("v0.1.11", rid=5)]), commit="")

    def test_ensure_with_a_target_for_a_tag_that_does_not_exist_yet(self):
        gh = FakeGh([], tags={})
        rd.ensure_draft(REPO, "v0.0.0-x", target=COMMIT, run=gh, sleep=lambda s: None)
        self.assertIn(f"target_commitish={COMMIT}", self.creates(gh)[0])
        self.assertFalse([c for c in gh.calls if c[0] == "api" and "/git/ref/" in c[1]])

    def test_ensure_needs_exactly_one_full_sha(self):
        for kw in ({}, {"commit": COMMIT, "target": COMMIT}, {"commit": "abc"}, {"target": "main"}):
            with self.assertRaises(rd.ReleaseError):
                rd.ensure_draft(REPO, "v0.1.11", run=FakeGh([]), sleep=lambda s: None, **kw)

    def test_ensure_refuses_a_published_release(self):
        gh = FakeGh([release("v0.1.11", draft=False)])
        with self.assertRaisesRegex(rd.ReleaseError, "already a published release"):
            rd.ensure_draft(REPO, "v0.1.11", commit=COMMIT, run=gh, sleep=lambda s: None)
        self.assertFalse(self.creates(gh))

    def test_require_looks_again_for_a_release_not_listed_yet(self):
        gh = FakeGh([], list_lag=2)
        gh.hidden.append(release("v0.1.11", rid=5))
        self.assertEqual(rd.require_draft(REPO, "v0.1.11", gh, sleep=lambda s: None)["id"], 5)


class ExpectedAssets(unittest.TestCase):
    def test_complete_release(self):
        self.assertEqual(rd.expected_assets(V, INSTALLER, True), {
            SERVICE, SERVICE + ".sha256", SERVICE + ".sigstore.json",
            INSTALLER, INSTALLER + ".sha256", INSTALLER + ".sigstore.json",
            rd.ALIAS, rd.ALIAS + ".sha256"})

    def test_testsigned_fallback_gets_no_stable_alias(self):
        names = rd.expected_assets(V, f"StreamToSpeakerSetup-{V}-testsigned.exe", False)
        self.assertNotIn(rd.ALIAS, names)
        self.assertEqual(len(names), 4)

    def test_installer_must_belong_to_the_version(self):
        for bad in ("StreamToSpeakerSetup-0.1.10.exe", "StreamToSpeakerSetup.exe", "x.exe", ""):
            with self.assertRaises(rd.ReleaseError):
                rd.expected_assets(V, bad, True)
        for bad in ("", "v0.1.11", "0.1", "0.1.11\n", None):
            with self.assertRaises(rd.ReleaseError):
                rd.expected_assets(bad, INSTALLER, True)

    def test_asset_problems(self):
        expected = rd.expected_assets(V, INSTALLER, True)
        rel = release("v0.1.11", names=sorted(expected))
        self.assertEqual(rd.asset_problems(rel, expected), [])
        rel["assets"] = [a for a in rel["assets"] if a["name"] != rd.ALIAS] + [asset("StreamToSpeakerSetup-0.1.11-unsigned.exe")]
        problems = rd.asset_problems(rel, expected)
        self.assertIn(f"missing: {rd.ALIAS}", problems)
        self.assertIn("unexpected asset: StreamToSpeakerSetup-0.1.11-unsigned.exe", problems)
        rel = release("v0.1.11", names=sorted(expected))
        rel["assets"][0]["state"] = "open"
        rel["assets"][1]["size"] = 0
        problems = rd.asset_problems(rel, expected)
        self.assertTrue(any("not completely uploaded" in p for p in problems))
        self.assertTrue(any(p.startswith("empty:") for p in problems))


class Versions(unittest.TestCase):
    def test_order(self):
        order = ["v0.1.9", "v0.1.10", "v0.1.11-rc.1", "v0.1.11-rc.2", "v0.1.11-rc.10", "v0.1.11", "v0.2.0", "v1.0.0"]
        self.assertEqual(sorted(reversed(order), key=rd.version_key), order)
        self.assertEqual(rd.version_key("v0.1.11+build.5"), rd.version_key("v0.1.11"))
        for bad in ("driver-v1.1.1.211", "v1.2", "main", None):
            self.assertIsNone(rd.version_key(bad))

    def test_newer_releases(self):
        rels = [release("v0.1.12", rid=1, draft=False), release("v0.1.10", rid=2, draft=False),
                release("v0.2.0", rid=3), release("driver-v1.1.1.211", rid=4, draft=False)]
        rels.append(dict(release("v0.3.0", rid=5, draft=False), prerelease=True))
        # Drafts, pre-releases and other tags never count.
        self.assertEqual(rd.newer_releases(rels, "v0.1.11"), ["v0.1.12"])
        self.assertEqual(rd.newer_releases(rels, "v0.1.13"), [])
        self.assertEqual(rd.newer_releases(rels, "v0.1.12"), [])
        rels.append(release("v0.1.20", rid=6, draft=False))
        self.assertEqual(rd.newer_releases(rels, "v0.1.11"), ["v0.1.12", "v0.1.20"])


class StaleAssets(unittest.TestCase):
    TESTSIGNED = f"StreamToSpeakerSetup-{V}-testsigned.exe"

    def test_a_testsigned_attempt_left_on_a_draft(self):
        names = sorted(set(complete_files()) | set(complete_files(self.TESTSIGNED)))
        stale = rd.stale_setup_assets(release("v0.1.11", names=names), V, INSTALLER, True)
        self.assertEqual(sorted(a["name"] for a in stale),
                         sorted(n for n in names if n.startswith(self.TESTSIGNED)))

    def test_alias_of_an_earlier_attempt_goes_for_a_testsigned_release(self):
        names = sorted(set(complete_files()) | set(complete_files(self.TESTSIGNED)))
        stale = rd.stale_setup_assets(release("v0.1.11", names=names), V, self.TESTSIGNED, True)
        self.assertEqual(sorted(a["name"] for a in stale),
                         sorted([INSTALLER, INSTALLER + ".sha256", INSTALLER + ".sigstore.json",
                                 rd.ALIAS, rd.ALIAS + ".sha256"]))

    def test_other_assets_are_left_alone(self):
        names = sorted(complete_files()) + ["notes.txt", "StreamToSpeakerSetup-0.1.10.exe",
                                             f"stream-to-speaker-{V}-old.exe"]
        self.assertEqual(rd.stale_setup_assets(release("v0.1.11", names=names), V, INSTALLER, True), [])

    def test_prune_deletes_them_from_the_draft_of_this_commit_only(self):
        names = sorted(set(complete_files()) | set(complete_files(self.TESTSIGNED)))
        gh = FakeGh([release("v0.1.11", rid=2, names=names)])
        rel, removed = rd.prune(REPO, "v0.1.11", COMMIT, V, INSTALLER, True, gh)
        self.assertEqual(len(removed), 3)
        self.assertEqual(sorted(a["name"] for a in gh.releases[0]["assets"]), sorted(complete_files()))
        gh = FakeGh([release("v0.1.11", rid=2, names=names)], tags={"v0.1.11": OTHER})
        with self.assertRaisesRegex(rd.ReleaseError, "points at"):
            rd.prune(REPO, "v0.1.11", COMMIT, V, INSTALLER, True, gh)
        self.assertFalse([c for c in gh.calls if c[:3] == ("api", "--method", "DELETE")])
        gh = FakeGh([release("v0.1.11", rid=2, draft=False, names=names)])
        with self.assertRaisesRegex(rd.ReleaseError, "already a published release"):
            rd.prune(REPO, "v0.1.11", COMMIT, V, INSTALLER, True, gh)
        self.assertFalse([c for c in gh.calls if c[:3] == ("api", "--method", "DELETE")])


class Check(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.dir = os.path.join(self.tmp, "assets")

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def gh_for(self, files, extra_names=()):
        return FakeGh([release("v0.1.10", rid=1, draft=False),
                       release("v0.1.11", rid=2, names=sorted(set(files) | set(extra_names)))], files)

    def check(self, files, directory=None, installer=INSTALLER, provenance=True, tag="v0.1.11", gh=None,
              hashes=None):
        gh = gh or self.gh_for(files)
        hashes = hashes or signed_hashes(files, installer)
        return rd.check(REPO, tag, V, installer, provenance, directory or self.dir, *hashes, run=gh)

    def test_complete_draft_passes(self):
        rel, digests = self.check(complete_files())
        self.assertEqual(rel["id"], 2)
        self.assertEqual(set(digests), {SERVICE, INSTALLER, rd.ALIAS})

    def test_testsigned_draft_without_provenance_passes(self):
        installer = f"StreamToSpeakerSetup-{V}-testsigned.exe"
        self.check(complete_files(installer, provenance=False), installer=installer, provenance=False)

    def test_incomplete_or_extra_assets_fail_before_downloading(self):
        files = complete_files()
        del files[rd.ALIAS + ".sha256"]
        gh = self.gh_for(files, extra_names=[f"StreamToSpeakerSetup-{V}-unsigned.exe"])
        with self.assertRaisesRegex(rd.ReleaseError, "not complete"):
            self.check(files, gh=gh, hashes=("0" * 64, "0" * 64))
        self.assertFalse([c for c in gh.calls if c[:2] == ("release", "download")])

    def check_fails(self, files, pattern, hashes=None):
        directory = tempfile.mkdtemp(dir=self.tmp)
        with self.assertRaisesRegex(rd.ReleaseError, pattern):
            self.check(files, directory, hashes=hashes)

    def test_wrong_sidecar_fails(self):
        files = complete_files()
        files[INSTALLER + ".sha256"] = ("0" * 64 + f"  {INSTALLER}").encode()
        self.check_fails(files, "sha256 does not say")

    def test_sidecar_naming_another_file_fails(self):
        files = complete_files()
        digest = hashlib.sha256(files[INSTALLER]).hexdigest()
        files[rd.ALIAS + ".sha256"] = f"{digest}  {INSTALLER}".encode()
        self.check_fails(files, "StreamToSpeakerSetup.exe.sha256 does not say")

    def test_alias_must_be_the_installer(self):
        files = complete_files()
        files[rd.ALIAS] = b"an older installer"
        files[rd.ALIAS + ".sha256"] = f"{hashlib.sha256(b'an older installer').hexdigest()}  {rd.ALIAS}".encode()
        self.check_fails(files, "not StreamToSpeakerSetup-0.1.11.exe byte for byte")

    def test_bundle_for_another_file_fails(self):
        files = complete_files()
        files[INSTALLER + ".sigstore.json"] = bundle_for(b"unsigned setup exe", INSTALLER)
        self.check_fails(files, "does not attest this file")
        files[INSTALLER + ".sigstore.json"] = b"{}"
        self.check_fails(files, "not a Sigstore bundle")
        files[INSTALLER + ".sigstore.json"] = b"not json"
        self.check_fails(files, "cannot be read as a provenance bundle")

    def test_exes_must_be_what_this_run_signed(self):
        # A complete, consistent set that another run (or anyone with write
        # access) put on the draft is refused.
        mine = complete_files()
        for name, data in ((SERVICE, b"another run's service exe"), (INSTALLER, b"another run's setup exe")):
            files = complete_files()
            files[name] = data
            files[name + ".sha256"] = f"{hashlib.sha256(data).hexdigest()}  {name}".encode()
            files[name + ".sigstore.json"] = bundle_for(data, name)
            if name == INSTALLER:
                files[rd.ALIAS] = data
                files[rd.ALIAS + ".sha256"] = f"{hashlib.sha256(data).hexdigest()}  {rd.ALIAS}".encode()
            self.check_fails(files, f"{name} is not the file this run signed", hashes=signed_hashes(mine))

    def test_signed_hashes_are_required(self):
        for hashes in (("", "0" * 64), ("0" * 64, "abc")):
            with self.assertRaisesRegex(rd.ReleaseError, "sha256 of the service exe"):
                self.check(complete_files(), hashes=hashes)

    def test_downloaded_set_must_match(self):
        files = complete_files()
        gh = self.gh_for(files)
        gh.files = dict(files, **{"extra.txt": b"x"})
        with self.assertRaisesRegex(rd.ReleaseError, "unexpected file: extra.txt"):
            self.check(files, gh=gh)

    def test_target_directory_must_be_empty(self):
        os.makedirs(self.dir)
        with open(os.path.join(self.dir, "leftover"), "w") as fh:
            fh.write("x")
        with self.assertRaisesRegex(rd.ReleaseError, "not empty"):
            self.check(complete_files())

    def test_version_must_be_the_tags(self):
        with self.assertRaisesRegex(rd.ReleaseError, "not the version of the tag"):
            self.check(complete_files(), tag="v0.1.12")

    def test_published_release_is_not_checked(self):
        gh = FakeGh([release("v0.1.11", rid=2, draft=False, names=sorted(complete_files()))], complete_files())
        with self.assertRaisesRegex(rd.ReleaseError, "already a published release"):
            self.check(complete_files(), gh=gh)


class Publish(unittest.TestCase):
    def setUp(self):
        self.names = sorted(complete_files())

    def publish(self, gh, release_id="2", ids=None, commit=COMMIT, version=V, installer=INSTALLER):
        if ids is None:
            rel = next(r for r in gh.releases if r["id"] == int(release_id))
            ids = rd.asset_ids(rel, rd.expected_assets(V, installer, True))
        result, newer = rd.publish(REPO, "v0.1.11", release_id, ids, commit, version, installer, True, gh)
        return result

    def patched(self, gh):
        return [c for c in gh.calls if c[:3] == ("api", "--method", "PATCH")]

    def test_publishes_the_checked_draft_as_latest(self):
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)])
        self.publish(gh)
        patch = self.patched(gh)[0]
        self.assertEqual(patch[3], f"repos/{REPO}/releases/2")
        self.assertIn("draft=false", patch)
        self.assertIn("prerelease=false", patch)
        self.assertIn("make_latest=true", patch)

    def test_older_release_published_later_is_not_made_latest(self):
        # v0.1.12 went out while v0.1.11's chain waited: publishing v0.1.11
        # must leave releases/latest (and the stable link) on v0.1.12.
        gh = FakeGh([release("v0.1.12", rid=1, draft=False), release("v0.1.11", rid=2, names=self.names)])
        self.publish(gh)
        patch = self.patched(gh)[0]
        self.assertIn("draft=false", patch)
        self.assertIn("prerelease=false", patch)
        self.assertIn("make_latest=false", patch)
        # An older published release, a newer draft or a newer pre-release
        # does not stop it from becoming latest.
        newer_pre = dict(release("v0.2.0", rid=4, draft=False), prerelease=True)
        gh = FakeGh([release("v0.1.10", rid=1, draft=False), release("v0.1.12", rid=3), newer_pre,
                     release("v0.1.11", rid=2, names=self.names)])
        self.publish(gh)
        self.assertIn("make_latest=true", self.patched(gh)[0])

    def test_testsigned_fallback_is_a_prerelease_never_latest(self):
        installer = f"StreamToSpeakerSetup-{V}-testsigned.exe"
        gh = FakeGh([release("v0.1.10", rid=1, draft=False),
                     release("v0.1.11", rid=2, names=sorted(complete_files(installer)))])
        result = self.publish(gh, installer=installer)
        patch = self.patched(gh)[0]
        self.assertIn("draft=false", patch)
        self.assertIn("prerelease=true", patch)
        self.assertIn("make_latest=false", patch)
        self.assertNotIn("make_latest=true", patch)
        self.assertIs(result["prerelease"], True)

    def test_refuses_a_result_that_is_not_what_was_asked(self):
        # A release that came out a pre-release (or not one, for the
        # fallback) would be offered, or hidden, wrongly.
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)])
        real = gh.__call__

        def flipped(*args):
            out = real(*args)
            if args[:3] == ("api", "--method", "PATCH"):
                r = json.loads(out)
                r["prerelease"] = not r["prerelease"]
                return json.dumps(r)
            return out
        with self.assertRaisesRegex(rd.ReleaseError, "did not take effect"):
            rd.publish(REPO, "v0.1.11", "2", rd.asset_ids(gh.releases[0], rd.expected_assets(V, INSTALLER, True)),
                       COMMIT, V, INSTALLER, True, flipped)

    def test_annotated_tag_is_peeled(self):
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)], tags={"v0.1.11": ("tag", "ab" * 20, COMMIT)})
        self.publish(gh)
        self.assertEqual(len(self.patched(gh)), 1)

    def test_refuses_another_draft_or_an_incomplete_one(self):
        gh = FakeGh([release("v0.1.11", rid=3, names=self.names)])
        with self.assertRaisesRegex(rd.ReleaseError, "not the checked release"):
            self.publish(gh, ids="1,2")
        names = [n for n in self.names if n != rd.ALIAS]
        gh = FakeGh([release("v0.1.11", rid=2, names=names)])
        with self.assertRaisesRegex(rd.ReleaseError, "not complete"):
            self.publish(gh, ids="1,2")
        self.assertFalse(self.patched(gh))

    def test_refuses_assets_replaced_after_the_check(self):
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)])
        rel = gh.releases[0]
        checked = rd.asset_ids(rel, rd.expected_assets(V, INSTALLER, True))
        # `gh release upload --clobber` deletes the asset and uploads a new one.
        rel["assets"][0]["id"] += 1000
        with self.assertRaisesRegex(rd.ReleaseError, "replaced after they were checked"):
            self.publish(gh, ids=checked)
        self.assertFalse(self.patched(gh))

    def test_refuses_a_tag_that_moved_or_is_gone(self):
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)], tags={"v0.1.11": OTHER})
        with self.assertRaisesRegex(rd.ReleaseError, "points at"):
            self.publish(gh)
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)], tags={})
        with self.assertRaisesRegex(rd.ReleaseError, "404"):
            self.publish(gh)
        for commit in ("", "c0ffee", COMMIT.upper(), "main"):
            gh = FakeGh([release("v0.1.11", rid=2, names=self.names)])
            with self.assertRaisesRegex(rd.ReleaseError, "full commit sha"):
                self.publish(gh, commit=commit)
            self.assertFalse(self.patched(gh))

    def test_refuses_a_version_that_is_not_the_tags(self):
        gh = FakeGh([release("v0.1.11", rid=2, names=self.names)])
        with self.assertRaisesRegex(rd.ReleaseError, "not the version of the tag"):
            self.publish(gh, version="0.1.12")
        self.assertFalse(self.patched(gh))

    def test_refuses_a_published_release(self):
        gh = FakeGh([release("v0.1.11", rid=2, draft=False, names=self.names)])
        with self.assertRaisesRegex(rd.ReleaseError, "already a published release"):
            self.publish(gh, ids="1")


class Cli(unittest.TestCase):
    def test_bad_tag_is_refused_before_any_call(self):
        for tag in ("main", "driver-v1.1.1.211", "v1.2", "v1.2.3\n::x"):
            self.assertEqual(rd.main(["require", tag, "--repo", REPO]), 1)

    def test_missing_arguments(self):
        self.assertEqual(rd.main(["check", "v0.1.11", "--repo", REPO]), 1)
        self.assertEqual(rd.main(["check", "v0.1.11", "--repo", REPO, "--version", V, "--installer", INSTALLER,
                                  "--dir", "d"]), 1)  # no signed hashes
        self.assertEqual(rd.main(["require", "v0.1.11", "--repo", REPO]), 1)  # no --commit
        self.assertEqual(rd.main(["prune", "v0.1.11", "--repo", REPO, "--commit", COMMIT]), 1)
        self.assertEqual(rd.main(["publish", "v0.1.11", "--repo", REPO, "--version", V]), 1)
        self.assertEqual(rd.main(["publish", "v0.1.11", "--repo", REPO, "--version", V, "--installer", INSTALLER,
                                  "--release-id", "2", "--asset-ids", "1,2"]), 1)  # no --commit


if __name__ == "__main__":
    unittest.main()
