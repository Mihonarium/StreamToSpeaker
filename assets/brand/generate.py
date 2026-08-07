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
    """Screen + base, drawn as strokes so it stays crisp when scaled down."""
    return (
        f'<g fill="none" stroke="{colour}" stroke-width="{sw}" '
        f'stroke-linejoin="round" stroke-linecap="round">'
        f'<rect x="30" y="120" width="96" height="66" rx="9"/>'
        f'<path d="M 17 200 h 122"/>'
        f'</g>'
    )


def arcs(colour, n, sw):
    """Concentric arcs springing from the screen's top-right corner."""
    origin = (130, 118)
    out = [f'<circle cx="{origin[0]}" cy="{origin[1]}" r="{sw*0.6:.1f}" fill="{colour}"/>']
    for i in range(n):
        r = 30 + i * 30
        out.append(
            f'<path d="{arc(origin[0], origin[1], r, 84, 6)}" fill="none" '
            f'stroke="{colour}" stroke-width="{sw}" stroke-linecap="round"/>'
        )
    return "".join(out)


def write(path, body, tile):
    head = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">'
    bg = f'<rect width="256" height="256" rx="56" fill="{INDIGO}"/>' if tile else ""
    open(path, "w").write(head + bg + body + "</svg>")


# ---- app icons: white laptop on the indigo tile -------------------------
for name, colour in [("idle", APP_IDLE), ("standby", APP_STANDBY), ("live", CORAL)]:
    write(f"{OUT}/app-{name}.svg", laptop(WHITE, 13) + arcs(colour, 3, 13), tile=True)

# ---- tray icons: no tile, one ink, sized for 16 px ----------------------
# Heavier strokes and a tighter arc set: at tray size the tile is gone, so
# the mark must carry itself on silhouette alone.
for name, colour in [("idle", TRAY_IDLE), ("standby", TRAY_STANDBY), ("live", CORAL)]:
    write(f"{OUT}/tray-{name}.svg", laptop(TRAY_INK, 17) + arcs(colour, 3, 17), tile=False)

print("wrote 6 svgs to", OUT)
