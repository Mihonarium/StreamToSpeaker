"""Checks on the workflow files themselves. Offline, standard library only;
reads .github/ next to this script and skips when that is not checked out.

Every action from outside this repository runs at a fixed commit:
`uses: owner/repo[/path]@<40 hex digits> # <version>`, the version being
the tag the commit was taken from (an update changes both together). A
tag can be moved by whoever controls the action's repository, a commit
SHA cannot. Local actions (./...) run from this repository's own commit.
"""
import os
import re
import unittest

GITHUB_DIR = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir))
WORKFLOWS_DIR = os.path.join(GITHUB_DIR, "workflows")
HAVE_WORKFLOWS = os.path.isdir(WORKFLOWS_DIR)

USES_RE = re.compile(r"^\s*(?:-\s+)?uses:\s*(?P<ref>[^\s#]+)(?P<rest>.*)$")
PINNED_RE = re.compile(r"(?P<repo>[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)(?:/[A-Za-z0-9_.-]+)*@(?P<sha>[0-9a-f]{40})")
VERSION_COMMENT_RE = re.compile(r"\s+#\s+(?P<version>v\d+(?:\.\d+)*)\s*")


def workflow_files():
    """Every workflow file and every action defined in this repository."""
    out = []
    for sub in ("workflows", "actions"):
        for dirpath, _dirs, files in os.walk(os.path.join(GITHUB_DIR, sub)):
            out += [os.path.join(dirpath, f) for f in files if f.endswith((".yml", ".yaml"))]
    return sorted(out)


def uses_lines():
    """(where, ref, rest of the line) for every `uses:` line."""
    for path in workflow_files():
        with open(path, encoding="utf-8") as fh:
            for n, line in enumerate(fh.read().splitlines(), 1):
                m = USES_RE.match(line)
                if m:
                    yield f"{os.path.relpath(path, GITHUB_DIR)}:{n}", m.group("ref"), m.group("rest")


def _depth(line):
    return len(line) - len(line.lstrip(" "))


def _content(line):
    return bool(line.strip()) and not line.lstrip().startswith("#")


def jobs_steps(name):
    """{job id: [step, ...]} of .github/workflows/<name>, each step the list
    of its lines. Read by indentation, which is enough for these block-style
    files: jobs are the keys one level under `jobs:`, steps the `- ` items
    of a job's `steps:` list."""
    with open(os.path.join(WORKFLOWS_DIR, name), encoding="utf-8") as fh:
        lines = fh.read().splitlines()
    jobs, job, job_depth, in_jobs, i = {}, None, None, False, 0
    while i < len(lines):
        line = lines[i]
        i += 1
        if not _content(line):
            continue
        if _depth(line) == 0:
            in_jobs = line.rstrip() == "jobs:"
            continue
        if not in_jobs:
            continue
        job_depth = _depth(line) if job_depth is None else job_depth
        if _depth(line) == job_depth:
            job = line.strip().rstrip(":")
            jobs[job] = []
        elif job is not None and line.strip() == "steps:":
            item = None
            while i < len(lines):
                cur = lines[i]
                if _content(cur):
                    item = _depth(cur) if item is None else item
                    if _depth(cur) < item or (_depth(cur) == item and not cur.lstrip().startswith("- ")):
                        break
                    if _depth(cur) == item:
                        jobs[job].append([])
                if jobs[job]:
                    jobs[job][-1].append(cur)
                i += 1
    return jobs


def job_fields(name):
    """{job id: {key: value}} of the keys directly under each job of
    .github/workflows/<name>, each value as written on the key's line (a
    trailing comment dropped; empty for a block such as `steps:`)."""
    with open(os.path.join(WORKFLOWS_DIR, name), encoding="utf-8") as fh:
        lines = fh.read().splitlines()
    jobs, job, job_depth, key_depth, in_jobs = {}, None, None, None, False
    for line in lines:
        if not _content(line):
            continue
        if _depth(line) == 0:
            in_jobs = line.rstrip() == "jobs:"
            continue
        if not in_jobs:
            continue
        job_depth = _depth(line) if job_depth is None else job_depth
        if _depth(line) == job_depth:
            job, key_depth = line.strip().rstrip(":"), None
            jobs[job] = {}
        elif job is not None:
            key_depth = _depth(line) if key_depth is None else key_depth
            m = re.match(r"^\s*([A-Za-z0-9_-]+):\s*(.*?)\s*$", line)
            if _depth(line) == key_depth and m:
                jobs[job][m.group(1)] = re.sub(r"\s+#.*$", "", m.group(2))
    return jobs


