#!/usr/bin/env python3
"""Stream To Speaker brand mark, authored as geometry.

Run this to regenerate the SVGs after changing colours or geometry, then
re-render the raster assets (see assets/README.md).

Every icon is transparent. That rules out drawing the mark in white — it
would vanish in Explorer and the installer — and equally rules out the brand
indigo, which vanishes on a dark taskbar. The ink below is a mid-tone picked
by rendering the candidates on white, Explorer grey, mid grey, taskbar dark
and black: it is the one value legible on all five.

Two families, same drawing:
  app/*   128px and up — window, taskbar, exe, installer
  tray/*  16-32px — heavier strokes, because hairlines break up when the
          rasteriser has only a few pixels to work with

Three states in each. The shape never changes, only the arc COLOUR, so the
icon stays recognisable while still reporting what the app is doing:
  idle      no speaker chosen     arcs in the ink — one quiet monochrome mark
  standby   connected, silent     arcs green
  live      audio playing         arcs a brighter green
Colour is what survives 16px; an arc-count difference does not.

The static brand mark (`app-brand`) keeps the coral arcs and is what the
exe, installer and shortcut icon use; only the running app swaps colour.
"""
import math
import os

OUT = os.path.dirname(os.path.abspath(__file__))
# The laptop is always drawn in the ink; so are the arcs when there is
# nothing to report, which keeps idle a single coherent monochrome mark
# instead of grey arcs that wash out against a mid-tone background.
INK = "#545cc4"
# Green means a speaker is connected. Both connected states use it — the
# icon's job is to answer "is this working?", and it cannot answer that if
# the green only appears in the half-second before audio starts flowing.
# Playing is the brighter of the two.
GREEN, GREEN_WAITING = "#22c55e", "#15a349"
# Coral is the brand, not a state: it is what the exe, installer and
# shortcut icon wear, so the mark stays coral wherever it is not reporting
# anything live.
CORAL = "#fe6045"


def arc(cx, cy, r, a0, a1):
    """Short arc from a0 to a1 (degrees, 0 = east, CCW positive).

    SVG y grows downward, so travelling from a high angle to a low one is a
    CLOCKWISE sweep on screen: sweep-flag 1. Getting this wrong bulges the
    arc back toward its centre and the set renders as one crescent blob.
    """
    x0, y0 = cx + r * math.cos(math.radians(a0)), cy - r * math.sin(math.radians(a0))
    x1, y1 = cx + r * math.cos(math.radians(a1)), cy - r * math.sin(math.radians(a1))
    return f"M {x0:.1f} {y0:.1f} A {r} {r} 0 0 1 {x1:.1f} {y1:.1f}"


def laptop(sw, ink=INK):
    """Screen outline and base wedge.

    Every number here was fitted rather than chosen: fit.mjs renders this
    geometry, scores it against reference.png by intersection-over-union and
    runs coordinate descent until nothing improves. These values are that
    search's fixed point (0.78 on the laptop, 0.80 on the arcs — every part
    of the mark lands within 3px of the reference). Nudging by eye had
    stalled at 0.46, so re-run the fit after changing the reference instead
    of adjusting these by hand.
    """
    return (
        f'<g fill="none" stroke="{ink}" stroke-width="{sw}" '
        f'stroke-linejoin="round" stroke-linecap="round">'
        f'<rect x="43" y="133" width="94" height="66" rx="3"/>'
        f'<path d="M 43 199 L 137 199 '
        f'L 149 225 '
        f'L 31 225 Z"/>'
        f'</g>'
    )


def arcs(colour, sw, opacities=(1, 1, 1)):
    """Three concentric sweeps, centre and radii fitted to the reference.

    The radii are evenly spaced and the arcs share one angular span. A later
    fitting pass that let each arc keep its own radius and end angles scored
    0.007 higher, but only by reproducing the wobble in the hand-made
    reference — regular beats marginally closer for a mark that has to
    survive being drawn at 16px.
    """
    cx, cy = 97, 160
    return "".join(
        f'<path d="{arc(cx, cy, r, 82, 9)}" fill="none" stroke="{colour}" '
        f'stroke-width="{sw}" stroke-linecap="round"' + (f' opacity="{o}"' if o != 1 else "") + "/>"
        for r, o in zip((71, 101, 131), opacities)
    )


def write(path, body):
    head = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">'
    open(path, "w").write(head + body + "</svg>")


STATES = [("idle", INK), ("standby", GREEN_WAITING), ("live", GREEN)]

for family, sw, arc_sw in [("app", 11, 15), ("tray", 13, 17)]:
    for name, colour in STATES:
        write(f"{OUT}/{family}-{name}.svg", laptop(sw) + arcs(colour, arc_sw))

write(f"{OUT}/app-brand.svg", laptop(11) + arcs(CORAL, 15))

# The tray icon of the panel by the clock: the arcs say where the sound
# is (ink: this PC; green: a speaker; green with the two outer arcs faint:
# connecting), and a badge that differs in shape as well as colour says
# something needs a look (amber "!") or doesn't work (red "x"). Each in a
# version for a light taskbar and one for a dark taskbar, where the ink
# turns light and the green brighter.
TASKBARS = {
    "light": {"ink": INK, "green": GREEN_WAITING, "ring": "#f0f0f0"},
    "dark": {"ink": "#c9ccf4", "green": "#3ddc84", "ring": "#202020"},
}
PLACES = {
    "pc": lambda t: arcs(t["ink"], 17),
    "connecting": lambda t: arcs(t["green"], 17, (1, 0.35, 0.15)),
    "speaker": lambda t: arcs(t["green"], 17),
}


def badge(kind, ring):
    """A round badge in the bottom right corner, ringed in the taskbar's
    colour so it stands off the mark."""
    cx = cy = 196
    if kind == "attention":
        fill, mark = "#f2a900", (
            '<path d="M 196 160 L 196 202" stroke="#1b1b1b" stroke-width="18" stroke-linecap="round"/>'
            '<circle cx="196" cy="230" r="10" fill="#1b1b1b"/>'
        )
    else:
        fill, mark = "#c42b1c", (
            '<path d="M 174 174 L 218 218 M 218 174 L 174 218" stroke="#ffffff" stroke-width="16" stroke-linecap="round"/>'
        )
    return f'<circle cx="{cx}" cy="{cy}" r="58" fill="{fill}" stroke="{ring}" stroke-width="12"/>' + mark


count = 7
for taskbar, t in TASKBARS.items():
    for place, draw in PLACES.items():
        for kind in ("none", "attention", "failure"):
            body = laptop(13, t["ink"]) + draw(t)
            if kind != "none":
                body += badge(kind, t["ring"])
            write(f"{OUT}/panel-tray-{place}-{kind}-{taskbar}.svg", body)
            count += 1

print("wrote", count, "svgs to", OUT)
