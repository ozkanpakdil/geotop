# Project assets

## `world-map.svg`

Bundled default world map. It is an equirectangular SVG world map with
country labels, rasterized at startup by `resvg` and used as the base
layer under the live connection dots.

- Source: Wikimedia Commons `BlankMap-World.svg` (public domain),
  with labels added by ahuseyn (<https://github.com/ahuseyn/>).
- License: MIT (per the SVG header).
- Projection: equirectangular, roughly 2:1 aspect ratio.

You can override the bundled map at runtime with `--map-path <PATH>`.
`geotop` supports PNG/JPEG and SVG map paths. If loading fails for any
reason, the renderer falls back to a procedurally generated dark world
map so the dashboard is never blank.
