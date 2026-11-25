"""The cases the Panel click workflow (panel-click.yml) runs.

On a push to a branch whose name starts with rig/: a fixed set, the
default case and five that each change one thing in it. Started by hand:
the one case its inputs give, each input checked against its choices.

Prints `cases=<json>` (a list of cases, each with a `name` that tells it
apart) and `window=<true|false>` (whether a case wants the panel as a
small window, which needs a build with debug assertions), for
$GITHUB_OUTPUT. Reads EVENT and CASE_<INPUT> from the environment.
"""
import json
import os
import sys

CHOICES = {
    "runner": ("windows-2025", "windows-2022"),
    "click": ("single", "double"),
    "icon": ("taskbar", "hidden"),
    "panel": ("flyout", "window"),
    "start": ("sign-in", "menu"),
    "network": ("quiet", "full"),
    "first_run": ("false", "true"),
    "tray": ("on", "off"),
}

DEFAULT = {key: allowed[0] for key, allowed in CHOICES.items()}

# On a push: the default case, then one change at a time.
PUSH_CHANGES = (
    {},
    {"start": "menu"},
    {"icon": "hidden"},
    {"click": "double"},
    {"runner": "windows-2022"},
    {"first_run": "true"},
)

WINDOWS = {"windows-2025": "win11", "windows-2022": "win10"}


def name(case):
    """`win11-single-taskbar-flyout-sign-in-quiet` (+ `-first-run`, + `-no-tray`)."""
    parts = [WINDOWS[case["runner"]], case["click"], case["icon"], case["panel"], case["start"], case["network"]]
    if case["first_run"] == "true":
        parts.append("first-run")
    if case["tray"] == "off":
        parts.append("no-tray")
    return "-".join(parts)


def cases(event, inputs):
    """The cases for `event` (push or workflow_dispatch); `inputs` are the
    dispatch's inputs by name ("" or missing: the default)."""
    if event == "push":
        chosen = [dict(DEFAULT, **change) for change in PUSH_CHANGES]
    elif event == "workflow_dispatch":
        case = {}
        for key, allowed in CHOICES.items():
            value = (inputs.get(key) or DEFAULT[key]).strip().lower()
            if value not in allowed:
                raise ValueError(f"{key}: {value!r} is not one of {', '.join(allowed)}")
            case[key] = value
        chosen = [case]
    else:
        raise ValueError(f"the workflow doesn't run on {event!r}")
    for case in chosen:
        case["name"] = name(case)
    return chosen


def main():
    chosen = cases(os.environ.get("EVENT", ""), {key: os.environ.get("CASE_" + key.upper(), "") for key in CHOICES})
    window = any(case["panel"] == "window" for case in chosen)
    print("cases=" + json.dumps(chosen, separators=(",", ":")))
    print("window=" + ("true" if window else "false"))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ValueError as e:
        print(f"::error::{e}", file=sys.stderr)
        sys.exit(1)
