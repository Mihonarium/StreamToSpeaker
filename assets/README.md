# Brand assets

The mark is **geometry, not pixels**: `brand/generate.py` writes the six SVGs,
and every raster below is rendered from them. Colours, stroke weight and the
arc set are parameters in that file — change them there, never in a bitmap.

## States

The shape never changes; only the arc colour does. Shape is what makes an icon
recognisable, colour is what still reads at 16 px.

| state | meaning | arcs |
| --- | --- | --- |
| `idle` | no speaker chosen | receded (tile) / grey (tray) |
| `standby` | speaker held, nothing playing | white (tile) / blue (tray) |
| `live` | audio streaming | coral |

## Files

| file | used by |
| --- | --- |
| `StreamToSpeaker.ico` | the exe resource (`service/build.rs`), `SetupIconFile`, and every shortcut |
| `window-*-128.rgba` | window and taskbar icon, swapped at runtime as state changes |
| `tray-*-32.rgba` | system-tray icon, swapped the same way |
| `brand/*.svg` | the source of all of the above |

The `.rgba` files are raw pixels, which is exactly what `egui::IconData` and
`Icon::from_rgba` want — shipping PNGs instead would mean carrying an image
decoder in the binary to load three small bitmaps.

## Regenerating

```
python3 assets/brand/generate.py        # SVGs
```

Rasters are produced by rendering those SVGs at each size and writing the
`.ico` and `.rgba` files. Any SVG renderer will do; the committed set was
produced with headless Chromium at 1× so the geometry is interpreted exactly
as authored.
