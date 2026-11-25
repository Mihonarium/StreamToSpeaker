#!/usr/bin/env python3
"""Check that the Inno Setup compiler on a runner is the pinned version.

    python .github/scripts/inno_setup.py check <version> [--iscc PATH]

The runner image may already hold another Inno Setup, which `choco install
--version` can leave in place ("already installed"), so what choco says is
not enough. ISCC.exe's version resource doesn't carry the version either:
on the hosted images its numeric file version reads 0.0.0. What the
compiler prints when it compiles, "Compiler engine version: Inno Setup
X.Y.Z", comes from the compiler engine ISCC.exe has loaded, the code that
will compile the installer; that line is what this check relies on. It
compiles a script with no files into a temporary folder to get it, and
fails unless the line names exactly <version>.

Every other source it can read is printed too (the version resources of
ISCC.exe and ISCmplr.dll, the uninstall entry's DisplayVersion, what choco
lists) so that a run's log shows them side by side; they never decide.

Stdlib only.
"""
import argparse
import os
import re
import subprocess
import sys
import tempfile

ENGINE_LINE = re.compile(r"^\s*Compiler engine version:\s*Inno Setup\s+(\S+)", re.M)

# Compiled only to make ISCC print its engine version; what it makes is
# thrown away with the folder.
CHECK_SCRIPT = """\
[Setup]
AppName=Inno Setup version check
AppVersion=1
DefaultDirName={autopf}\\Inno Setup version check
OutputBaseFilename=version-check
Uninstallable=no
"""

UNINSTALL_KEYS = (
    ("HKEY_LOCAL_MACHINE", r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1"),
    ("HKEY_LOCAL_MACHINE", r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1"),
    ("HKEY_CURRENT_USER", r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1"),
)

# No double quotes: they don't survive powershell.exe's command line.
VERSION_RESOURCE = (
    "$v = (Get-Item -LiteralPath $env:INNO_CHECK_FILE).VersionInfo; "
    "'ProductVersion ' + $v.ProductVersion; 'FileVersion ' + $v.FileVersion; "
    "'FileMajorPart.FileMinorPart.FileBuildPart ' + $v.FileMajorPart + '.' + $v.FileMinorPart + '.' + "
    "$v.FileBuildPart"
)


def default_iscc():
    base = os.environ.get("ProgramFiles(x86)") or r"C:\Program Files (x86)"
    return os.path.join(base, "Inno Setup 6", "ISCC.exe")


def run_text(argv, env=None, timeout=600):
    """(exit code, output with stderr merged) of `argv`."""
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env, timeout=timeout)
    return p.returncode, p.stdout.decode("utf-8", errors="replace")


def engine_version(output):
    """The version ISCC's "Compiler engine version" line names, or None."""
    found = ENGINE_LINE.findall(output)
    return found[0] if len(set(found)) == 1 else None


def other_sources(iscc, run=run_text):
    """[(source, what it says)] of everything besides the engine line that
    names a version. Each one that can't be read says why instead."""
    out = []
    for path in (iscc, os.path.join(os.path.dirname(iscc), "ISCmplr.dll")):
        try:
            code, text = run(["powershell", "-NoProfile", "-NonInteractive", "-Command", VERSION_RESOURCE],
                             env=dict(os.environ, INNO_CHECK_FILE=path), timeout=60)
            said = "; ".join(line.strip() for line in text.splitlines() if line.strip())
            out.append((f"version resource of {path}", said if code == 0 else f"not read (exit {code}): {said}"))
        except (OSError, subprocess.SubprocessError) as e:
            out.append((f"version resource of {path}", f"not read: {e}"))
    try:
        import winreg
    except ImportError:
        out.append(("uninstall entry", "not read: no Windows registry here"))
    else:
        for hive, key in UNINSTALL_KEYS:
            try:
                with winreg.OpenKey(getattr(winreg, hive), key) as k:
                    value = winreg.QueryValueEx(k, "DisplayVersion")[0]
            except OSError:
                value = "(none)"
            out.append((f"DisplayVersion of {hive}\\{key}", str(value)))
    try:
        code, text = run(["choco", "list", "innosetup", "--exact", "--limit-output"], timeout=120)
        said = "; ".join(line.strip() for line in text.splitlines() if line.strip())
        out.append(("choco list innosetup", said if code == 0 else f"exit {code}: {said}"))
    except (OSError, subprocess.SubprocessError) as e:
        out.append(("choco list innosetup", f"not read: {e}"))
    return out


def check(version, iscc, run=run_text):
    """0 when the compiler engine at `iscc` says it is Inno Setup `version`,
    else 1. Prints every source it read."""
    print(f"Inno Setup pinned: {version}")
    for source, said in other_sources(iscc, run):
        print(f"  {source}: {said}")
    if not os.path.isfile(iscc):
        print(f"::error::there is no {iscc}")
        return 1
    with tempfile.TemporaryDirectory(prefix="inno-setup-check-") as tmp:
        script = os.path.join(tmp, "version-check.iss")
        with open(script, "w", encoding="ascii", newline="\r\n") as fh:
            fh.write(CHECK_SCRIPT)
        try:
            code, output = run([iscc, f"/O{os.path.join(tmp, 'out')}", script])
        except (OSError, subprocess.SubprocessError) as e:
            print(f"::error::could not run {iscc}: {e}")
            return 1
    print(f"  {iscc} compiling a script with no files (exit {code}):")
    for line in output.splitlines():
        print(f"    {line}")
    have = engine_version(output)
    if have is None:
        print(f"::error::{iscc} printed no single \"Compiler engine version: Inno Setup <version>\" line, so "
              f"which Inno Setup it is can't be told (its output is above); not building with it")
        return 1
    if have != version:
        print(f"::error::{iscc} is Inno Setup {have}, not the pinned {version} (INNO_SETUP_VERSION in build.yml)")
        return 1
    if code != 0:
        print(f"::warning::the check script did not compile (exit {code}); the version line above is what counts")
    print(f"Inno Setup {have}")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check", help="fail unless the compiler engine is this version")
    c.add_argument("version")
    c.add_argument("--iscc", default=None, help="ISCC.exe (default: Program Files (x86)\\Inno Setup 6)")
    args = ap.parse_args(argv)
    return check(args.version, args.iscc or default_iscc())


if __name__ == "__main__":
    sys.exit(main())
