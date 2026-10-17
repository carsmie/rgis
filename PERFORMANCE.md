# Performance: rGIS vs QGIS Server

Measured on 2026-10-17 on an Apple M5 (10 cores), both servers on the same machine and
project (`demo/earth_local.qgs`: Natural Earth countries, lakes and boundaries as local
FlatGeobuf files). QGIS Server 4.3.0-Master is a `Release` build from
`~/Projekte/kartverket/QGIS/build-release`; rGIS is `cargo build --release`.
The load generator is `examples/bench.rs`; workloads come from `tools/make_bench_urls.py`
and the matrix from `tools/run_bench.py`.

## How QGIS Server handles concurrency (this matters for fairness)

`qgis_mapserver` processes **one request at a time**. Every request takes a global
`SERVER_MUTEX` and runs `server.handleRequest` on the main thread
(`src/server/qgis_mapserver.cpp:646-650`). Production deployments scale with **several
FastCGI processes**, one per core. So QGIS appears below in two forms: one process
(×1), and 10 processes behind round-robin (×10, like a typical FastCGI pool on this
machine). rGIS is always **one process** that renders on a thread pool.

## Startup

| | QGIS Server | rGIS |
|---|---|---|
| Launch → project loaded → first GetCapabilities answered | 0.8–1.1 s | 0.02 s |
| First (cold) 600×700 GetMap | 98 ms | 51 ms |

## Latency, one client, sequential requests (p50 / p95)

| Request | QGIS Server | rGIS | Speed-up (p50) |
|---|---|---|---|
| WMS GetCapabilities | 0.4 / 0.5 ms | 0.1 / 0.2 ms | ~3× |
| GetMap world, 800×400, EPSG:4326 | 39.1 / 40.1 ms | 37.3 / 39.0 ms | 1.05× |
| GetMap Europe, 600×700, EPSG:3857 | 37.0 / 38.3 ms | 32.5 / 33.7 ms | 1.14× |
| GetMap 256×256 tiles, zoom 3–8, EPSG:3857 (3,544 distinct tiles) | 4.7 / 6.3 ms | 2.8 / 4.2 ms | 1.7× |
| GetFeatureInfo JSON, 2 layers (200 positions) | 2.1 / 2.5 ms | 1.2 / 1.4 ms | 1.75× |
| WFS GetFeature, all 768 lakes as GeoJSON (650 KB) | 16.0 / 16.5 ms | 10.0 / 10.5 ms | 1.6× |
| WFS GetFeature with BBOX (Norway, 150 KB) | 4.6 / 4.9 ms | 2.8 / 2.9 ms | 1.6× |

## Throughput, tile workload, concurrent clients

| Clients | QGIS ×1 | QGIS ×10 processes | rGIS ×1 |
|---|---|---|---|
| 1 | 202 req/s (p50 4.7 ms) | 183 req/s (p50 5.3 ms) | 344 req/s (p50 2.8 ms) |
| 4 | 213 req/s (p50 18.6 ms) | 749 req/s (p50 5.1 ms) | 1,292 req/s (p50 3.0 ms) |
| 10 | 214 req/s (p50 41.9 ms) | 1,302 req/s (p50 7.2 ms) | 2,124 req/s (p50 4.4 ms) |
| 32 | 214 req/s (p50 142 ms, p99 260 ms) | 1,396 req/s (p50 19.2 ms, p99 93 ms) | 2,117 req/s (p50 12.1 ms, p99 56 ms) |

No request failed in any run.

## Memory (resident set size after the load tests)

| | QGIS Server | rGIS |
|---|---|---|
| One process | 148–178 MB | 80 MB after the 32-client runs (16 MB after light use) |
| Setup with the throughput above | 10 processes ≈ 1.5 GB | 1 process ≈ 80 MB |
| Binary and dependencies | `qgis_mapserver` links 134 libraries: QGIS frameworks (147 MB) + 2.5 MB service modules + 113 Homebrew libraries (Qt, GDAL, PROJ, …) | 12.8 MB single binary; links only macOS system libraries (libSystem, Security, CoreFoundation, libiconv) |

## What it took to get here

The first version rendered **more slowly** than QGIS (tiles: 8.8 ms vs 4.7 ms p50). Profiling
showed fetching was cheap (~0.9 ms), but reprojecting and drawing every vertex of the
detailed coastlines was not. The fixes copy what QGIS does:

1. **Render-time simplification in the layer CRS** before reprojecting
   (`simplifyDrawingHints` / `simplifyDrawingTol`, QGIS's "Distance" algorithm, rings
   smaller than the tolerance collapse to their box). This also moved the output *closer*
   to QGIS's: pixel difference on the earth maps went from 0.72 %/0.56 % to 0.49 %/0.49 %.
2. **Clipping to the view** with a margin (QGIS's `clip_to_extent`), so a tile touching a
   corner of Russia doesn't draw all of Russia.
3. **An exact fast path for EPSG:4326 ↔ EPSG:3857** (closed-form spherical Mercator,
   verified against proj4rs to <1 µm): 13 ns instead of 136 ns per point. Other CRS
   pairs still go through proj4rs.

## Caveats

* This is one dataset with simple symbology on one machine. QGIS's costs grow with
  features it has and this server lacks (labels, raster layers, complex symbol layers);
  a project that uses them is not a like-for-like comparison.
* QGIS was measured through its development server (`qgis_mapserver`). Behind
  nginx + FastCGI the per-request overhead is slightly different, but rendering is the
  same code path, and the ×10 setup models a FastCGI process pool.
* PNG encoding settings differ slightly (output sizes are within a few percent of each other).
