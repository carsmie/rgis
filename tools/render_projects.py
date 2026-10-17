"""For each QGIS project: GetMap of the root layer from QGIS Server and rGIS."""
import os
import sys
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

QGIS, RUST = "http://127.0.0.1:8099/", "http://127.0.0.1:8098/"
outdir, projects = sys.argv[1], sys.argv[2:]
os.makedirs(outdir, exist_ok=True)


def get(base, q):
    try:
        with urllib.request.urlopen(base + "?" + q, timeout=120) as r:
            return r.status, r.headers.get("Content-Type"), r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type"), e.read()


strip = lambda t: t.split("}")[-1]
for path in projects:
    name = os.path.splitext(os.path.basename(path))[0]
    m = "MAP=" + urllib.parse.quote(path)
    _, _, caps = get(QGIS, f"{m}&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities")
    root = next(l for l in ET.fromstring(caps).iter() if strip(l.tag) == "Layer")
    lname = next((c.text for c in root if strip(c.tag) == "Name"), None)
    if lname is None:  # unnamed root: request the leaf layers, bottom first
        leaves = [l for l in root.iter() if strip(l.tag) == "Layer" and not any(strip(c.tag) == "Layer" for c in l)]
        lname = ",".join(reversed([next(c.text for c in l if strip(c.tag) == "Name") for l in leaves]))
    # Union of all layers' boxes in the first advertised projected/geographic CRS.
    boxes = [c for c in root.iter() if strip(c.tag) == "BoundingBox" and c.get("CRS") != "CRS:84"]
    crs = boxes[0].get("CRS")
    vals = [[float(b.get(k)) for k in ("minx", "miny", "maxx", "maxy")] for b in boxes if b.get("CRS") == crs]
    x0, y0 = min(v[0] for v in vals), min(v[1] for v in vals)
    x1, y1 = max(v[2] for v in vals), max(v[3] for v in vals)
    pad_x, pad_y = (x1 - x0) * 0.05, (y1 - y0) * 0.05
    bbox = f"{x0 - pad_x},{y0 - pad_y},{x1 + pad_x},{y1 + pad_y}"
    q = f"{m}&SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={urllib.parse.quote(lname)}&STYLES=&CRS={crs}&BBOX={bbox}&WIDTH=512&HEIGHT=512&FORMAT=image/png"
    for tag, base in (("qgis", QGIS), ("rust", RUST)):
        status, ctype, body = get(base, q)
        fn = os.path.join(outdir, f"{name}.{tag}.png")
        if ctype and ctype.startswith("image/png"):
            open(fn, "wb").write(body)
        else:
            print(f"{name} [{tag}] HTTP {status} {ctype}: {body[:300]!r}")
    print(f"{name}: LAYERS={lname} CRS={crs} BBOX={bbox}")
