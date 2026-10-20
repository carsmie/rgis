"""Prepare a QGIS Server vs rGIS test on the tnt-topo-qlr topo projects.

python3 tools/topo/prepare.py --qlr-repo ~/repos/tnt-topo-qlr [--dry-run]

1. Converts every non-test <variant>/lag*.qlr to .qgs with the repo's own qlr_to_qgs.py.
2. Rewrites /vsigs/<bucket>/<obj> datasources to <out>/data/<bucket>/<obj>; drops PostGIS
   layers (on-prem DB, and their QLRs carry plaintext credentials); writes the projects to
   <out>/projects and a copy with all labels off to <out>/projects-nolabels.
3. Extracts the area of interest (--center ± --half, EPSG:25833) of each object from GCS
   via /vsicurl/ + Bearer token (GDAL's /vsigs/ rejects gcloud tokens). FlatGeobuf reads
   only the R-tree nodes and features in the box; rasters are cut with -projwin.
4. Copies the QGIS image's fonts to <out>/fonts so rGIS sees the same font files.

Existing data files are kept; delete <out>/data/... to refetch. Writes <out>/manifest.json.
"""
import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
import struct
import time
import urllib.parse
import urllib.request
from pathlib import Path

from lxml import etree

HERE = Path(__file__).resolve().parent
ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("--qlr-repo", type=Path, required=True)
ap.add_argument("--variant", default="farge")
ap.add_argument("--out", type=Path, default=HERE.parent.parent / "out" / "topo")
# Oslo; half = z9 tile (512 px × 42.3 m) / 2 plus margin for symbols and labels at edges.
ap.add_argument("--center", default="262000,6649000")
ap.add_argument("--half", type=float, default=12000)
ap.add_argument("--only", default="", help="comma-separated QLR name prefixes, e.g. lag10,lag26")
ap.add_argument("--qgis-image", default="tnt-qgis:fork-test")
ap.add_argument("--big-bytes", type=float, default=1e9,
                help="objects larger than this are cut to --big-half instead (detail layers)")
ap.add_argument("--big-half", type=float, default=1000)
ap.add_argument("--jobs", type=int, default=2)  # gentle on the bucket
ap.add_argument("--dry-run", action="store_true", help="convert and list objects + sizes, fetch nothing")
args = ap.parse_args()

out = args.out.resolve()
cx, cy = (float(v) for v in args.center.split(","))
bbox = (cx - args.half, cy - args.half, cx + args.half, cy + args.half)
sys.path.insert(0, str(args.qlr_repo / "scripts"))
from qlr_to_qgs import qlr_to_qgs  # noqa: E402

qlrs = sorted(p for p in (args.qlr_repo / args.variant).glob("lag*.qlr") if "_test" not in p.stem)
if args.only:
    qlrs = [p for p in qlrs if p.stem.startswith(tuple(args.only.split(",")))]
for d in ("raw", "projects", "projects-nolabels"):
    (out / d).mkdir(parents=True, exist_ok=True)

objects = {}  # bucket/obj -> "vector" | "raster"
manifest = {"center": [cx, cy], "half": args.half, "bbox": bbox, "projects": {}}
for qlr in qlrs:
    raw = out / "raw" / f"{qlr.stem}.qgs"
    qlr_to_qgs(qlr, raw)
    tree = etree.parse(str(raw))
    root = tree.getroot()
    skipped, external = [], []
    for ml in list(root.iter("maplayer")):  # snapshot: PostGIS layers are removed
        provider = ml.findtext("provider") or ""
        ds = ml.find("datasource")
        name = ml.findtext("layername")
        if provider == "postgres":
            lid = ml.findtext("id")
            for tl in root.iter("layer-tree-layer"):
                if tl.get("id") == lid:
                    tl.getparent().remove(tl)
            ml.getparent().remove(ml)
            skipped.append(name)
        elif ds is not None and ds.text and ds.text.startswith("/vsigs/"):
            path, sep, rest = ds.text.partition("|")
            obj = path[len("/vsigs/"):]
            objects[obj] = "raster" if provider == "gdal" else "vector"
            ds.text = str(out / "data" / obj) + sep + rest
        elif ds is not None and ds.text:
            external.append(f"{name}: {ds.text.split('|')[0][:80]}")
    for tl in root.iter("layer-tree-layer"):  # informational copy of the datasource
        if (src := tl.get("source") or "").startswith("/vsigs/"):
            tl.set("source", str(out / "data") + src[len("/vsigs") :])
    tree.write(str(out / "projects" / raw.name), xml_declaration=True, encoding="utf-8")
    for ml in root.iter("maplayer"):
        ml.set("labelsEnabled", "0")
    tree.write(str(out / "projects-nolabels" / raw.name), xml_declaration=True, encoding="utf-8")
    manifest["projects"][qlr.stem] = {"skipped_postgres": skipped, "external": external}
    print(f"{qlr.stem}: {len(skipped)} postgres layer(s) dropped, {len(external)} external source(s)")