def step_field(step, key):
    """The value of one of a step's own keys (`uses`, `if`, `run`, ...) as
    written on its line, or None."""
    base = _depth(step[0]) + 2
    for k, line in enumerate(step):
        text = line.lstrip()[2:] if k == 0 else line
        if (k == 0 or _depth(line) == base) and _content(line):
            m = re.match(rf"^\s*{re.escape(key)}:\s*(.*?)\s*$", text)
            if m:
                return m.group(1)
    return None


def step_with(step, key):
    """One of a step's `with:` inputs as written (a trailing comment
    dropped), or None."""
    base = _depth(step[0]) + 2
    in_with, child = False, None
    for k, line in enumerate(step):
        text = " " * base + line.lstrip()[2:] if k == 0 else line
        if not _content(text):
            continue
        if _depth(text) <= base:
            in_with, child = _depth(text) == base and text.strip() == "with:", None
            continue
        if in_with:
            child = _depth(text) if child is None else child
            m = re.match(rf"^\s*{re.escape(key)}:\s*(.*?)\s*$", text)
            if _depth(text) == child and m:
                return re.sub(r"\s+#.*$", "", m.group(1))
    return None


def step_text(step):
    return "\n".join(step)


def step_script(step):
    """The commands of a step's `run:` script, stripped, without blank and
    comment lines."""
    base = _depth(step[0]) + 2
    out, inside = [], False
    for k, line in enumerate(step):
        text = " " * base + line.lstrip()[2:] if k == 0 else line
        if text.strip() and _depth(text) <= base:
            m = re.match(r"^\s*run:\s*(.*?)\s*$", text)
            inside = bool(m) and _depth(text) == base and m.group(1) in ("|", "|-", ">", ">-")
            if m and _depth(text) == base and not inside:
                out.append(m.group(1))
        elif inside and _content(text):
            out.append(text.strip())
    return out


# Commands that delete or empty files and folders: Remove-Item and its
# aliases, Clear-Content / Clear-Item, .NET Delete(), cmd's rd / del,
# robocopy (whose /MIR and /PURGE delete), git clean.
DELETE_RE = re.compile(r"(?i)(?<![\w$-])(?:Remove-Item|ri|rm|rmdir|rd|del|erase|Clear-Content|Clear-Item)(?![\w-])"
                       r"|::Delete\(|\.Delete\(|(?<![\w-])robocopy(?![\w-])|\bgit\s+clean\b")


def expression(value):
    """`${{ x }}` or `x` as x with its whitespace removed."""
    m = re.fullmatch(r"\$\{\{(.*)\}\}", (value or "").strip())
    return re.sub(r"\s+", "", m.group(1) if m else (value or ""))


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class ActionPins(unittest.TestCase):
    def test_every_outside_action_is_pinned_to_a_commit_with_its_version(self):
        refs = list(uses_lines())
        self.assertGreater(len(refs), 0)
        bad = [f"{where}: {ref}{rest}" for where, ref, rest in refs
               if not ref.startswith("./")
               and not (PINNED_RE.fullmatch(ref) and VERSION_COMMENT_RE.fullmatch(rest))]
        self.assertEqual(bad, [], "write uses: owner/repo@<full commit SHA> # <version tag of that commit>")

    def test_one_commit_per_action_version(self):
        # actions/cache, actions/cache/restore and actions/cache/save are one
        # repository: the same version must name the same commit everywhere.
        commits = {}
        for where, ref, rest in uses_lines():
            pin, comment = PINNED_RE.fullmatch(ref), VERSION_COMMENT_RE.fullmatch(rest)
            if pin and comment:
                key = (pin.group("repo").lower(), comment.group("version"))
                commits.setdefault(key, {}).setdefault(pin.group("sha"), []).append(where)
        self.assertEqual({k: v for k, v in commits.items() if len(v) > 1}, {})


