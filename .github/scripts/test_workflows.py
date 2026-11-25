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



def job_lines(name):
    """(job id, line) for every line under `jobs:` of .github/workflows/<name>
    below a job's own line: its keys, steps, env, conditions."""
    with open(os.path.join(WORKFLOWS_DIR, name), encoding="utf-8") as fh:
        lines = fh.read().splitlines()
    out, job, job_depth, in_jobs = [], None, None, False
    for line in lines:
        if _content(line) and _depth(line) == 0:
            in_jobs, job = line.rstrip() == "jobs:", None
            continue
        if not in_jobs:
            continue
        if _content(line):
            job_depth = _depth(line) if job_depth is None else job_depth
            if _depth(line) == job_depth:
                job = line.strip().rstrip(":")
                continue
        if job is not None:
            out.append((job, line))
    return out

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
class ReleaseTools(unittest.TestCase):
    def test_the_service_is_built_and_tested_from_the_locked_dependencies(self):
        # Without --locked, a Cargo.lock that doesn't satisfy Cargo.toml is
        # re-resolved to the newest matching crates, which then get
        # EV-signed. The tag build stamps the crate version, so it updates
        # the lock entry of the crate itself, and only that.
        cargo = [(step, line) for job, step, line in script_lines("build.yml")
                 if job == "build" and re.match(r"cargo (build|test)\b", line)]
        self.assertGreaterEqual(len(cargo), 3)
        self.assertEqual([c for c in cargo if "--locked" not in c[1]], [])
        stamp = [step_script(s) for s in jobs_steps("build.yml")["build"]
                 if (step_field(s, "name") or "").startswith("Stamp crate version")]
        self.assertEqual(len(stamp), 1)
        self.assertIn("cargo update --workspace", stamp[0])
        self.assertTrue(any("git diff --numstat -- Cargo.lock" in line for line in stamp[0]))

    def test_inno_setup_is_one_pinned_version_everywhere(self):
        # choco without --version installs whatever is newest, and the
        # runner image may hold another version already: every install
        # names the pinned version, then asks the compiler what it is
        # (inno_setup.py). ISCC.exe's version resource reads 0.0.0 on the
        # hosted images, so nothing may decide by it.
        with open(os.path.join(WORKFLOWS_DIR, "build.yml"), encoding="utf-8") as fh:
            pin = re.search(r'(?m)^  INNO_SETUP_VERSION: "(\d+\.\d+\.\d+)"$', fh.read())
        self.assertTrue(pin)
        installs = [(name, job, step, line) for name in sorted(os.listdir(WORKFLOWS_DIR)) if name.endswith(".yml")
                    for job, step, line in script_lines(name) if re.search(r"\bchoco\s+install\b", line)]
        self.assertGreaterEqual(len(installs), 2)
        for name, job, step, line in installs:
            self.assertIn("--version $env:INNO_SETUP_VERSION", line, f"{name} {job}")
            steps = [s for s in jobs_steps(name)[job] if (step_field(s, "name") or s[0].strip()) == step]
            script = step_script(steps[0])
            self.assertIn("python .github/scripts/inno_setup.py check $env:INNO_SETUP_VERSION", script, job)
            self.assertGreater(script.index("python .github/scripts/inno_setup.py check $env:INNO_SETUP_VERSION"),
                               next(i for i, l in enumerate(script) if "choco install" in l), job)
        for name in sorted(os.listdir(WORKFLOWS_DIR)):
            if name.endswith(".yml"):
                for job, step, line in script_lines(name):
                    self.assertNotRegex(line, r"File(Major|Minor|Build)Part", f"{name} {job} {step}")


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class WhatTheInstallerBundles(unittest.TestCase):
    def test_devcon_is_checked_wherever_it_is_staged(self):
        # devcon.exe comes from the WDK through the driver cache, nothing
        # here builds it, and the installer runs it elevated.
        staging = [(job, step_field(s, "name"), step_script(s)) for job, steps in jobs_steps("build.yml").items()
                   for s in steps if any("devcon.exe" in l for l in step_script(s))
                   and any("$staging" in l for l in step_script(s))]
        self.assertGreaterEqual(len(staging), 2)
        for job, name, lines in staging:
            self.assertTrue(any(re.search(r"Assert-MicrosoftSigned\.ps1 -Path \S*devcon\.exe", l) for l in lines),
                            f"{job}: {name}")

    def test_a_tag_falls_back_to_the_test_signed_driver_only_after_the_tree_check(self):
        steps = jobs_steps("build.yml")["package"]
        names = [step_field(s, "name") for s in steps]
        check = names.index("Check a test-signed fallback is not a source-hash mismatch")
        self.assertEqual(expression(step_field(steps[check], "if")), "steps.attested.outputs.found=='false'")
        self.assertLess(names.index("Fetch Microsoft-signed driver (if one matches the driver source)"), check)
        self.assertLess(check, names.index("Stage + build release installer"))


