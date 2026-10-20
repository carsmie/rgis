# rGIS vs QGIS Server on the Kartverket topo projects (tnt-topo-qlr)

Renders the production topo QLRs (`tnt-topo-qlr/farge/lag*.qlr`, `_test` variants skipped)
with the prod QGIS Server image and with rGIS, side by side, over a local extract of the
data around a centre point (default Oslo, 24 × 24 km, EPSG:25833).

```sh
gcloud auth login                                    # token for the GCS extract
python3 tools/topo/prepare.py --qlr-repo ../tnt-topo-qlr --dry-run   # objects + full sizes
python3 tools/topo/prepare.py --qlr-repo ../tnt-topo-qlr             # convert + extract + fonts
docker build -t rgis .
OUT=$PWD/out/topo docker compose -f tools/topo/compose.yml up -d     # QGIS :8099, rGIS :8098
python3 tools/topo/compare.py                        # out/topo/report-projects/report.md
python3 tools/topo/compare.py --set projects-nolabels                # symbology only
docker compose -f tools/topo/compose.yml down
```

* **Data**: each `/vsigs/topo-2025_prod_0i4s/...` object is cut to the box with `ogr2ogr -spat`
  (FlatGeobuf R-tree: only index nodes and features in the box are read) or
  `gdal_translate -projwin`, through `/vsicurl/` + Bearer token. A 24 km box of
  `n50_arealdekke_flate.fgb` (5.4 GB) is 7 MB. Files that exist are not fetched again.
* **Dropped**: PostGIS layers (on-prem `fkb5`, lag26/29/30) are removed from the test
  projects; their QLRs contain plaintext credentials, and the raw conversions are deleted.
* **Reference**: `tnt-qgis:fork-test` (QGIS 4.3.0-master, GDAL 3.14dev) run as
  `qgis_mapserver` with the image's prod `QGIS_SERVER_*` env. rGIS mounts that image's
  fonts (`out/topo/fonts`) and falls back to Noto Sans like Qt/fontconfig there.
* **Requests**: one 512 px GetMap of each project's top group per utm33n zoom level
  (`--zooms 9-15`), transparent PNG, EPSG:25833. Diff = share of pixels with any channel
  off by more than 64/255 (as `tools/parity.py`).
* `IGNORE_RENDERING_ERRORS=1` (compose env) renders around missing data files; without it
  both servers answer 500 for a request touching a layer whose source is missing.
* `--center x,y --half m` for another area; `--only lag06,lag10` for a subset.

Expected gaps (rGIS, see the main README): line/curved labels (most topo labels are
`placement` 2/3), `ShapeburstFill` (lag06) and other unsupported symbol layers (logged as
warnings: `docker logs topo-rgis-1`), paletted/pseudocolour/hillshade raster renderers.
