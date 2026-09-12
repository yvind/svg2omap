# SVG to OMAP point symbol

This command converts SVG vector artwork into one point symbol in an otherwise empty [OpenOrienteering Mapper](https://www.openorienteering.org/mapper-manual/pages/file_format.html) `.omap` file.

## Build and run

```sh
cargo build --release
./target/release/svg2omap input.svg output.omap
```
or
```sh
cargo run --release input.svg output.omap
```

The output path is optional; without it, `input.svg` becomes `input.omap`.

```text
svg2omap [OPTIONS] <INPUT.svg> [OUTPUT.omap]

--name <NAME>        Point-symbol name (default: input file name)
--code <A[.B[.C]]>   Point-symbol code (default: 900)
--map-scale <N>      Map scale denominator (default: 10000)
--dpi <DPI>          SVG CSS pixel density (default: 96)
--width-mm <MM>      Override the physical SVG viewport width
```

SVG pixels use the CSS standard of 96 DPI, so 96 px becomes 25.4 mm. Physical
SVG units such as `mm` retain their intended size. `--width-mm` scales the whole
symbol proportionally and is useful for SVGs that only have a `viewBox`.

The converter resolves CSS, transforms, path commands, primitive shapes, dashed
strokes, line caps/joins, and text outlines. It centers the SVG viewport at the
point-symbol origin and changes SVG's downward-positive Y axis to Mapper's
upward-positive axis. Strokes are expanded into filled outlines so their SVG
appearance is retained.

Adjacent paint layers with the same RGB color share one OMAP color. If another
color occurs between two equal colors, they remain separate so that the color
table still preserves the SVG drawing order.

OMAP point symbols cannot faithfully represent every SVG feature.
The converter prints warnings when it makes these fallbacks:
gradients become their average color, partial transparency becomes fully opaque,
and clip paths, masks, filters, and blend modes are ignored.
Pattern paints and raster images are skipped.