LEVELS = {"none": 0, "read": 1, "write": 2}


def permissions_block(lines):
    """{scope: level} of a `permissions:` block in `lines` (the lines of a
    workflow's top level or of one job), or None when there is none.
    `read-all` / `write-all` give every scope that level."""
    for i, line in enumerate(lines):
        m = re.match(r"^(\s*)permissions:\s*(\S*)\s*$", line)
        if not m:
            continue
        if m.group(2) in ("read-all", "write-all"):
            return {"*": m.group(2).split("-")[0]}
        if m.group(2) == "{}":
            return {}
        out = {}
        for nxt in lines[i + 1:]:
            if not _content(nxt):
                continue
            if _depth(nxt) <= len(m.group(1)):
                break
            k = re.match(r"^\s*([a-z-]+):\s*([a-z]+)", nxt)
            if k:
                out[k.group(1)] = k.group(2)
        return out
    return None


def workflow_permissions(name):
    with open(os.path.join(WORKFLOWS_DIR, name), encoding="utf-8") as fh:
        lines = fh.read().splitlines()
    top = []
    for line in lines:
        if line.rstrip() == "jobs:":
            break
        top.append(line)
    return permissions_block(top)


def job_permissions(name, job):
    """What the job's token gets: its own block, else the workflow's."""
    own = permissions_block(job_block(name, job))
    return own if own is not None else workflow_permissions(name)


def level(perms, scope):
    if perms is None:
        return None
    return LEVELS[perms.get(scope, perms.get("*", "none"))]


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class ReusableWorkflowPermissions(unittest.TestCase):
    """A job that calls a workflow of this repository grants it permissions
    that the called workflow's jobs can only narrow: GitHub refuses the whole
    run when a called job asks for more than the caller grants."""

    def calls(self):
        for name in sorted(os.listdir(WORKFLOWS_DIR)):
            if not name.endswith(".yml"):
                continue
            for job, fields in job_fields(name).items():
                m = re.fullmatch(r"\./\.github/workflows/([\w.-]+\.yml)", fields.get("uses", ""))
                if m:
                    yield name, job, m.group(1)

    def test_every_called_job_asks_for_no_more_than_its_caller_grants(self):
        calls = list(self.calls())
        self.assertGreaterEqual(len(calls), 3)
        bad = []
        for name, job, called in calls:
            grant = job_permissions(name, job)
            self.assertIsNotNone(grant, f"{name} job {job} grants no explicit permissions")
            for cjob in job_fields(called):
                asked = job_permissions(called, cjob) or {}
                for scope in asked:
                    if scope == "*" or level(asked, scope) > level(grant, scope):
                        bad.append(f"{called} job {cjob} asks {scope}: {asked[scope]}, "
                                   f"but {name} job {job} grants {grant.get(scope, 'none')}")
        self.assertEqual(bad, [])

    def test_the_permission_reader(self):
        lines = ["    permissions:", "      contents: write   # x", "      actions: read", "    steps:"]
        self.assertEqual(permissions_block(lines), {"contents": "write", "actions": "read"})
        self.assertEqual(permissions_block(["    permissions: read-all"]), {"*": "read"})
        self.assertIsNone(permissions_block(["    runs-on: x"]))


