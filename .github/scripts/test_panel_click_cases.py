"""Tests for panel_click_cases.py. Standard library only."""
import json
import os
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import panel_click_cases as pc  # noqa: E402


class Cases(unittest.TestCase):
    def test_a_push_runs_the_default_and_five_single_changes_all_as_a_flyout(self):
        chosen = pc.cases("push", {})
        self.assertEqual([c["name"] for c in chosen], [
            "win11-single-taskbar-flyout-sign-in-quiet",
            "win11-single-taskbar-flyout-menu-quiet",
            "win11-single-hidden-flyout-sign-in-quiet",
            "win11-double-taskbar-flyout-sign-in-quiet",
            "win10-single-taskbar-flyout-sign-in-quiet",
            "win11-single-taskbar-flyout-sign-in-quiet-first-run",
        ])
        self.assertTrue(all(c["panel"] == "flyout" for c in chosen))
        self.assertEqual([c["first_run"] for c in chosen], ["false"] * 5 + ["true"])
        self.assertEqual(len({c["name"] for c in chosen}), len(chosen))
        # Inputs play no part in a push.
        self.assertEqual(pc.cases("push", {"panel": "window", "runner": "windows-2022"}), chosen)

    def test_a_start_by_hand_runs_the_one_case_its_inputs_give(self):
        chosen = pc.cases("workflow_dispatch", {"runner": "windows-2022", "click": "double", "icon": "hidden",
                                                "panel": "window", "start": "menu", "network": "full",
                                                "first_run": "true"})
        self.assertEqual(len(chosen), 1)
        self.assertEqual(chosen[0]["name"], "win10-double-hidden-window-menu-full-first-run")
        # Missing or empty inputs: the defaults.
        self.assertEqual(pc.cases("workflow_dispatch", {"click": ""})[0]["name"],
                         "win11-single-taskbar-flyout-sign-in-quiet")

    def test_values_outside_the_choices_and_other_events_are_refused(self):
        with self.assertRaises(ValueError):
            pc.cases("workflow_dispatch", {"runner": "windows-latest"})
        with self.assertRaises(ValueError):
            pc.cases("workflow_dispatch", {"start": "menu\nwindow=true"})
        with self.assertRaises(ValueError):
            pc.cases("pull_request", {})

    def test_the_outputs_say_whether_a_build_with_debug_assertions_is_needed(self):
        def run(env):
            out = subprocess.run([sys.executable, "-I", pc.__file__], env=dict(env, PATH=os.environ.get("PATH", "")),
                                 capture_output=True, text=True, check=True).stdout.splitlines()
            return dict(line.split("=", 1) for line in out)

        push = run({"EVENT": "push"})
        self.assertEqual(push["window"], "false")
        self.assertEqual(len(json.loads(push["cases"])), 6)
        hand = run({"EVENT": "workflow_dispatch", "CASE_PANEL": "window"})
        self.assertEqual(hand["window"], "true")
        self.assertEqual([c["panel"] for c in json.loads(hand["cases"])], ["window"])


if __name__ == "__main__":
    unittest.main()
