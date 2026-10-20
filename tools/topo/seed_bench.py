"""Seed tnt-mapproxy's topo_cache from a local QGIS Server or rGIS and time it.

python3 tools/topo/seed_bench.py --server http://127.0.0.1:8097/ --label rgis [--levels 0-7]

Runs the real tnt-mapproxy image (docker build -t tnt-mapproxy:seedtest <tnt-mapproxy>) with
its own mapproxy.yaml/caches.yaml, but sources.yaml rendered so every source asks the one
server under test (`?MAP=<projects>/<lag>.qgs`, which both servers understand). Each run seeds
into an empty cache, so every meta-tile hits the server. Reports wall time, meta-tile requests
and, with --cpu-pid / --cpu-container, the server's CPU seconds.
"""
import argparse
import os
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
ap.add_argument("--server", required=True, help="base URL of the WMS under test")
ap.add_argument("--label", required=True)
ap.add_argument("--projects", type=Path, default=HERE.parent.parent / "out" / "topo" / "projects")
ap.add_argument("--mapproxy-repo", type=Path, default=Path.home() / "repos" / "tnt-mapproxy")
ap.add_argument("--image", default="tnt-mapproxy:seedtest")
ap.add_argument("--levels", default="0-7")
ap.add_argument("--concurrency", type=int, default=4)  # the scheduled seed jobs' -c
ap.add_argument("--cpu-pid", type=int, help="server process to measure CPU of (rGIS)")
ap.add_argument("--cpu-container", help="server container to measure CPU of (QGIS)")
args = ap.parse_args()
lo, _, hi = args.levels.partition("-")


def cpu_seconds():
    if args.cpu_pid:
        f = Path(f"/proc/{args.cpu_pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / 100  # utime + stime, clock ticks
    if args.cpu_container:
        cid = subprocess.check_output(["docker", "inspect", "-f", "{{.Id}}", args.cpu_container], text=True).strip()
        for p in Path("/sys/fs/cgroup").glob(f"**/*{cid}*/cpu.stat"):
            return int(p.read_text().split()[1]) / 1e6  # usage_usec
    return None


work = Path(tempfile.mkdtemp(prefix=f"seedbench-{args.label}-", dir=os.environ.get("SEEDBENCH_TMP")))
rendered, cache = work / "rendered", work / "cache"
rendered.mkdir()
cache.mkdir()
for d in (rendered, cache):
    d.chmod(0o777)  # the image runs as uid 150

# Every QGIS source → the server under test; keep min_res, coverages, on_error as deployed.
src = (args.mapproxy_repo / "sources.yaml.template").read_text()
base = args.server if args.server.endswith("/") else args.server + "/"
# Docker Desktop: the host's loopback is host.docker.internal inside containers.
base = re.sub(r"//(127\.0\.0\.1|localhost)([:/])", r"//host.docker.internal\2", base)
src = re.sub(r"url: http://[^\s/]+/qgis/QLR/(\w+)",
             lambda m: f"url: '{base}?MAP={args.projects}/{m.group(1)}.qgs'", src)
(rendered / "sources.yaml").write_text(src)
# Sources whose project isn't in the local extract fail every request, and with on_error
# `cache: false` MapProxy then caches no meta-tile they touch: leave them out of topo_cache.
caches = (args.mapproxy_repo / "caches.yaml").read_text()
missing = sorted({m.group(1) for m in re.finditer(r"^  (qgis_\w+):\n(?:    .*\n)*?      url: '[^']*MAP=[^']*/(\w+)\.qgs'", src, re.M)
                  if not (args.projects / f"{m.group(2)}.qgs").exists()})
head, sep, rest = caches.partition("  topo_qgis_cache:")
for name in missing:
    head = re.sub(rf"^      - {name}\n", "", head, flags=re.M)
(work / "caches.yaml").write_text(head + sep + rest)
if missing:
    print(f"left out of topo_cache (no local project): {', '.join(missing)}")
(work / "seed.yaml").write_text(f"""seeds:
  bench:
    caches: [topo_cache]
    grids: [norway_25833]
    coverages: [norway]
    levels: {{from: {lo}, to: {hi or lo}}}
coverages:
  norway:
    datasource: /mapproxy/coverage_norway.json
    srs: 'EPSG:4326'
""")
script = (
    "set -e; cd /mapproxy; cp layers.yaml rendered/layers.yaml; cp services.yaml rendered/services.yaml; "
    "PYTHONPATH=/mapproxy python -m app.hidpi_config caches.yaml rendered/sources.yaml rendered/hidpi.yaml; "
    f"mapproxy-seed -f mapproxy.yaml -s /bench/seed.yaml -c {args.concurrency} --seed bench"
)
cmd = ["docker", "run", "--rm", "--add-host", "host.docker.internal:host-gateway",
       "-v", f"{rendered}:/mapproxy/rendered", "-v", f"{cache}:/tmp/nfs/tntcache",
       "-v", f"{work / 'seed.yaml'}:/bench/seed.yaml:ro", "-v", f"{work / 'caches.yaml'}:/mapproxy/caches.yaml:ro", "--entrypoint", "sh", args.image, "-c", script]
c0, t0 = cpu_seconds(), time.perf_counter()
run = subprocess.run(cmd, capture_output=True, text=True)
wall = time.perf_counter() - t0
c1 = cpu_seconds()
log = run.stdout + run.stderr
(work / "seed.log").write_text(log)
tiles = sum(1 for p in cache.rglob("*.png") if p.is_file())
errors = len(re.findall(r"(?i)error|failed", log))
cpu = f"  server CPU {c1 - c0:.1f}s" if c0 is not None and c1 is not None else ""
print(f"{args.label}: levels {args.levels}  wall {wall:.1f}s  tiles {tiles}  error lines {errors}{cpu}"
      f"  exit {run.returncode}  log {work / 'seed.log'}")
shutil.rmtree(cache, ignore_errors=True)