def env_of(lines):
    """{NAME: value} of the `env:` block directly in `lines` (a job's or a
    step's own lines)."""
    out, inside, depth = {}, False, None
    for line in lines:
        if not _content(line):
            continue
        m = re.match(r"^(\s*)(?:- )?env:\s*$", line)
        if m:
            inside, depth = True, len(m.group(1))
            continue
        if inside:
            if _depth(line) <= depth:
                inside = False
                continue
            k = re.match(r"^\s*([A-Z][A-Z0-9_]*):\s*(.*?)\s*$", line)
            if k:
                out[k.group(1)] = k.group(2)
    return out


def substituted(text, env):
    """`text` with $NAME, ${NAME} and $env:NAME replaced by the env value
    they name, quotes dropped."""
    def sub(m):
        return env.get(m.group(1) or m.group(2) or m.group(3), m.group(0))
    text = re.sub(r"\$env:([A-Z][A-Z0-9_]*)|\$\{([A-Z][A-Z0-9_]*)\}|\$([A-Z][A-Z0-9_]*)", sub, text)
    return text.replace('"', "").replace("'", "")


UPLOADED_RE = re.compile(r"--signed-zip\s+(\S+)|release_draft\.py\s+upload\s+\S+\s+(\S+)"
                         r"|gh\s+release\s+upload\s+\S+\s+(\S+)")
