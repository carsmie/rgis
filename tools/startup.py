"""Launch a server (detached), time until it answers, then time a cold GetMap.

python3 tools/startup.py <port> <logfile> <cwd> <command> [args…]
"""
import subprocess
import sys
import time
import urllib.request

port, log, cwd, cmd = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:]
base = f"http://127.0.0.1:{port}/?"
t0 = time.time()
proc = subprocess.Popen(cmd, cwd=cwd, stdout=open(log, "w"), stderr=subprocess.STDOUT, start_new_session=True)
while True:
    try:
        urllib.request.urlopen(base + "SERVICE=WMS&REQUEST=GetCapabilities", timeout=5).read()
        break
    except Exception:
        if proc.poll() is not None:
            sys.exit(f"server exited with {proc.returncode}")
        time.sleep(0.01)
print(f"pid {proc.pid}: ready (incl. project load + first GetCapabilities) in {time.time() - t0:.2f} s")
t = time.time()
urllib.request.urlopen(base + "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:3857"
                       "&BBOX=-1500000,4000000,4500000,11000000&WIDTH=600&HEIGHT=700&FORMAT=image/png").read()
print(f"first (cold) Europe GetMap: {(time.time() - t) * 1000:.0f} ms")
