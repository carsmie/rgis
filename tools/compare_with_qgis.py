"""Send identical requests to QGIS Server (:8099) and rGIS (:8098) and compare."""
import json
import sys
import urllib.request
import xml.etree.ElementTree as ET

QGIS, RUST = "http://127.0.0.1:8099/", "http://127.0.0.1:8098/"
WFS_MAP = sys.argv[1]  # absolute path of earth_local_wfs.qgs


def get(base, q):
    try:
        with urllib.request.urlopen(base + "?" + q, timeout=60) as r:
            return r.status, r.headers.get("Content-Type"), r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Content-Type"), e.read()


def strip(tag):
    return tag.split("}")[-1]


def wms_layers(xml):
    root = ET.fromstring(xml)
    out = []
    for lyr in root.iter():
        if strip(lyr.tag) != "Layer":
            continue
        name = next((c.text for c in lyr if strip(c.tag) == "Name"), None)
        crs = [c.text for c in lyr if strip(c.tag) in ("CRS", "SRS")]
        bbs = {c.get("CRS") or c.get("SRS"): tuple(round(float(c.get(k)), 2) for k in ("minx", "miny", "maxx", "maxy")) for c in lyr if strip(c.tag) == "BoundingBox"}
        out.append((name, lyr.get("queryable"), crs, bbs))
    return out


results = []


def check(label, ok, detail=""):
    results.append(ok)
    print(("PASS " if ok else "FAIL ") + label + (f"  -- {detail}" if detail and not ok else ""))


# --- WMS GetCapabilities 1.3.0 and 1.1.1
for v in ("1.3.0", "1.1.1"):
    q = f"SERVICE=WMS&VERSION={v}&REQUEST=GetCapabilities"
    (_, _, a), (_, _, b) = get(QGIS, q), get(RUST, q)
    la, lb = wms_layers(a), wms_layers(b)
    check(f"WMS {v} capabilities: same layer names/order", [x[0] for x in la] == [x[0] for x in lb], f"{[x[0] for x in la]} vs {[x[0] for x in lb]}")
    check(f"WMS {v} capabilities: same queryable flags", [x[1] for x in la] == [x[1] for x in lb])
    check(f"WMS {v} capabilities: same CRS lists", [x[2] for x in la] == [x[2] for x in lb], f"{la[0][2]} vs {lb[0][2]}")
    check(f"WMS {v} capabilities: same BoundingBox CRSs per layer ({sum(len(x[3]) for x in la)} boxes)", [sorted(x[3]) for x in la] == [sorted(x[3]) for x in lb],
          f"{[sorted(x[3]) for x in la]} vs {[sorted(x[3]) for x in lb]}")
    worst, merc = 0.0, []
    for (na, _, _, ba), (nb, _, _, bb) in zip(la, lb):
        for crs, box in ba.items():
            if crs in bb:
                d = max(abs(p - q) for p, q in zip(box, bb[crs]))
                if crs == "EPSG:3857" and min(box[1], box[0]) < -2.0038e7:
                    merc.append((na, box[1], bb[crs][1]))  # polar latitude: QGIS does not clamp
                else:
                    worst = max(worst, d)
    check(f"WMS {v} capabilities: BoundingBoxes agree (max diff {worst:g})", worst < 0.01)
    for name, qv, rv in merc:
        print(f"NOTE {name}: EPSG:3857 miny QGIS {qv:.0f} (unclamped, lat -89.9) vs rust {rv:.0f} (clamped at Mercator limit)")

# --- GetMap axis order: 1.1.1 lon/lat must equal 1.3.0 lat/lon
q11 = "SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS=Earth&SRS=EPSG:4326&BBOX=-180,-90,180,90&WIDTH=200&HEIGHT=100&FORMAT=image/png"
q13 = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:4326&BBOX=-90,-180,90,180&WIDTH=200&HEIGHT=100&FORMAT=image/png"
check("GetMap 1.1.1 (lon/lat) == 1.3.0 (lat/lon) image", get(RUST, q11)[2] == get(RUST, q13)[2])

# --- GetFeatureInfo in several formats and positions
for i, j, bbox in [(100, 100, "50,0,70,20"), (50, 150, "40,-10,60,30"), (10, 10, "-60,-80,-20,-30")]:
    q = f"SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=Earth&QUERY_LAYERS=ne_10m_admin_0_countries,ne_10m_lakes&CRS=EPSG:4326&BBOX={bbox}&WIDTH=200&HEIGHT=200&I={i}&J={j}&INFO_FORMAT=application/json&FEATURE_COUNT=5"
    a, b = json.loads(get(QGIS, q)[2]), json.loads(get(RUST, q)[2])
    ids_a, ids_b = [f["id"] for f in a["features"]], [f["id"] for f in b["features"]]
    check(f"GetFeatureInfo JSON at I={i},J={j} bbox={bbox}: same features {ids_a}", ids_a == ids_b and a == b, f"{ids_a} vs {ids_b}")