REMAKE_RE = re.compile(r"certified\.py\s+package\b|Compress-Archive\b|canonical_zip\(")


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class WhatIsAttestedIsWhatIsPublished(unittest.TestCase):
    """A job that makes a provenance attestation uploads the very file the
    attestation names, and nothing in that job makes the file again after
    it was attested."""

    def test_every_attested_file_is_the_file_the_job_uploads(self):
        checked, bad = 0, []
        for name in sorted(os.listdir(WORKFLOWS_DIR)):
            if not name.endswith(".yml"):
                continue
            for job, steps in jobs_steps(name).items():
                attests = [i for i, st in enumerate(steps)
                           if "attest-build-provenance@" in (step_field(st, "uses") or "")]
                if not attests:
                    continue
                job_env = env_of(job_block(name, job))
                subject = substituted(step_with(steps[attests[0]], "subject-path") or "", job_env)
                uploaded = set()
                for st in steps:
                    env = dict(job_env, **env_of(st))
                    for line in step_script(st):
                        for m in UPLOADED_RE.finditer(line):
                            # (a bash array's closing parenthesis is not part of it)
                            uploaded.add(substituted(next(g for g in m.groups() if g), env).rstrip(")"))
                checked += 1
                if subject not in uploaded:
                    bad.append(f"{name} job {job}: attests {subject}, uploads {sorted(uploaded)}")
                for st in steps[attests[0] + 1:]:
                    if any(REMAKE_RE.search(line) for line in step_script(st)):
                        bad.append(f"{name} job {job}: {step_field(st, 'name')} makes a file again after it was attested")
        self.assertGreaterEqual(checked, 4)
        self.assertEqual(bad, [])

    def test_the_readers(self):
        self.assertEqual(env_of(["    env:", "      A: x", "      B: ${{ y }}", "    steps:"]), {"A": "x", "B": "${{ y }}"})
        self.assertEqual(substituted('--signed-zip "canonical/$NAME" $env:X', {"NAME": "${{ a }}", "X": "b"}),
                         "--signed-zip canonical/${{ a }} b")


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class InstallerLifecycleFilter(unittest.TestCase):
    """build.yml's installer-changes job decides whether a pull request runs
    the installer's life-cycle test: every file the installer is made of
    or runs counts, including scripts added later."""

    def pattern(self):
        steps = jobs_steps("build.yml")["installer-changes"]
        lines = [l for s in steps for l in step_script(s) if "grep -qE" in l]
        self.assertEqual(len(lines), 1)
        return re.compile(re.search(r"grep -qE '([^']+)'", lines[0]).group(1))

    def test_what_the_installer_is_made_of_counts(self):
        pattern = self.pattern()
        for path in ("installer/StreamToSpeaker.iss", "installer/Uninstall-Driver.ps1", "installer/build-installer.ps1",
                     "scripts/Pre-Install.ps1", "scripts/Install-Driver.ps1", "scripts/lib/Driver.psm1",
                     "driver/StreamToSpeaker.inf", "driver/adapter.cpp", "include/stream_to_speaker_ioctl.h",
                     "service/src/autostart.rs",
                     "README.md", "LICENSE", "LICENSE-BINARIES.md",
                     ".github/workflows/build.yml", ".github/workflows/installer-lifecycle.yml",
                     ".github/scripts/Test-InstallerLifecycle.ps1", ".github/scripts/Write-DriverRecord.ps1",
                     ".github/scripts/Get-AttestedDriver.ps1", ".github/scripts/Assert-MicrosoftSigned.ps1"):
            self.assertTrue(pattern.search(path), path)
        # A driver change rebuilds the driver, whose version and upgrade
        # the test checks; the rest of service/ never runs in it (silent
        # install, "Launch" is skipped).
        for path in ("service/src/gui.rs", "docs/driver-signing.md",
                     ".github/workflows/driver-attest.yml", "TECHNICAL.md"):
            self.assertFalse(pattern.search(path), path)


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class WhereTheseChecksRun(unittest.TestCase):
    """Every test of the chain's scripts (this file among them) runs on
    every pull request, push and tag, with a read-only token and no secret,
    and a tag's release jobs wait for them."""

    def test_build_yml_runs_them_on_pull_requests_and_pushes(self):
        with open(os.path.join(WORKFLOWS_DIR, "build.yml"), encoding="utf-8") as fh:
            on = re.search(r"(?ms)^on:\n(.*?)^\S", fh.read()).group(1)
        self.assertRegex(on, r"(?m)^  pull_request:")
        self.assertRegex(on, r"(?m)^  push:")
        fields = job_fields("build.yml")
        jobs = jobs_steps("build.yml")
        # Both find their test files themselves (test*.py, *.Tests.ps1), so
        # a test file added there runs without an edit here.
        for job, command in (("chain-tests", "python3 -m unittest discover -b -v -s .github/scripts"),
                             ("chain-tests-powershell", "Invoke-Pester -Path .github/scripts -Output Detailed -CI")):
            self.assertFalse({"if", "needs", "environment"} & set(fields[job]), fields[job])
            steps = jobs[job]
            self.assertEqual(step_field(steps[0], "uses").split("@")[0], "actions/checkout")
            self.assertEqual(step_with(steps[0], "persist-credentials"), "false")
            self.assertTrue(any(command in line for s in steps for line in step_script(s)), job)
            lines = [line.strip() for j, line in job_lines("build.yml") if j == job]
            self.assertIn("contents: read", lines)
            self.assertFalse([l for l in lines if "secrets." in l or "write" in l], job)

    def test_a_tag_signs_nothing_before_they_pass(self):
        needs = job_fields("build.yml")["sign-service"]["needs"]
        for job in ("chain-tests", "chain-tests-powershell"):
            self.assertIn(job, needs)


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



