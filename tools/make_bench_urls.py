"""Write benchmark query files (one query string per line) into the given directory."""
import math
import os
import random
import sys

out, wfs_map = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
R = 6378137.0
ORIGIN = math.pi * R


def tile_bbox(x, y, z):
    size = 2 * ORIGIN / 2**z
    return (-ORIGIN + x * size, ORIGIN - (y + 1) * size, -ORIGIN + (x + 1) * size, ORIGIN - y * size)


def lonlat_to_tile(lon, lat, z):
    n = 2**z
    x = int((lon + 180) / 360 * n)
    y = int((1 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2 * n)
    return x, y


tiles = []
for z in range(3, 9):
    x0, y0 = lonlat_to_tile(-25, 72, z)
    x1, y1 = lonlat_to_tile(45, 34, z)
    tiles += [(x, y, z) for x in range(x0, x1 + 1) for y in range(y0, y1 + 1)]
random.Random(42).shuffle(tiles)

files = {
    "caps": ["SERVICE=WMS&VERSION=1.3.0&REQUEST=GetCapabilities"],
    "world": ["SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:4326&BBOX=-90,-180,90,180&WIDTH=800&HEIGHT=400&FORMAT=image/png"],
    "europe": ["SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:3857&BBOX=-1500000,4000000,4500000,11000000&WIDTH=600&HEIGHT=700&FORMAT=image/png"],
    "tiles": [
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&STYLES=&CRS=EPSG:3857&BBOX=%.3f,%.3f,%.3f,%.3f&WIDTH=256&HEIGHT=256&FORMAT=image/png&TRANSPARENT=true" % tile_bbox(*t)
        for t in tiles
    ],
    "gfi": [
        f"SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=Earth&QUERY_LAYERS=ne_10m_admin_0_countries,ne_10m_lakes&CRS=EPSG:4326&BBOX=35,-10,70,40&WIDTH=500&HEIGHT=350&I={i}&J={j}&INFO_FORMAT=application/json&FEATURE_COUNT=5"
        for i, j in [(random.Random(s).randrange(500), random.Random(s + 1000).randrange(350)) for s in range(200)]
    ],
    "wfs_lakes_all": [f"MAP={wfs_map}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME=ne_10m_lakes&OUTPUTFORMAT=application/json"],
    "wfs_bbox": [f"MAP={wfs_map}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME=ne_10m_admin_0_countries&BBOX=5,58,10,62&OUTPUTFORMAT=application/json"],
}
for name, lines in files.items():
    with open(os.path.join(out, name + ".txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"{name}: {len(lines)} distinct requests")
