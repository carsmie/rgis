"""Render every published layer of every project with QGIS Server and rGIS and diff them.

python3 tools/parity.py <outdir> <project.qgs|.qgz> …   (QGIS on :8099, rGIS on :8098)

For each leaf layer of each project's WMS capabilities, a 384×384 GetMap of the layer's
own extent is requested from both servers and compared with examples/compare_png.
Writes <outdir>/parity.md (sorted worst first) and the images.
"""
import os
import re
import subprocess
import sys
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

QGIS = os.environ.get("QGIS_URL", "http://127.0.0.1:8099/")
RGIS = os.environ.get("RGIS_URL", "http://127.0.0.1:8098/")
outdir, projects = sys.argv[1], sys.argv[2:]
os.makedirs(outdir, exist_ok=True)
strip = lambda t: t.split("}")[-1]
SIZE = 384


def get(base, q):
    try:
        with urllib.request.urlopen(base + "?" + q, timeout=120) as r:
            return r.status, r.headers.get("Content-Type") or "", r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type") or "", e.read()
    except Exception as e:  # timeouts, resets
        return 0, "", str(e).encode()


def error_text(body):
    m = re.search(rb"<ServiceException[^>]*>(.*?)</ServiceException>", body, re.S)
    return (m.group(1) if m else body[:200]).decode(errors="replace").strip()


rows = []
for path in projects:
    pname = os.path.splitext(os.path.basename(path))[0]
    m = "MAP=" + urllib.parse.quote(path)
    status, _, caps = get(QGIS, f"{m}&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities")
    try:
        root = ET.fromstring(caps)
    except ET.ParseError:
        rows.append((pname, "-", None, "QGIS: no capabilities", ""))
        continue
    leaves = [l for l in root.iter() if strip(l.tag) == "Layer" and not any(strip(c.tag) == "Layer" for c in l)]
    for leaf in leaves:
        name = next((c.text for c in leaf if strip(c.tag) == "Name"), None)
        boxes = {b.get("CRS"): b for b in leaf if strip(b.tag) == "BoundingBox"}
        if not name or not boxes:
            continue
        crs = "EPSG:3857" if "EPSG:3857" in boxes else next(c for c in boxes if c != "CRS:84")
        b = boxes[crs]
        x0, y0, x1, y1 = (float(b.get(k)) for k in ("minx", "miny", "maxx", "maxy"))
        if x1 <= x0 or y1 <= y0:  # point layers with one feature: pad
            x0, y0, x1, y1 = x0 - 50, y0 - 50, x1 + 50, y1 + 50
        px, py = (x1 - x0) * 0.05, (y1 - y0) * 0.05
        x0, y0, x1, y1 = x0 - px, y0 - py, x1 + px, y1 + py
        # Like real clients, ask for an image with the BBOX's aspect ratio (no stretching).
        # WMS 1.3.0 EPSG:4326 boxes are lat/lon, so the height axis is the first one there.
        geo = crs == "EPSG:4326"
        span_w, span_h = ((y1 - y0), (x1 - x0)) if geo else ((x1 - x0), (y1 - y0))
        height = max(32, min(4 * SIZE, round(SIZE * span_h / span_w)))
        # Make the box match the integer image size exactly.
        mid = (x0 + x1) / 2 if geo else (y0 + y1) / 2
        half = span_w * height / SIZE / 2
        if geo:
            x0, x1 = mid - half, mid + half
        else:
            y0, y1 = mid - half, mid + half
        q = (f"{m}&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={urllib.parse.quote(name)}&STYLES="
             f"&CRS={crs}&BBOX={x0},{y0},{x1},{y1}&WIDTH={SIZE}&HEIGHT={height}&FORMAT=image/png")
        safe = re.sub(r"[^A-Za-z0-9_.-]", "_", f"{pname}__{name}")
        files = {}
        notes = []
        for tag, base in (("qgis", QGIS), ("rgis", RGIS)):
            st, ct, body = get(base, q)
            if ct.startswith("image/png"):
                files[tag] = os.path.join(outdir, f"{safe}.{tag}.png")
                open(files[tag], "wb").write(body)
            else:
                notes.append(f"{tag}: HTTP {st} {error_text(body)}")
        if len(files) < 2:
            rows.append((pname, name, None, "; ".join(notes), q))
            continue
        out = subprocess.run(["./target/release/examples/compare_png", files["qgis"], files["rgis"],
                              os.path.join(outdir, f"{safe}.diff.png")], capture_output=True, text=True).stdout
        pct = float(re.search(r"\(([\d.]+)%\)", out).group(1))
        rows.append((pname, name, pct, "", q))
        print(f"{pct:6.2f}%  {pname} / {name}", flush=True)

rows.sort(key=lambda r: (-1 if r[2] is None else -r[2]))
with open(os.path.join(outdir, "parity.md"), "w") as f:
    f.write("| differing pixels | project | layer | note |\n|---|---|---|---|\n")
    for p, l, pct, note, _ in rows:
        f.write(f"| {'error' if pct is None else f'{pct:.2f} %'} | {p} | {l} | {note} |\n")
ok = [r for r in rows if r[2] is not None]
print(f"\n{len(ok)} layers compared, {len(rows) - len(ok)} not comparable; "
      f"<0.5%: {sum(r[2] < 0.5 for r in ok)}, <2%: {sum(r[2] < 2 for r in ok)}, >=2%: {sum(r[2] >= 2 for r in ok)}")
