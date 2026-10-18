# rGIS

A QGIS-Server-like OGC map server written in Rust. It opens QGIS project files
(`.qgs` and `.qgz`) and serves them over **WMS 1.3.0/1.1.1**, **WFS 1.1.0** and
**OGC API – Features** (`/wfs3`), reproducing QGIS Server's behaviour closely enough that
its responses can be compared with QGIS Server's side by side.

How QGIS Server itself works — with source line references and runtime logs — is
documented in [EVIDENCE.md](EVIDENCE.md). A benchmark against QGIS Server is in
[PERFORMANCE.md](PERFORMANCE.md): one Rust process serves ~2,100 map tiles/s at 80 MB,
vs ~1,400/s for 10 QGIS Server processes at ~1.5 GB, and is as fast or faster per request.

## Quick start

```sh
cargo build --release
./target/release/rgis -p demo/earth_local.qgs 127.0.0.1:8000

# what did the server understand from the project?
./target/release/rgis --inspect demo/earth_local.qgz
```

```
http://127.0.0.1:8000/?SERVICE=WMS&REQUEST=GetCapabilities
http://127.0.0.1:8000/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:3857&BBOX=-1500000,4000000,4500000,11000000&WIDTH=600&HEIGHT=700&FORMAT=image/png
http://127.0.0.1:8000/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=Earth&QUERY_LAYERS=ne_10m_admin_0_countries&CRS=EPSG:4326&BBOX=50,0,70,20&WIDTH=200&HEIGHT=200&I=100&J=100&INFO_FORMAT=application/json
http://127.0.0.1:8000/?MAP=/abs/path/demo/earth_local_wfs.qgs&SERVICE=WFS&REQUEST=GetFeature&TYPENAME=ne_10m_lakes&MAXFEATURES=2&OUTPUTFORMAT=application/json
http://127.0.0.1:8000/wfs3/collections/ne_10m_admin_0_countries/items?MAP=/abs/path/demo/earth_local_wfs.qgs&bbox=5,58,10,62
```

The original `~/Projekte/kartverket/earth.qgs` (FlatGeobuf over `/vsicurl/https://s3-rin.statkart.no/…`)
works unchanged where that bucket is reachable. `demo/earth_local.qgs` is the same project
with the data sources pointing at local copies in `demo/natural_earth/` (built from QGIS's
`world_map.gpkg` and GDAL's Natural Earth test data, since the bucket was not reachable
from the build machine).

Options: `-p/--project FILE` (default project when no `MAP`), `--projects-dir DIR`
(restrict `MAP` to a directory — QGIS has no such guard), `QGIS_PROJECT_FILE` (fixed
project that overrides `MAP`, as in QGIS), `QGIS_SERVER_WMS_MAX_WIDTH/HEIGHT`,
`QGIS_SERVER_IGNORE_RENDERING_ERRORS`, `RUST_LOG`.

## Architecture: QGIS Server → this crate

