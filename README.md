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
| `services/wms/*` | `services/wms.rs` |
| `services/wfs/*` | `services/wfs.rs` |

## What is supported

* Projects: `.qgs`, `.qgz`; QGIS 3.x property encodings (`<Option>` maps and legacy `<prop k v>`);
  relative and absolute data source paths.
* Data: OGR provider with FlatGeobuf (local or `/vsicurl/` HTTP range requests, spatial index
  used), GeoPackage (R-tree used, `subset=` filters), Shapefile, GeoJSON.
* Symbology: single symbol, categorized, graduated, rule-based (filters, ELSE, scale ranges,
  nested rules), null; SimpleFill / SimpleLine / SimpleMarker layers (colours, alpha, widths in
  mm/px/pt/map units/meters, dash styles, joins, caps, marker shapes); layer opacity;
  scale-based layer visibility.
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

**Rendering — share of pixels differing by more than 64/255 in any channel:**

| Project / request | Renderer, data | Differing pixels |
|---|---|---|
| `earth.qgs`, world, EPSG:4326 (WMS 1.3.0 lat/lon) | single symbol, FlatGeobuf | 0.49 % |
| `earth.qgs`, Europe, EPSG:3857 | single symbol, FlatGeobuf | 0.49 % |
| QGIS test `test_project_legend_rule.qgs` | rule-based, Shapefile | 0.01 % |
| QGIS test `test_project_wfs.qgs` | single symbol, Shapefile | 0.01 % |
| QGIS test `test_project_mapthemes.qgs` | rule-based, GPKG + subset filters, non-square pixels | 0.39 % |
| QGIS test `bug_gh32020.qgs` | categorized, GeoPackage | 0.21 % |
| QGIS test `test_project_meters_at_scaledsymbols.qgs` | categorized, sizes in meters | 0.00 % |
| QGIS test `test_project_wms_grouped_layers.qgs` (zoomed, EPSG:25832) | nested groups, categorized | 3.09 % — labels and an OSM raster not drawn |

The remaining differences are isolated anti-aliasing pixels along edges (see the `*_diff.png`
images in `evidence/`). `/vsicurl/`: a map rendered from FlatGeobuf over HTTP range requests is
byte-identical to the one rendered from local files (14 range requests for 4 MB of data).

### Deliberate differences

* WMS image size is capped at 10000 px when neither `WMSMaxWidth/Height` nor
  `QGIS_SERVER_WMS_MAX_*` is set (QGIS is unlimited and rendered a 99999 px image).
* Web Mercator bounding boxes are clamped to ±85.0511° (QGIS reports −44 927 326 m for −89.9°).
* Field names are sanitised into valid XML element names in GML (QGIS emits
  `<qgs:feature::id>`, which is not well-formed).

## Limitations

* No labels (no text rendering), no raster layers (skipped with a warning), no diagrams,
  no 3D, no print layouts (GetPrint), no DXF/PDF output, no WMTS/WCS, no WFS-T, no Python
  server plugins or access control.
* Only the `ogr` provider for the formats listed above (no PostGIS, WMS cascading, etc.).
* Symbol layers other than Simple Fill/Line/Marker are approximated or skipped (logged),
  data-defined properties are ignored, hatch patterns are drawn as translucent fills.
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