@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class PartnerCenterCredential(unittest.TestCase):
    def test_only_jobs_in_the_partner_center_environment_read_it(self):
        # AZURE_* are secrets of the partner-center environment, whose
        # deployment policy admits branch main alone; outside it a job gets
        # nothing for them. Exactly the two jobs that call the Hardware API
        # read them, and both run in the environment.
        readers, inside = set(), set()
        for name in sorted(os.listdir(WORKFLOWS_DIR)):
            if not name.endswith((".yml", ".yaml")):
                continue
            readers |= {(name, job) for job, line in job_lines(name) if _content(line) and "secrets.AZURE_" in line}
            inside |= {(name, job) for job, f in job_fields(name).items() if f.get("environment") == "partner-center"}
        expected = {("driver-attest.yml", "reconcile"), ("driver-certified.yml", "download")}
        self.assertEqual(readers, expected)
        self.assertEqual(inside, expected)

    def test_the_credential_free_runs_stay_outside_it(self):
        # Driver attest's dry run and Driver certified's ms_zip_asset path
        # are how a change to these workflows is tried from its branch.
        attest = job_fields("driver-attest.yml")
        self.assertNotIn("environment", attest["dry-run"])
        self.assertIn("!inputs.dry_run", attest["reconcile"].get("if", "") + "".join(
            line for job, line in job_lines("driver-attest.yml") if job == "reconcile"))
        certified = job_fields("driver-certified.yml")
        self.assertIn("needs.resolve.outputs.mode=='api'", expression(certified["download"]["if"]))


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class DriverWorkflowScripts(unittest.TestCase):
    # These workflows hold the Partner Center credential or write driver
    # releases. Their scripts take every value from env:, so no value,
    # whoever typed it, becomes script text.
    FILES = ("driver-attest.yml", "driver-attested.yml", "driver-certified.yml")
    INPUTS_RE = re.compile(r"(?<![\w.])inputs\.")

    def test_no_expression_inside_a_run_script(self):
        bad = []
        for name in self.FILES:
            for job, steps in jobs_steps(name).items():
                for step in steps:
                    bad += [f"{name} job {job}: {line}" for line in step_script(step) if "${{" in line]
        self.assertEqual(bad, [])

    def test_the_script_reader_sees_expressions(self):
        steps = [["      - name: x", "        run: |", "          echo ${{ inputs.a }}"],
                 ["      - run: echo ${{ inputs.b }}"],
                 ["      - name: y", "        env:", "          A: ${{ inputs.c }}", "        run: echo \"$A\""]]
        self.assertEqual([[l for l in step_script(s) if "${{" in l] for s in steps],
                         [["echo ${{ inputs.a }}"], ["echo ${{ inputs.b }}"], []])

    def test_only_the_check_step_reads_driver_certifieds_inputs(self):
        # Every later step of driver-certified.yml uses the values that
        # certified.py inputs checked (resolve's outputs).
        uses = lambda line: _content(line) and self.INPUTS_RE.search(line)
        readers = [job for job, line in job_lines("driver-certified.yml") if uses(line)]
        check = [s for s in jobs_steps("driver-certified.yml")["resolve"] if step_field(s, "id") == "inputs"]
        self.assertEqual(len(check), 1)
        in_check = [line for line in check[0] if uses(line)]
        self.assertEqual(len(in_check), 10)
        self.assertEqual(len(readers), len(in_check))
        self.assertEqual(step_field(check[0], "run"), "python3 .github/scripts/certified.py inputs")


def script_lines(name):
    """(job, step name, command) for every command of every step script of
    .github/workflows/<name>."""
    for job, steps in jobs_steps(name).items():
        for step in steps:
            for line in step_script(step):
                yield job, step_field(step, "name") or step[0].strip(), line


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class DriverVerChecks(unittest.TestCase):
    def test_every_driverver_check_matches_the_whole_version(self):
        # ",1.1.0.20" is a substring of ",1.1.0.209": the workflows check
        # an INF's DriverVer with repro_compare.py driverver instead.
        bad = []
        for name in sorted(os.listdir(WORKFLOWS_DIR)):
            if name.endswith(".yml"):
                bad += [f"{name} job {job}, {step}: {line}" for job, step, line in script_lines(name)
                        if re.search(r"-notmatch\s+\[regex\]::Escape\(\",", line)]
        self.assertEqual(bad, [])



SCRIPTS_DIR = os.path.join(GITHUB_DIR, "scripts")
DRIVER_WORKFLOWS = ("driver-submission.yml", "driver-attest.yml", "driver-attested.yml", "driver-certified.yml")
RELEASE_WRITE_RE = re.compile(r"\bgh\s+release\s+(?:upload|create|delete|delete-asset|edit)\b"
                              r"|\bgh\s+api\b.*(?:--method|-X)\s+(?:POST|PATCH|PUT|DELETE)\b.*\breleases\b")