q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=ne_10m_admin_0_countries&QUERY_LAYERS=ne_10m_admin_0_countries&CRS=EPSG:3857&BBOX=0,7000000,2000000,10000000&WIDTH=256&HEIGHT=256&I=128&J=128&INFO_FORMAT=application/json"
a, b = json.loads(get(QGIS, q)[2]), json.loads(get(RUST, q)[2])
check(f"GetFeatureInfo in EPSG:3857 identical ({[f['properties'].get('NAME') for f in a['features']]})", a == b, f"{a} vs {b}")

# --- WFS (project with WFSLayers)
M = "MAP=" + WFS_MAP
q = f"{M}&SERVICE=WFS&REQUEST=GetCapabilities"
names = lambda x: [c.text for c in ET.fromstring(x).iter() if strip(c.tag) == "Name" and c.text and not c.text.startswith(("Get", "Describe"))]
check("WFS capabilities: same feature types", names(get(QGIS, q)[2]) == names(get(RUST, q)[2]))
for tn, extra in [("ne_10m_lakes", "MAXFEATURES=3"), ("ne_10m_admin_0_countries", "BBOX=5,58,10,62&PROPERTYNAME=NAME,ISO_A3"),
                  ("ne_10m_admin_0_countries", "FEATUREID=ne_10m_admin_0_countries.137"), ("ne_10m_admin_0_boundary_lines_land", "MAXFEATURES=2&STARTINDEX=5"),
                  ("ne_10m_admin_0_countries", "EXP_FILTER=%22ISO_A3%22%20IN%20('NOR','SWE')&PROPERTYNAME=NAME")]:
    q = f"{M}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME={tn}&{extra}&OUTPUTFORMAT=application/json"
    a, b = json.loads(get(QGIS, q)[2]), json.loads(get(RUST, q)[2])
    same_ids = [f["id"] for f in a["features"]] == [f["id"] for f in b["features"]]
    same_props = [f["properties"] for f in a["features"]] == [f["properties"] for f in b["features"]]
    same_geom = [f["geometry"] for f in a["features"]] == [f["geometry"] for f in b["features"]]
    check(f"WFS GetFeature {tn} {extra}: ids/properties/geometry identical", same_ids and same_props and same_geom,
          f"ids {same_ids} props {same_props} geom {same_geom}: {[f['id'] for f in a['features']]} vs {[f['id'] for f in b['features']]}")

pos = lambda x: [c.text for c in ET.fromstring(x).iter() if strip(c.tag) in ("lowerCorner", "upperCorner", "posList")]
q = f"{M}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME=ne_10m_lakes&MAXFEATURES=2"
a, b = get(QGIS, q)[2], get(RUST, q)[2]
try:
    ET.fromstring(a)
    check("QGIS GML for ne_10m_lakes is well-formed", True)
except ET.ParseError as e:
    print(f"NOTE QGIS's own GML for ne_10m_lakes is NOT well-formed ({e}); field 'feature::id' becomes <qgs:feature::id>")
check("rGIS GML for ne_10m_lakes is well-formed", bool(pos(b)))
q = f"{M}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME=ne_10m_admin_0_countries&FEATUREID=ne_10m_admin_0_countries.137"
a, b = get(QGIS, q)[2], get(RUST, q)[2]
pa, pb = pos(a), pos(b)
same = len(pa) == len(pb) and all(
    all(abs(float(x) - float(y)) < 1e-6 for x, y in zip(sa.split(), sb.split())) for sa, sb in zip(pa, pb))
check(f"WFS GML3 countries.137: same envelopes and posLists, lat/lon order ({len(pa)} coordinate lists)", same, f"{pa[:2]} vs {pb[:2]}")

q = f"{M}&SERVICE=WFS&VERSION=1.1.0&REQUEST=DescribeFeatureType&TYPENAME=ne_10m_admin_0_countries"
els = lambda x: [(e.get("name")) for e in ET.fromstring(x).iter() if strip(e.tag) == "element" and e.get("name")]
check("WFS DescribeFeatureType: same element names", els(get(QGIS, q)[2]) == els(get(RUST, q)[2]), f"{els(get(QGIS, q)[2])} vs {els(get(RUST, q)[2])}")

# --- Errors
for q, label in [("SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=nope&CRS=EPSG:4326&BBOX=0,0,1,1&WIDTH=10&HEIGHT=10", "unknown layer"),
                 ("SERVICE=WMS&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:4326&BBOX=0,0,1,1&WIDTH=10&HEIGHT=10", "missing VERSION"),
                 ("SERVICE=WMS&VERSION=1.3.0&REQUEST=Foo", "unknown request"),
                 ("SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:9999999&BBOX=0,0,1,1&WIDTH=10&HEIGHT=10", "invalid CRS")]:
    (sa, _, a), (sb, _, b) = get(QGIS, q), get(RUST, q)
    code = lambda x: next((e.get("code") for e in ET.fromstring(x).iter() if strip(e.tag) == "ServiceException"), None)
    check(f"error '{label}': HTTP {sa}/{sb}, code {code(a)}/{code(b)}", sa == sb and code(a) == code(b))

print(f"\n{sum(results)}/{len(results)} checks passed")
