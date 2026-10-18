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

## Windows specifics (new machine)

* **Build:** the `raster` feature needs GDAL headers/libs + libclang for `gdal-sys`
  (`bindgen`). Quick start without it: `cargo build --release --no-default-features`.
  With OSGeo4W: set `GDAL_HOME`/`GDAL_VERSION` and `LIBCLANG_PATH` (LLVM install).
* **Reference server:** QGIS Server from OSGeo4W (`qgis_mapserver.exe`, or `qgis_mapserv.fcgi`
  behind a web server). Start it on :8099 with the project via `QGIS_PROJECT_FILE`
  or `-p`; rGIS on :8098 (`target\release\rgis -p demo/earth_local.qgs 127.0.0.1:8098`).
  The tools use `MAP=` per request, so one running instance serves all test projects.
  QGIS test data: clone QGIS and use `tests/testdata/qgis_server/`.
* **Fonts — important:** text metrics are modelled per Qt font engine (`src/text.rs`,
  `Engine`). macOS = CoreText (verified glyph by glyph). Everything else currently uses the
  "FreeType" model, but **Qt on Windows uses DirectWrite**, which is NOT modelled yet:
  expect label differences until a `DirectWrite` engine variant exists. Do it the same way
  as for CoreText: dump Qt's numbers with PyQt6 on the same machine
  (`QFontMetricsF.ascent/descent/horizontalAdvance`, `QRawFont.advancesForGlyphIndexes`,
  `QTextLayout.glyphRuns` positions, `QPainterPath.addText`) and fit
  `SizedFont::{new, engine_adv64}` + `shape()` until `matches_qt_*` style tests pass.
  Override for experiments: `RGIS_TEXT_ENGINE=coretext|freetype`.
  Fallback font when a project font is missing: Qt's default on Windows (likely
  "Segoe UI"); set `RGIS_LABEL_FONT` / adjust `resolve_face` accordingly.
  QGIS Server loads "QGIS Vera Sans" Roman/Bold itself — bundled in `fonts/`.
* Tests marked `#[cfg(target_os = "macos")]` in `src/text.rs` only run on macOS.

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
