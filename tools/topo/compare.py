"""Render the prepared topo projects with QGIS Server and rGIS at Norgeskart zoom levels.

python3 tools/topo/compare.py [--set projects|projects-nolabels] [--zooms 9-15] [--only lag10]

For each project, one GetMap of the project's top group (as MapProxy requests it) per zoom
level of the utm33n grid (21664 m/px at z0), centred on the prepared area, in EPSG:25833.
Writes <out>/report-<set>/report.md (worst first), report.json and the images.
"""
import argparse
import io
import json
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

import numpy as np
from PIL import Image

HERE = Path(__file__).resolve().parent
ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("--out", type=Path, default=HERE.parent.parent / "out" / "topo")
ap.add_argument("--set", default="projects", choices=["projects", "projects-nolabels"])
ap.add_argument("--zooms", default="9-15")
ap.add_argument("--size", type=int, default=512)
ap.add_argument("--only", default="")
ap.add_argument("--leaves", action="store_true", help="render each leaf layer on its own")
ap.add_argument("--qgis", default="http://127.0.0.1:8099/")
ap.add_argument("--rgis", default="http://127.0.0.1:8098/")
args = ap.parse_args()

out = args.out.resolve()
manifest = json.loads((out / "manifest.json").read_text())
cx, cy = manifest["center"]
lo, _, hi = args.zooms.partition("-")
zooms = range(int(lo), int(hi or lo) + 1)
rep = out / f"report-{args.set}"
rep.mkdir(exist_ok=True)
strip = lambda t: t.split("}")[-1]


def get(base, q):
    t = time.time()
    try:
        with urllib.request.urlopen(base + "?" + q, timeout=300) as r:
            body, status, ctype = r.read(), r.status, r.headers.get("Content-Type") or ""
    except urllib.error.HTTPError as e:
        body, status, ctype = e.read(), e.code, e.headers.get("Content-Type") or ""
    except Exception as e:  # timeouts, resets
        body, status, ctype = str(e).encode(), 0, ""
    return status, ctype, body, time.time() - t


def image(status, ctype, body):
    if status == 200 and ctype.startswith("image/"):
        return np.asarray(Image.open(io.BytesIO(body)).convert("RGBA")).astype(np.int16)
    return None


rows = []
projects = sorted((out / args.set).glob("*.qgs"))
if args.only:
    projects = [p for p in projects if p.stem.startswith(tuple(args.only.split(",")))]
for path in projects:
    m = "MAP=" + urllib.parse.quote(str(path))
    _, _, caps, _ = get(args.qgis, f"{m}&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities")
    try:
        root = ET.fromstring(caps)
        top = next(e for e in root.iter() if strip(e.tag) == "Layer")
        layer = next(c.text for c in next(c for c in top if strip(c.tag) == "Layer") if strip(c.tag) == "Name")
    except Exception:
        rows.append({"project": path.stem, "z": "-", "note": "QGIS: no capabilities"})
        continue
    names = [layer]
    if args.leaves:
        names = [n.text for l in root.iter() if strip(l.tag) == "Layer" and not any(strip(c.tag) == "Layer" for c in l)
                 for n in l if strip(n.tag) == "Name"]
    for layer, z in ((n, z) for n in names for z in zooms):
        h = 21664 / 2**z * args.size / 2
        q = (f"{m}&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={urllib.parse.quote(layer)}"
             f"&STYLES=&CRS=EPSG:25833&BBOX={cx - h},{cy - h},{cx + h},{cy + h}"
             f"&WIDTH={args.size}&HEIGHT={args.size}&FORMAT=image/png&TRANSPARENT=TRUE")
        rq, rr = get(args.qgis, q), get(args.rgis, q)
        a, b = image(*rq[:3]), image(*rr[:3])
        name = path.stem if layer == names[0] and not args.leaves else f"{path.stem}/{layer}"
        row = {"project": name, "z": z, "qgis_s": round(rq[3], 3), "rgis_s": round(rr[3], 3)}
        stem = rep / f"{name.replace('/', '__')}_z{z}"
        if a is None or b is None:
            row["note"] = "; ".join(f"{n}: HTTP {r[0]} {r[2][:150].decode(errors='replace')}"
                                    for n, r, i in (("QGIS", rq, a), ("rGIS", rr, b)) if i is None)
        else:
            Image.fromarray(a.astype(np.uint8)).save(f"{stem}_qgis.png")
            Image.fromarray(b.astype(np.uint8)).save(f"{stem}_rgis.png")
            # Same metric as tools/parity.py: any channel off by more than 64/255.
            bad = (np.abs(a - b) > 64).any(axis=2)
            row["diff_pct"] = round(100 * bad.mean(), 2)
            row["qgis_empty"] = bool((a[..., 3] == 0).all())
            d = np.zeros_like(a, dtype=np.uint8)
            d[..., 3] = 255
            d[bad] = (255, 0, 0, 255)
            Image.fromarray(d).save(f"{stem}_diff.png")
        rows.append(row)
        print(row, flush=True)

(rep / "report.json").write_text(json.dumps(rows, indent=1))
rank = lambda r: (-(r.get("diff_pct") if "diff_pct" in r else 101), r["project"], str(r["z"]))
with open(rep / "report.md", "w") as f:
    f.write(f"# {args.set}: QGIS Server vs rGIS, zooms {args.zooms}, {args.size} px, "
            f"centre {cx:.0f},{cy:.0f} EPSG:25833\n\n")
    f.write("| project | z | diff % | QGIS s | rGIS s | note |\n|---|---|---|---|---|---|\n")
    for r in sorted(rows, key=rank):
        note = r.get("note", "") or ("QGIS image empty" if r.get("qgis_empty") else "")
        f.write(f"| {r['project']} | {r['z']} | {r.get('diff_pct', '-')} | {r.get('qgis_s', '-')} "
                f"| {r.get('rgis_s', '-')} | {note} |\n")
    for proj, info in manifest["projects"].items():
        if info["skipped_postgres"] or info["external"]:
            f.write(f"\n{proj}: dropped PostGIS {info['skipped_postgres']}; external {info['external']}")
print(f"\n{rep / 'report.md'}")