| QGIS Server | rGIS |
|---|---|
| `qgis_mapserver` / FastCGI loop calling `QgsServer::handleRequest` | `main.rs` (axum; each request runs on a blocking thread) → `server::Server::handle` |
| `QgsServerParameters` (case-insensitive KVP) | `params.rs` |
| `QgsServer::configPath` (`QGIS_PROJECT_FILE` > `MAP` > default) | `Server::config_path` |
| `QgsConfigCache` ('filesystem' strategy) | `server::ConfigCache` (reload when the file's mtime changes) |
| `QgsServiceRegistry` + `QgsServiceNativeLoader` (`libwms.so`, …) | `server::ServiceRegistry`; modules compiled in, registered per (name, version), default-version fallback |
| `apiForRequest` when `SERVICE` is empty (WFS3 / OGC API) | `services/oapif.rs` mounted at `/wfs3` |
| `QgsProject::read` / `unzip` (.qgz) | `project.rs` (roxmltree; `.qgz` via `zip`) |
| `QgsServerProjectUtils` (`WMSRootName`, `WFSLayers`, …) | `Project::{root_name, wfs_layers, crs_list, …}` |
| `QgsWmsRenderContext::layerNickname`, group expansion, layer order reversal | `Project::{nickname, resolve}`, `wms::layers_from` |
| OGR provider (FlatGeobuf, GPKG, SHP, GeoJSON, `/vsicurl/`) | `provider.rs` (flatgeobuf + packed R-tree, rusqlite + GPKG R-tree, shapefile, serde_json; HTTP range reads with a block cache) |
| `QgsCoordinateReferenceSystem` / `QgsCoordinateTransform` (PROJ) | `crs.rs` (proj4rs, pure Rust; EPSG definitions from crs-definitions) |
| `QgsFeatureRenderer` / `QgsSymbol` / `QgsExpression` | `symbology.rs`, `expression.rs` |
| `QgsMapRendererJob` + `QPainter` on `QImage` | `render.rs` (tiny-skia, anti-aliased) |
| `QgsPalLayerSettings`, `QgsLabelingEngine`, PAL (`src/core/pal`) | `labeling/` (settings, registration, placement, FALP + chain solver, the PAL R-tree) |
| `QgsTextRenderer` + Qt font engine (CoreText / FreeType) | `text.rs` (skrifa outlines, harfrust shaping, Qt's metric rounding) |
| `services/wms/*` | `services/wms.rs` |
| `services/wfs/*` | `services/wfs.rs` |

## What is supported

* Projects: `.qgs`, `.qgz`; QGIS 3 and QGIS 4 property encodings (`<Option>` maps, legacy
  `<prop k v>`, `<properties name=…>`); relative and absolute data source paths; project and
  layer variables; opaque layer-tree groups; several layers sharing one WMS name.
  The XML is parsed once at load into typed structures; requests never touch XML (QGIS
  Server, by contrast, keeps a parsed DOM per project in `QgsConfigCache`).
* Vector data: OGR provider with FlatGeobuf (local or `/vsicurl/` HTTP range requests, spatial
  index used), GeoPackage (R-tree used, `subset=` filters), Shapefile, GeoJSON, CSV (WKT).
* Raster data (feature `raster`, on by default): GDAL provider — single-band gray and multiband
  colour renderers with contrast enhancement, transparency, alpha bands, reprojection and
  warped VRTs, ported from QGIS's raster pipeline.
* Symbology: single symbol, categorized, graduated, rule-based (filters, ELSE, scale ranges,
  nested rules), null; SimpleFill (incl. Qt brush patterns), SimpleLine, SimpleMarker,
  EllipseMarker, SvgMarker (QGIS SVG search paths, `param()` substitution), LinePatternFill;
  data-defined properties (static, field, expression); render-time simplification and
  clipping exactly as QGIS does; layer opacity; scale-based layer visibility.
* Labels: simple and rule-based labeling for point layers (and polygon centroids) with
  over-point, around-point and ordered-position placement; fixed (data-defined) positions and
  rotation; text buffers; obstacles; PAL's candidate costs, conflict graph and FALP + chain
  search, ported from QGIS so the same labels win. Text is laid out with Qt's numbers: font
  metrics in 26.6 fixed point, HarfBuzz kerning and `trak` tracking as Qt applies them, Qt 6
  weight semantics. QGIS Server's bundled "QGIS Vera Sans" fonts are built in.
* Expressions: comparisons, `AND/OR/NOT`, `IN`, `IS [NOT] NULL`, `LIKE/ILIKE`, arithmetic, `||`,
  `lower/upper/length/coalesce/to_int/to_real/round/concat/…`.
* WMS: GetCapabilities (1.3.0 & 1.1.1), GetMap (PNG; `LAYERS` with groups, `CRS`/`SRS`,
  axis order, `CRS:84`, `TRANSPARENT`, `BGCOLOR`, `DPI`, `OPACITIES`, `FILTER`, QGIS's
  aspect-ratio handling), GetFeatureInfo (`application/json`, `text/xml`, `text/plain`,
  `text/html`; QGIS tolerances; `FEATURE_COUNT`, `WITH_GEOMETRY`, `FILTER`),
  GetLegendGraphic (JSON with base64 icons; PNG with symbol patches).
* WFS 1.1.0: GetCapabilities, DescribeFeatureType, GetFeature (GML 3.1.1 or GeoJSON;
  `TYPENAME`, `FEATUREID`, `BBOX`, `EXP_FILTER`, `PROPERTYNAME`, `MAXFEATURES`, `STARTINDEX`,
  `SRSNAME`; `WFSLayersPrecision`; `HideFromWfs` fields).
* OGC API Features: landing page, conformance, collections, items (`bbox`, `limit`, `offset`,
  attribute equality filters, paging links), single items.
* OGC exception reports matching QGIS's codes, messages and HTTP statuses.

## Verification against the real QGIS Server

QGIS Server 4.3.0 (built from the local checkout) and rGIS were run side by side on
the same projects. Tools: `tools/compare_with_qgis.py`, `tools/render_projects.py`,
`cargo run --release --example compare_png`.

**Protocol level — `tools/compare_with_qgis.py`: 28/28 checks pass**, including: identical
WMS layer tree, queryable flags, CRS lists and bounding boxes (1.3.0 and 1.1.1); GetFeatureInfo
JSON byte-identical (same feature, FID and properties, in EPSG:4326 and EPSG:3857); WFS
GetFeature with `BBOX`, `FEATUREID`, `EXP_FILTER`, `PROPERTYNAME`, `STARTINDEX` returning
identical ids, properties and geometries; GML 3 coordinates in lat/lon order matching to 1e-6;
DescribeFeatureType; error codes and HTTP statuses.

**Rendering — `tools/parity.py`** renders every published layer of all 58 QGIS server test
projects with both servers and diffs them: **126 layers, 123 within 0.5 % differing pixels,
all label layers 0.00 %**. The one layer above 2 % is an SVG marker whose file is missing on
this machine (QGIS's "?" placeholder; with QGIS's SVG library available on both servers the
layer is 0.00 %).

Selected results (share of pixels differing by more than 64/255 in any channel):

| Project / request | Renderer, data | Differing pixels |
|---|---|---|
| `earth.qgs`, world, EPSG:4326 (WMS 1.3.0 lat/lon) | single symbol, FlatGeobuf | 0.49 % |
| `earth.qgs`, Europe, EPSG:3857 | single symbol, FlatGeobuf | 0.49 % |
| QGIS test `test_project_legend_rule.qgs` | rule-based, Shapefile | 0.01 % |
| QGIS test `test_project_wfs.qgs` | single symbol, Shapefile | 0.01 % |
| QGIS test `test_project_mapthemes.qgs` | rule-based, GPKG + subset filters, non-square pixels | 0.39 % |
| QGIS test `bug_gh32020.qgs` | categorized, GeoPackage | 0.21 % |
| QGIS test `test_project_meters_at_scaledsymbols.qgs` | categorized, sizes in meters | 0.00 % |
| QGIS test `test_project_opaque.qgz` `streetarea` | LinePatternFill (QGIS 4 project) | < 0.5 % |
| QGIS test `wms_tile_buffer.qgs` with QGIS's SVG library | SvgMarker | 0.00 % |
| QGIS test `test_project.qgs` `landsat` | raster, gray stretch, UTM → Web Mercator | 0.02 % |
| QGIS test `test_project_wms_grouped_layers.qgs` `osm` | raster, GeoPackage RGBA tiles | 0.00 % |
| QGIS test `test_raster_nodata.qgz` | rotated raster via warped VRT, custom CRS | 0.00 % |
| QGIS test `bug_gh30817_atlas_pk.qgs` | CSV with WKT | 0.00 % |
| QGIS test `test_project_wms_grouped_layers.qgs` `cdb_labels` | labels over point, text buffer | 0.00 % |
| QGIS test `wms_tile_buffer.qgs` labels / rotated labels | data-defined positions, PAL solver | 0.00 % |

Labelling speed (median GetMap, 1024 px wide): the test projects' label layers render
1.4–1.6× faster than QGIS Server; with 3 036 labelled points in view, 1445 ms → 204 ms
(7.1×) and, for rotated labels, 2465 ms → 921 ms (2.7×). The placement itself takes well
under a millisecond for hundreds of labels; most of the remaining time is anti-aliased
filling of glyph outlines and text buffers.

The remaining differences are isolated anti-aliasing pixels along edges (see the `*_diff.png`
images in `evidence/`). `/vsicurl/`: a map rendered from FlatGeobuf over HTTP range requests is
byte-identical to the one rendered from local files (14 range requests for 4 MB of data).

### Deliberate differences

* WMS image size is capped at 10000 px when neither `WMSMaxWidth/Height` nor
  `QGIS_SERVER_WMS_MAX_*` is set (QGIS is unlimited and rendered a 99999 px image).
* Field names are sanitised into valid XML element names in GML (QGIS emits
  `<qgs:feature::id>`, which is not well-formed).

## Limitations

* Labels: line and curved placements, free/horizontal polygon placement, outside-polygon
  placement, callouts, backgrounds, shadows, HTML text, line wrapping and label masks are not
  ported yet. Text metrics follow Qt's CoreText engine on macOS (verified glyph by glyph) and
  its FreeType engine elsewhere (not yet verified against a Linux QGIS Server). When a
  project's font is missing on macOS, Qt falls back to the variable system font, whose
  CoreText advances rGIS reproduces to within 1/64 px but not always exactly; with thousands
  of touching labels this can change which of two equally good labels wins.
* Stretched GetMap requests (BBOX aspect ≠ WIDTH/HEIGHT) are resized bilinearly, not with
  Qt's area-averaging `SmoothTransformation`, so thin lines differ there.
* Raster: paletted, pseudocolour and hillshade renderers, non-nearest resampling, and
  raster GetFeatureInfo are not implemented yet.
* Not implemented: diagrams, 3D, print layouts (GetPrint), DXF/PDF output, WMTS/WCS, WFS-T,
  Python server plugins, access control; PostGIS and other non-file providers;
  GeometryGenerator, MarkerLine, PointPatternFill and other less common symbol layers
  (skipped with a warning); blend modes and paint effects.
* PNG output only; GetLegendGraphic PNG has symbol patches without text (use
  `FORMAT=application/json` for titles).
* The expression engine covers the common subset listed above, not all QGIS functions.

## Development

```sh
cargo test                     # unit tests (CRS, expressions, symbology, geometry, …)
# side-by-side checks need QGIS Server on :8099 and this server on :8098:
python3 tools/compare_with_qgis.py "$PWD/demo/earth_local_wfs.qgs"
python3 tools/render_projects.py out/ path/to/project.qgs …
cargo run --release --example compare_png -- a.png b.png diff.png
python3 tools/range_server.py demo/natural_earth 8097   # local S3-like range server
```