# What a job runs that writes a driver release.
DRIVER_WRITE_RE = re.compile(r"driver_release\.py\s+(?:submission-(?:release|signed|cleanup)|attested-record)\b"
                             r"|certified\.py\s+release\b|attest\.py\b")
MAIN_ONLY = "github.ref=='refs/heads/main'"
NOT_DRY_RUN = ("!inputs.dry_run", "needs.resolve.outputs.dry_run!='true'")


def job_block(name, job):
    """The lines of one job, without its steps."""
    lines = [line for j, line in job_lines(name) if j == job]
    out = []
    for line in lines:
        if line.strip() == "steps:":
            break
        out.append(line)
    return out


def job_if(name, job):
    """A job's `if:` as one expression, also when written as a block
    (`if: >-` and the lines under it)."""
    lines = [line for j, line in job_lines(name) if j == job]
    for i, line in enumerate(lines):
        m = re.match(r"^(\s*)if:\s*(.*?)\s*$", line)
        if not m or len(m.group(1)) != _depth(next(l for l in lines if _content(l))):
            continue
        if m.group(2) not in (">-", ">", "|", "|-"):
            return expression(m.group(2))
        block = []
        for nxt in lines[i + 1:]:
            if _content(nxt) and _depth(nxt) <= len(m.group(1)):
                break
            block.append(nxt.strip())
        return expression(" ".join(block))
    return ""


def writes_driver_releases(name, job):
    return any(DRIVER_WRITE_RE.search(line) for j, _step, line in script_lines(name) if j == job)


def text_files():
    """(relative path, text) of every workflow, action and script."""
    out = []
    for path in workflow_files() + sorted(os.path.join(SCRIPTS_DIR, f) for f in os.listdir(SCRIPTS_DIR)
                                          if f.endswith((".py", ".ps1")) and not f.startswith(("test_", "fake_"))
                                          and not f.endswith(".Tests.ps1")):
        with open(path, encoding="utf-8") as fh:
            out.append((os.path.relpath(path, GITHUB_DIR), fh.read()))
    return out


