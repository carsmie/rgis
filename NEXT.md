# Handoff: where rGIS stands and what's next

Written 2026-10-18 when moving development from macOS to Windows. Read README.md first
(what is supported, parity numbers, limitations), then this.

## State

* Vector rendering, rasters (GDAL), WMS/WFS/OGC API: done; 123 of 126 layers of the QGIS
  server test projects within 0.5 % of QGIS Server (`tools/parity.py`).
* Labels (`src/labeling/`, `src/text.rs`): point placements (over point, around point,
  ordered positions), data-defined position/rotation, buffers, obstacles, the full PAL
  solver incl. QGIS's own R-tree (`labeling/rtree.rs`) so ties resolve like QGIS. All label
  layers of the test projects are 0.00 % on macOS. 7× faster than QGIS with 3 000 labels.

## Next steps (in order)

1. **Line labels** — port `FeaturePart::createCandidatesAlongLine*` (parallel / line
   placement, `placementFlags` above/below/on line), then `createHorizontalCandidatesAlongLine`
   and curved labels (`createCurvedCandidatesAlongLine`, `curvedPlacementAtOffset`,
   needs per-character metrics). Source: `QGIS/src/core/pal/feature.cpp` 871–2147.
   Line parts need `PointSet` helpers (`pointset.cpp`: `getPointByDistance`,
   `extendLineByDistance`, `edgeDistances`) and `addSizePenalty` (already ported).
2. **Polygon labels** — `createCandidatesForPolygon` (free/horizontal, `splitPolygons`,
   oriented bbox via `computeConvexHullOrientedBoundingBox`), ring/centroid distance costs
   (TODO in `labeling/pal.rs::finalize_costs`), `createCandidatesOutsidePolygon`,
   `GeomFunction::reorderPolygon` (convex hull order), proper `GEOSPointOnSurface`.
3. Test projects to verify with: find layers with `placement="2|3|4|5|7|8"` in
   `QGIS/tests/testdata/qgis_server/*.qgs`; if none, make copies of a line/polygon project
   with labels enabled (see `make_rect_project.py` idea below).
4. Label extras: line wrapping (`autoWrapLength`, `QgsStringUtils::wordWrap`), backgrounds,
   shadows, callouts, `formatNumbers`, more data-defined properties (font, buffer).
5. Stretched GetMap (BBOX aspect ≠ WIDTH/HEIGHT): Qt `SmoothTransformation` area averaging
   instead of bilinear (`render::scale`); explains the remaining 0.77 % / 0.87 % layers.
6. Housekeeping: add a LICENSE (should be GPL-2.0-or-later — PAL/QGIS code is ported);
   GitHub may still serve the pre-rewrite commit f429e75 by SHA (ask GitHub support or
   recreate the repo if that matters).

## New machine: Windows with WSL + Docker

Work inside WSL (Ubuntu); keep the repo and the QGIS checkout in the WSL filesystem
(`~/…`, not `/mnt/c/…` — much faster builds and file access).

* **Build in WSL:**
  `sudo apt install build-essential pkg-config libclang-dev libgdal-dev gdal-bin libfontconfig1-dev`
  then `cargo build --release` (the `raster` feature uses the distro GDAL through
  `gdal-sys` + bindgen). Without GDAL: `cargo build --release --no-default-features`.
  `cargo test` — the `#[cfg(target_os = "macos")]` text tests are skipped on Linux.
* **Reference QGIS Server in Docker:** use a QGIS Server image (e.g. the official
  `qgis/qgis-server` images, or `camptocamp/qgis-server`; check the current tags and how
  each passes the project — `QGIS_PROJECT_FILE` env or `MAP=` parameter). Mount the QGIS test
  data (`QGIS/tests/testdata/qgis_server`) and `demo/` read-only at the **same paths** inside
  the container as in WSL, so the `MAP=` paths `tools/parity.py` sends work for both servers.
  Publish it on `localhost:8099`; run rGIS on `:8098`
  (`./target/release/rgis -p demo/earth_local.qgs 127.0.0.1:8098`). Point the tools at other
  ports with `QGIS_URL=…` / `RGIS_URL=…`. Note the QGIS version of the image (labels were
  verified against 4.3.0-master); differences between QGIS versions are real.
  Run `tools/compare_with_qgis.py` and `tools/parity.py` first to get a baseline on Linux.
* **Fonts — the main thing to sort out:** text metrics are modelled per Qt font engine
  (`src/text.rs`, `Engine`): macOS = CoreText (verified glyph by glyph); Linux = the
  "FreeType" model, which is written but **not yet verified** against Qt. QGIS Server in
  the container uses Qt's FreeType engine with that container's fonts and fontconfig.
  1. rGIS must see the same font files: run rGIS in a container built from the same base
     image / font packages, or install the same packages in WSL (compare `fc-match "Sans
     Serif"`, `fc-match "Liberation Sans:bold"` in both). Missing families fall back to
     fontconfig's sans-serif in Qt; rGIS falls back to `RGIS_LABEL_FONT` (default
     "DejaVu Sans") — make both agree.
  2. Fit the FreeType model like the CoreText one: install PyQt6 in the QGIS container (or a
     container with the same Qt + fonts) and dump `QFontMetricsF` ascent/descent/
     horizontalAdvance, `QRawFont.advancesForGlyphIndexes`, `QTextLayout` glyph positions and
     `QPainterPath.addText` for the label fonts at the label pixel sizes. Things to check:
     hinting (Qt's default hinting on the offscreen platform rounds metrics/advances to
     whole pixels?), ascent/descent rounding, kerning, variable fonts. Adjust
     `SizedFont::{new, engine_adv64}` / `shape()` until `matches_qt_*`-style tests pass.
     `RGIS_TEXT_ENGINE=coretext|freetype` overrides the model.
  QGIS Server loads "QGIS Vera Sans" Roman/Bold itself — bundled in `fonts/`.
* Then re-run label parity (`cdb_labels`, `wms_tile_buffer_labels[_rotated]`) on Linux and
  only then continue with line/polygon labels.

## Tools and tricks that worked

* `python tools/parity.py <outdir> <project.qgs …>` — per-layer pixel diff vs QGIS
  (needs both servers). `examples/compare_png.rs` does the diff.
* Seeing QGIS's label placement: copy a project and set `<PAL><DrawRectOnly>true` (QGIS
  then draws only label rectangles); rGIS has the same with env `RGIS_LABEL_RECTS=1`.
  Make data paths absolute in the copy. Compare rectangles numerically.
* Debug logging: `RUST_LOG=rgis=debug` (label problem size, stage timings, fonts chosen).
* QGIS behaviours that surprised: Qt 6 treats QGIS 3's `fontWeight=75` as weight 75
  (ultra-light); `namedStyle` only applies when the family is installed; labels with
  `$x/$y` positions are fixed (one candidate); PalRtree coordinates are scaled so f32
  volumes underflow to 0 — tie-breaks then decide the tree shape.