NOT_ON_TAGS = "!startsWith(github.ref,'refs/tags/v')"


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class ReleaseBuild(unittest.TestCase):
    def test_the_release_exe_is_compiled_without_a_restored_rust_cache(self):
        # The job that compiles the service exe a tag run hands to signing
        # restores no Rust cache on v* tags: an entry an earlier run wrote
        # could otherwise put prebuilt artifacts into the signed binary.
        steps = jobs_steps("build.yml")["build"]
        self.assertTrue(any("cargo build --release" in step_text(s) for s in steps))
        caches = [s for s in steps if "Swatinem/rust-cache@" in (step_field(s, "uses") or "")]
        self.assertTrue(caches)
        for s in caches:
            self.assertEqual(expression(step_field(s, "if")), NOT_ON_TAGS, step_text(s))

    def test_release_tag_builds_get_more_time(self):
        # Without the Rust cache every tag build compiles every dependency
        # (other runs do only when the cache misses). A timeout there fails
        # the release, and a re-run compiles cold again.
        timeout = expression(job_fields("build.yml")["build"]["timeout-minutes"])
        m = re.fullmatch(r"startsWith\(github\.ref,'refs/tags/v'\)&&(\d+)\|\|(\d+)", timeout)
        self.assertTrue(m, timeout)
        self.assertGreater(int(m.group(1)), int(m.group(2)))

    def test_step_reader(self):
        # The reader the checks in this file rely on sees the jobs, their
        # steps and the steps' inputs.
        jobs = jobs_steps("build.yml")
        for job in ("build", "sign-service", "package", "sign-release", "publish", "msrv"):
            self.assertIn(job, jobs)
        self.assertEqual(jobs["verify-driver"], [])          # calls a workflow: no steps
        names = [step_field(s, "name") for s in jobs["build"]]
        self.assertIn("Build service (cargo --release)", names)
        checkout = jobs["build"][0]
        self.assertEqual(step_field(checkout, "uses").split("@")[0], "actions/checkout")
        self.assertEqual((step_with(checkout, "fetch-depth"), step_with(checkout, "persist-credentials")),
                         ("0", "false"))
        publish = jobs["publish"][0]
        self.assertEqual(step_with(publish, "sparse-checkout"), ".github/scripts")
        self.assertIsNone(step_with(publish, "fetch-depth"))
        fields = job_fields("build.yml")
        self.assertEqual(set(fields), set(jobs))
        self.assertEqual((fields["build"]["runs-on"], fields["msrv"]["runs-on"]), ("windows-2022", "ubuntu-latest"))
        self.assertEqual(fields["build"]["steps"], "")
        self.assertEqual(fields["verify-driver"]["uses"], "./.github/workflows/driver-reproducibility.yml")
        self.assertNotIn("contents", fields["sign-service"])          # under permissions:, not a job key


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class WhereTheseChecksRun(unittest.TestCase):
    def test_build_yml_runs_them_on_pull_requests_and_pushes(self):
        # build.yml runs for every pull request and every push to main; its
        # msrv job has no condition and runs this file right after its
        # checkout.
        with open(os.path.join(WORKFLOWS_DIR, "build.yml"), encoding="utf-8") as fh:
            on = re.search(r"(?ms)^on:\n(.*?)^\S", fh.read()).group(1)
        self.assertRegex(on, r"(?m)^  pull_request:")
        self.assertRegex(on, r"(?m)^  push:")
        fields = job_fields("build.yml")["msrv"]
        self.assertFalse({"if", "needs"} & set(fields), fields)
        steps = jobs_steps("build.yml")["msrv"]
        self.assertEqual(step_field(steps[0], "uses").split("@")[0], "actions/checkout")
        self.assertEqual((step_field(steps[1], "working-directory"), step_field(steps[1], "run")),
                         (".github/scripts", "python3 -m unittest -v test_workflows"))


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class Checkouts(unittest.TestCase):
    def test_jobs_with_actions_from_outside_github_keep_no_token_in_git_config(self):
        # actions/checkout leaves the job's token in .git/config unless told
        # not to. A job that also runs an action from another publisher
        # (rust-toolchain, rust-cache, setup-msbuild) must not hand it over.
        checked, bad = 0, []
        for name in sorted(os.listdir(WORKFLOWS_DIR)):
            if not name.endswith((".yml", ".yaml")):
                continue
            for job, steps in jobs_steps(name).items():
                uses = [step_field(s, "uses") or "" for s in steps]
                if not any(u and not u.startswith(("./", "actions/")) for u in uses):
                    continue
                for step, u in zip(steps, uses):
                    if u.startswith("actions/checkout@"):
                        checked += 1
                        if step_with(step, "persist-credentials") != "false":
                            bad.append(f"{name} job {job}: {step[0].strip()}")
        self.assertGreaterEqual(checked, 5)
        self.assertEqual(bad, [])


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class ReproducibilityBuildDirectory(unittest.TestCase):
    # The driver rebuild's directory comes from the release (the submitted
    # .sys's PDB path, the manifest) or from an input. repro_compare.py
    # builddir decides what happens there (the checkout is built in place;
    # any other directory must not exist yet and is created), and the job
    # deletes nothing but folders of its own under RUNNER_TEMP.
    @staticmethod
    def reproduce_steps():
        return jobs_steps("driver-reproducibility.yml")["reproduce"]

    def test_the_build_step_gets_its_directory_from_builddir_first(self):
        build = [s for s in self.reproduce_steps() if (step_field(s, "name") or "").startswith("Build driver")]
        self.assertEqual(len(build), 1)
        lines = step_script(build[0])
        calls = [i for i, line in enumerate(lines) if re.search(r"repro_compare\.py builddir \$root \$ws\b", line)]
        self.assertEqual(len(calls), 1, lines)
        self.assertRegex(lines[calls[0] + 1], r"^if \(\$LASTEXITCODE -ne 0\) \{ throw ")
        self.assertIn("$inPlace = [bool](Get-Content builddir.json -Raw | ConvertFrom-Json).in_place", lines)
        # Until builddir has answered, the step only checks the path.
        start = lines.index("$root = $env:BUILD_ROOT")
        for line in lines[start + 1:calls[0]]:
            if "$root" in line:
                self.assertRegex(line, r"^if \(.*\) \{ throw [^;]*\}$")

    def test_the_job_deletes_only_folders_of_its_own_under_runner_temp(self):
        deleted = []
        for step in self.reproduce_steps():
            lines = step_script(step)
            for i, line in enumerate(lines):
                if not DELETE_RE.search(line):
                    continue
                m = re.fullmatch(r"if \(Test-Path -LiteralPath (\$\w+)\) \{ Remove-Item -LiteralPath \1 -Recurse -Force \}",
                                 line)
                self.assertTrue(m, f"{step_field(step, 'name')}: {line}")
                named = re.fullmatch(rf'{re.escape(m.group(1))} = Join-Path \$env:RUNNER_TEMP "([\w.-]+)"',
                                     lines[i - 1] if i else "")
                self.assertTrue(named, f"{step_field(step, 'name')}: {line} must follow the line naming "
                                       f"{m.group(1)} as a folder under RUNNER_TEMP")
                deleted.append(named.group(1))
        self.assertIn("pristine-src", deleted)

    def test_the_deletion_scanner(self):
        for line in ("if (Test-Path $root) { Remove-Item -Recurse -Force $root }", "rm -r -fo $root",
                     "Get-ChildItem $root | Remove-Item -Recurse", "[IO.Directory]::Delete($root, $true)",
                     "(Get-Item $root).Delete($true)", "cmd /c rd /s /q $root", "cmd /c del /q $root\\*",
                     "Clear-Content $path", "robocopy $empty $root /MIR", "git clean -xdf", "rmdir $root",
                     "ri $root -Recurse"):
            self.assertTrue(DELETE_RE.search(line), line)
        for line in ("Copy-Item -Recurse driver (Join-Path $root \"driver\")", "Write-Host \"nothing was deleted\"",
                     "New-Item -ItemType Directory -Force -Path fresh | Out-Null", "$rd = 1", "-Recurse -Filter",
                     "throw \"build root $root is not a canonical absolute path\"", "Get-ChildItem -Recurse driver"):
            self.assertFalse(DELETE_RE.search(line), line)
        step = ["      - name: x", "        shell: pwsh", "        run: |", "          # a comment",
                "          a", "", "          b", "      # next step", "      - run: c"]
        self.assertEqual(step_script(step), ["a", "b"])
        self.assertEqual(step_script(["      - run: c"]), ["c"])


if __name__ == "__main__":
    unittest.main()