@unittest.skipUnless(HAVE_WORKFLOWS, "the workflow files are not checked out next to this script")
class ReleaseWrites(unittest.TestCase):
    """What exists on a release is kept: nothing deletes an asset before
    its replacement is up, and driver releases are written by one script
    that refuses to change a finished one."""

    def test_no_release_asset_is_replaced_in_place(self):
        # gh release upload --clobber deletes the old asset first; a failed
        # or cancelled upload then leaves the release without it.
        bad = []
        for rel, text in text_files():
            for m in re.finditer(r"gh\s+release\s+upload\b[^\n]*--clobber|\"upload\"[^)\]]*\"--clobber\"", text):
                bad.append(f"{rel}: {m.group(0)[:120]}")
        self.assertEqual(bad, [])

    def test_driver_releases_are_written_only_through_driver_release_py(self):
        bad = [f"{name} job {job}, {step}: {line}" for name in DRIVER_WORKFLOWS
               for job, step, line in script_lines(name) if RELEASE_WRITE_RE.search(line)]
        self.assertEqual(bad, [])

    def test_driver_submission_writes_releases_only_from_main(self):
        # A dispatch from any other branch builds the CAB and stops: it
        # creates, changes and deletes no release.
        main = "github.ref=='refs/heads/main'"
        writers = []
        for job, steps in jobs_steps("driver-submission.yml").items():
            job_if = expression(job_fields("driver-submission.yml")[job].get("if", ""))
            for step in steps:
                lines = step_script(step)
                if any(RELEASE_WRITE_RE.search(l) or re.search(r"driver_release\.py\s+submission-(?!check|guard)", l)
                       for l in lines) or step_with(step, "token"):
                    writers.append((job, step_field(step, "name"), main in job_if or main in expression(
                        step_field(step, "if") or "")))
        self.assertTrue(writers)
        self.assertEqual([w for w in writers if not w[2]], [])

    def test_the_write_check_sees_direct_api_writes(self):
        for line in ("gh api --method DELETE repos/o/r/releases/assets/1", "gh api -X PATCH repos/o/r/releases/2",
                     "gh release upload v1 a.zip"):
            self.assertTrue(RELEASE_WRITE_RE.search(line), line)
        for line in ("gh api repos/o/r/releases?per_page=100", "gh release download v1 --pattern x"):
            self.assertFalse(RELEASE_WRITE_RE.search(line), line)

    def test_every_job_that_writes_a_driver_release_waits_for_the_others(self):
        # One repository-wide group, nothing cancelled: two runs never
        # write driver releases at the same time.
        writers = [(name, job) for name in DRIVER_WORKFLOWS for job in jobs_steps(name)
                   if writes_driver_releases(name, job)]
        self.assertGreaterEqual(len(writers), 5, writers)
        for name, job in writers:
            block = [line.strip() for line in job_block(name, job)]
            for want in ("group: driver-release-writes", "cancel-in-progress: false", "queue: max"):
                self.assertIn(want, block, f"{name} job {job}")
        # Waiting publish jobs of several tags are not cancelled either.
        block = [line.strip() for line in job_block("build.yml", "publish")]
        self.assertIn("queue: max", block)
        # Driver attest's dry run has no group a real run waits in.
        with open(os.path.join(WORKFLOWS_DIR, "driver-attest.yml"), encoding="utf-8") as fh:
            self.assertNotRegex(fh.read(), r"(?m)^concurrency:")

    def test_no_dry_run_or_branch_run_holds_a_write_token_or_a_secret(self):
        # A job that executes in a dry run or from a ref other than main has
        # contents: read and no id-token, attestations, secret or
        # environment. A job that holds any of these runs only on main (its
        # if, or the partner-center environment, which admits main alone)
        # and, in a workflow with a dry_run input, never in a dry run.
        bad = []
        for name in DRIVER_WORKFLOWS:
            with open(os.path.join(WORKFLOWS_DIR, name), encoding="utf-8") as fh:
                text = fh.read()
            top = re.search(r"(?ms)^permissions:\n(.*?)^\S", text).group(1)
            self.assertEqual([l.strip() for l in top.splitlines() if l.strip()], ["contents: read"], name)
            has_dry_run = re.search(r"(?m)^      dry_run:", text) is not None
            for job, fields in job_fields(name).items():
                if "uses" in fields:
                    continue  # a called workflow: its own jobs are checked in its file
                lines = [l for j, l in job_lines(name) if j == job and _content(l)]
                held = [l.strip() for l in lines if re.search(r":\s*write\b|^\s*environment:|secrets\.", l)]
                if not held:
                    continue
                cond = job_if(name, job)
                main_only = MAIN_ONLY in cond or fields.get("environment") == "partner-center"
                not_dry = not has_dry_run or any(c in cond for c in NOT_DRY_RUN)
                if not (main_only and not_dry):
                    bad.append(f"{name} job {job} holds {held} with if: {fields.get('if', '')!r}")
        self.assertEqual(bad, [])

    def test_a_driver_submission_run_from_a_branch_starts_no_attestation(self):
        self.assertIn("github.event.workflow_run.head_branch=='main'", job_if("driver-attest.yml", "reconcile"))

    def test_every_release_listing_reads_every_page(self):
        # One page holds 100 releases; every version and every driver round
        # adds one. A listing that stops there misses older releases.
        bad = []
        for rel, text in text_files():
            for m in re.finditer(r"[^\n]*releases\?per_page=[^\n]*", text):
                if "--paginate" not in m.group(0):
                    bad.append(f"{rel}: {m.group(0).strip()[:120]}")
        self.assertEqual(bad, [])


if __name__ == "__main__":
    unittest.main()
