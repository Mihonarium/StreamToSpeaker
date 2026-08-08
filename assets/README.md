# Brand assets

The mark is **geometry, not pixels**: `brand/generate.py` writes the six SVGs,
and every raster below is rendered from them. Colours, stroke weight and the
arc set are parameters in that file — change them there, never in a bitmap.

## States

The shape never changes; only the arc colour does. Shape is what makes an icon
recognisable, colour is what still reads at 16 px.

| state | meaning | arcs |
| --- | --- | --- |
| `idle` | no speaker chosen | ink — the whole mark goes monochrome |
| `standby` | speaker held, nothing playing | green |
| `live` | audio streaming | coral |

Every icon is transparent, which fixes the ink. White would disappear in
Explorer and the installer; the brand indigo would disappear on a dark
taskbar. `#545cc4` is the mid-tone that survives white, Explorer grey, mid
grey, taskbar dark and black alike.

## Files

| file | used by |
| --- | --- |
| `StreamToSpeaker.ico` | the exe resource (`service/build.rs`), `SetupIconFile`, and every shortcut |
| `window-*-128.rgba` | window and taskbar icon, swapped at runtime as state changes |
| `tray-*-32.rgba` | system-tray icon, swapped the same way |
| `brand/*.svg` | the source of all of the above |
| `brand/reference.png` | the concept art the geometry is fitted to |

The `.rgba` files are raw pixels, which is exactly what `egui::IconData` and
`Icon::from_rgba` want — shipping PNGs instead would mean carrying an image
decoder in the binary to load three small bitmaps.

## Regenerating

```
python3 assets/brand/generate.py        # SVGs
```

The geometry in `generate.py` is fitted to `brand/reference.png`, not chosen
by hand. To re-derive it — after changing the reference, or to check the
committed numbers are still the best ones:

```
CHROME=/path/to/chrome node assets/brand/fit.mjs           # search
CHROME=/path/to/chrome node assets/brand/fit.mjs --score   # score what ships
```

Rasters are produced by rendering those SVGs at each size and writing the
`.ico` and `.rgba` files. Any SVG renderer will do; the committed set was
produced with headless Chromium at 1× so the geometry is interpreted exactly
as authored.
