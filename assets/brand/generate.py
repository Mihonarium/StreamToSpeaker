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
  live      audio playing         arcs coral
Colour is what survives 16px; an arc-count difference does not.
"""
import math
import os

OUT = os.path.dirname(os.path.abspath(__file__))
# The laptop is always drawn in the ink; so are the arcs when there is
# nothing to report, which keeps idle a single coherent monochrome mark
# instead of grey arcs that wash out against a mid-tone background.
INK = "#545cc4"
# State colours: green for connected and ready, coral for playing.
GREEN, CORAL = "#22c55e", "#fe6045"


def arc(cx, cy, r, a0, a1):
    """Short arc from a0 to a1 (degrees, 0 = east, CCW positive).

    SVG y grows downward, so travelling from a high angle to a low one is a
    CLOCKWISE sweep on screen: sweep-flag 1. Getting this wrong bulges the
    arc back toward its centre and the set renders as one crescent blob.
    """
    x0, y0 = cx + r * math.cos(math.radians(a0)), cy - r * math.sin(math.radians(a0))
    x1, y1 = cx + r * math.cos(math.radians(a1)), cy - r * math.sin(math.radians(a1))
    return f"M {x0:.1f} {y0:.1f} A {r} {r} 0 0 1 {x1:.1f} {y1:.1f}"


def laptop(sw):
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
        f'<g fill="none" stroke="{INK}" stroke-width="{sw}" '
        f'stroke-linejoin="round" stroke-linecap="round">'
        f'<rect x="43" y="133" width="94" height="66" rx="3"/>'
        f'<path d="M 43 199 L 137 199 '
        f'L 149 225 '
        f'L 31 225 Z"/>'
        f'</g>'
    )


def arcs(colour, sw):
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
        f'stroke-width="{sw}" stroke-linecap="round"/>'
        for r in (71, 101, 131)
    )


def write(path, body):
    head = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">'
    open(path, "w").write(head + body + "</svg>")


STATES = [("idle", INK), ("standby", GREEN), ("live", CORAL)]

for family, sw, arc_sw in [("app", 11, 15), ("tray", 13, 17)]:
    for name, colour in STATES:
        write(f"{OUT}/{family}-{name}.svg", laptop(sw) + arcs(colour, arc_sw))

print("wrote 6 svgs to", OUT)
