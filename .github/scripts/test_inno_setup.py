"""Tests for inno_setup.py: the pinned compiler is recognised by the
version line its engine prints, never by its version resource or choco."""
import contextlib
import io
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import inno_setup  # noqa: E402

PIN = "6.7.1"


def banner(version_line):
    return ("Inno Setup 6 Command-Line Compiler\r\n"
            "Copyright (C) 1997-2025 Jordan Russell. All rights reserved.\r\n\r\n"
            + version_line +
            "\r\n\r\nParsing [Setup] section, line 2\r\n")


class FakeRunner:
    """Stands in for the programs the check runs. The version resources
    read 0.0.0, as on the hosted runner images."""

    def __init__(self, iscc_output, iscc_exit=0, choco="innosetup|6.7.1"):
        self.iscc_output, self.iscc_exit, self.choco = iscc_output, iscc_exit, choco
        self.iscc_calls, self.scripts = [], []

    def __call__(self, argv, env=None, timeout=600):
        if argv[0] == "powershell":
            return 0, "ProductVersion \r\nFileVersion \r\nFileMajorPart.FileMinorPart.FileBuildPart 0.0.0\r\n"
        if argv[0] == "choco":
            return 0, self.choco + "\r\n"
        self.iscc_calls.append(argv)
        with open(argv[-1], encoding="ascii") as fh:
            self.scripts.append(fh.read())
        return self.iscc_exit, self.iscc_output


class TheEngineLineDecides(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.iscc = os.path.join(self.tmp.name, "ISCC.exe")
        with open(self.iscc, "wb") as fh:
            fh.write(b"MZ")

    def check(self, runner, pin=PIN):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = inno_setup.check(pin, self.iscc, run=runner)
        self.output = out.getvalue()
        return code

    def test_the_pinned_engine_passes_though_its_file_version_reads_0_0_0(self):
        run = FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1"))
        self.assertEqual(self.check(run), 0, self.output)
        self.assertIn("FileMajorPart.FileMinorPart.FileBuildPart 0.0.0", self.output)
        self.assertIn("Inno Setup 6.7.1", self.output.splitlines()[-1])

    def test_another_engine_fails_even_when_choco_names_the_pin(self):
        run = FakeRunner(banner("Compiler engine version: Inno Setup 6.4.3"))
        self.assertEqual(self.check(run), 1)
        self.assertIn("is Inno Setup 6.4.3, not the pinned 6.7.1", self.output)
        self.assertIn("choco list innosetup: innosetup|6.7.1", self.output)

    def test_a_longer_version_is_not_the_pin(self):
        self.assertEqual(self.check(FakeRunner(banner("Compiler engine version: Inno Setup 6.7.10"))), 1)
        self.assertEqual(self.check(FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1-dev"))), 1)

    def test_the_older_unicode_suffix_is_not_part_of_the_version(self):
        self.assertEqual(self.check(FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1 (u)"))), 0)

    def test_no_engine_line_fails_and_shows_what_iscc_printed(self):
        run = FakeRunner("Inno Setup 6 Command-Line Compiler\r\nError: something else\r\n", iscc_exit=2)
        self.assertEqual(self.check(run), 1)
        self.assertIn("printed no single", self.output)
        self.assertIn("    Error: something else", self.output)

    def test_two_different_engine_lines_fail(self):
        run = FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1") +
                         "Compiler engine version: Inno Setup 6.4.3\r\n")
        self.assertEqual(self.check(run), 1)

    def test_a_check_script_that_does_not_compile_does_not_decide(self):
        run = FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1"), iscc_exit=2)
        self.assertEqual(self.check(run), 0)
        self.assertIn("::warning::the check script did not compile (exit 2)", self.output)

    def test_without_iscc_it_fails(self):
        os.remove(self.iscc)
        run = FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1"))
        self.assertEqual(self.check(run), 1)
        self.assertEqual(run.iscc_calls, [])

    def test_it_compiles_a_script_with_no_files_into_a_folder_it_removes(self):
        run = FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1"))
        self.check(run)
        (argv,) = run.iscc_calls
        self.assertEqual(argv[0], self.iscc)
        self.assertTrue(argv[1].startswith("/O"))
        self.assertIn("[Setup]", run.scripts[0])
        self.assertNotIn("[Files]", run.scripts[0])
        self.assertFalse(os.path.exists(os.path.dirname(argv[-1])))
        self.assertFalse(os.path.exists(argv[1][2:]))

    def test_every_source_is_printed(self):
        self.check(FakeRunner(banner("Compiler engine version: Inno Setup 6.7.1")))
        for source in ("version resource of " + self.iscc, "ISCmplr.dll", "choco list innosetup",
                       "Compiler engine version: Inno Setup 6.7.1"):
            self.assertIn(source, self.output)
        self.assertTrue("DisplayVersion of HKEY_LOCAL_MACHINE" in self.output
                        or "uninstall entry: not read" in self.output)


class TheEngineLine(unittest.TestCase):
    def test_reads_the_version_iscc_prints(self):
        self.assertEqual(inno_setup.engine_version(banner("Compiler engine version: Inno Setup 6.7.1")), "6.7.1")

    def test_is_none_without_the_line(self):
        self.assertIsNone(inno_setup.engine_version("Inno Setup 6 Command-Line Compiler\r\n"))


if __name__ == "__main__":
    unittest.main()