shutil.rmtree(out / "raw")  # still holds the PostGIS credentials


def token():
    if os.environ.get("GCS_TOKEN"):
        return os.environ["GCS_TOKEN"]
    r = subprocess.run(["gcloud", "auth", "print-access-token"], capture_output=True, text=True)
    if r.returncode:
        sys.exit("no GCS token: run `gcloud auth login` or set GCS_TOKEN\n" + r.stderr)
    return r.stdout.strip()


todo = sorted(o for o in objects if not (out / "data" / o).exists())
print(f"\n{len(objects)} objects referenced, {len(todo)} not yet extracted")
# Object metadata only (no content read): upper bound of what a fetch could transfer.
sizes = {}
if todo:
    r = subprocess.run(["gcloud", "storage", "ls", "-l"] + [f"gs://{o}" for o in todo],
                       capture_output=True, text=True)
    for line in r.stdout.splitlines():
        f = line.split()
        if len(f) == 3 and f[2].startswith("gs://"):
            sizes[f[2][5:]] = int(f[0])
    if missing := [o for o in todo if o not in sizes]:
        print(f"{len(missing)} object(s) not found in GCS: {missing}\n{r.stderr[-300:]}")
for o in todo:
    if o in sizes:
        big = " (big: small box)" if sizes[o] > args.big_bytes else ""
        print(f"  {sizes[o] / 1e6:10.1f} MB  {o}{big}")
if args.dry_run:
    sys.exit(0)

tok = token()
env = dict(os.environ, GDAL_HTTP_HEADERS=f"Authorization: Bearer {tok}",
           GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR")


def read_range(obj, start, end):
    req = urllib.request.Request(f"https://storage.googleapis.com/{urllib.parse.quote(obj)}",
                                 headers={"Authorization": f"Bearer {tok}", "Range": f"bytes={start}-{end}"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.read()


def fgb_index_node_size(obj):
    # FlatGeobuf header only: 8 magic + u32 size + flatbuffer Header. Without an R-tree
    # (index_node_size 0) a bbox filter would read the whole object, so such files are skipped.
    head = read_range(obj, 0, 11)
    hdr = read_range(obj, 12, 11 + struct.unpack_from("<I", head, 8)[0])
    table = struct.unpack_from("<I", hdr, 0)[0]
    vt = table - struct.unpack_from("<i", hdr, table)[0]
    slot = 4 + 2 * 9  # field 9 = index_node_size, default 16
    off = struct.unpack_from("<H", hdr, vt + slot)[0] if slot < struct.unpack_from("<H", hdr, vt)[0] else 0
    return struct.unpack_from("<H", hdr, table + off)[0] if off else 16


def extract(obj):
    dst = out / "data" / obj
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_name(".part-" + dst.name)
    src = f"/vsicurl/https://storage.googleapis.com/{urllib.parse.quote(obj)}"
    half = args.big_half if sizes.get(obj, 0) > args.big_bytes else args.half
    x0, y0, x1, y1 = (str(v) for v in (cx - half, cy - half, cx + half, cy + half))
    if objects[obj] == "vector":
        if not fgb_index_node_size(obj):
            return obj, "SKIPPED: no spatial index (a bbox read would scan the whole object)"
        cmd = ["ogr2ogr", "-f", "FlatGeobuf", "-spat", x0, y0, x1, y1, "-spat_srs", "EPSG:25833",
               "-nln", dst.stem, "-skipfailures", str(tmp), src]  # drops NULL geometries (FGB index)
    else:
        cmd = ["gdal_translate", "-of", "COG", "-projwin", x0, y1, x1, y0,
               "-projwin_srs", "EPSG:25833", src, str(tmp)]
    t = time.time()
    r = subprocess.run(cmd, env=env, capture_output=True, text=True)
    if r.returncode:
        tmp.unlink(missing_ok=True)
        return obj, f"FAILED: {(r.stderr.strip().splitlines() or ['?'])[-1]}"
    tmp.rename(dst)
    return obj, f"{dst.stat().st_size / 1e6:.1f} MB in {time.time() - t:.0f}s"


with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
    for obj, msg in pool.map(extract, [o for o in todo if o in sizes]):
        print(f"  {obj}: {msg}")
        manifest.setdefault("fetch", {})[obj] = msg

fonts = out / "fonts"
if not fonts.exists():
    cid = subprocess.run(["docker", "create", args.qgis_image], capture_output=True, text=True,
                         check=True).stdout.strip()
    try:
        for src, name in (("/usr/share/fonts", "share"), ("/usr/local/share/fonts", "local")):
            subprocess.run(["docker", "cp", f"{cid}:{src}", str(fonts / name)], check=True)
    finally:
        subprocess.run(["docker", "rm", cid], capture_output=True)
(out / "manifest.json").write_text(json.dumps(manifest, indent=1))
print(f"\nready: {out}")
