#!/usr/bin/env python3
"""Stream To Speaker brand mark, authored as geometry.

Run this to regenerate the SVGs after changing colours or geometry, then
re-render the raster assets (see assets/README.md).

Palette sampled from the chosen concept art:
  indigo #303086, coral #fe6045, white.

Two families:
  app/*   full-colour tile — exe, installer, window, Store
  tray/*  transparent monochrome — system tray, both light and dark taskbars

Three states in each. The mark never changes shape — same laptop, same three
arcs — only the arc COLOUR moves, so the icon stays recognisable while still
reporting what the app is doing:
  idle      no speaker chosen        arcs dimmed into the background
  standby   speaker held, silent     arcs in the neutral ink
  live      audio streaming          arcs coral
Colour is also what survives 16px; an arc-count difference does not.
"""
import math
import os

OUT = os.path.dirname(os.path.abspath(__file__))
INDIGO, CORAL, WHITE = "#303086", "#fe6045", "#ffffff"
# State inks. On the tile, "idle" recedes toward the indigo; in the tray
# there is no tile, so idle is a neutral grey and standby a brand blue that
# both hold up on a light and a dark taskbar.
APP_IDLE, APP_STANDBY = "#575da0", "#ffffff"
TRAY_INK, TRAY_IDLE, TRAY_STANDBY = "#8c94a3", "#8c94a3", "#7b88f5"


def arc(cx, cy, r, a0, a1):
    """Short arc from a0 to a1 (degrees, 0 = east, CCW positive).

    SVG y grows downward, so travelling from a high angle to a low one is a
    CLOCKWISE sweep on screen: sweep-flag 1. Getting this wrong bulges the
    arc back toward its centre and the set renders as one crescent blob.
    """
    x0, y0 = cx + r * math.cos(math.radians(a0)), cy - r * math.sin(math.radians(a0))
    x1, y1 = cx + r * math.cos(math.radians(a1)), cy - r * math.sin(math.radians(a1))
    return f"M {x0:.1f} {y0:.1f} A {r} {r} 0 0 1 {x1:.1f} {y1:.1f}"


def laptop(colour, sw):
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
        f'<g fill="none" stroke="{colour}" stroke-width="{sw}" '
        f'stroke-linejoin="round" stroke-linecap="round">'
        f'<rect x="43" y="133" width="94" height="66" rx="3"/>'
        f'<path d="M 43 199 L 137 199 '
        f'L 149 225 '
        f'L 31 225 Z"/>'
        f'</g>'
    )


def arcs(colour, n, sw):
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
        for r in (71, 101, 131)[:n]
    )


def write(path, body, tile):
    head = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">'
    # Square, not rounded: Windows draws app icons unrounded, so a radius
    # here would just be indigo missing from the corners.
    bg = f'<rect width="256" height="256" fill="{INDIGO}"/>' if tile else ""
    open(path, "w").write(head + bg + body + "</svg>")


# ---- app icons: white laptop on the indigo tile -------------------------
for name, colour in [("idle", APP_IDLE), ("standby", APP_STANDBY), ("live", CORAL)]:
    write(f"{OUT}/app-{name}.svg", laptop(WHITE, 11) + arcs(colour, 3, 15), tile=True)

# ---- tray icons: no tile, one ink, sized for 16 px ----------------------
# Same geometry, heavier strokes: at tray size the tile is gone, so the
# mark has only its silhouette to carry it.
for name, colour in [("idle", TRAY_IDLE), ("standby", TRAY_STANDBY), ("live", CORAL)]:
    write(f"{OUT}/tray-{name}.svg", laptop(TRAY_INK, 13) + arcs(colour, 3, 17), tile=False)

print("wrote 6 svgs to", OUT)
