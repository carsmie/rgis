"""Throughput matrix: tile workload at several concurrency levels against several setups.

python3 tools/run_bench.py <bench-dir> <workload> <concurrency,...> <label>=<base[,base…]> …
e.g. QGIS-x1=http://127.0.0.1:8099/ Rust-x1=http://127.0.0.1:8098/
"""
import subprocess
import sys

bench_dir, workload, levels = sys.argv[1], sys.argv[2], [int(c) for c in sys.argv[3].split(",")]
setups = [a.split("=", 1) for a in sys.argv[4:]]
exe = "./target/release/examples/bench"
urls = f"{bench_dir}/{workload}.txt"

for label, bases in setups:  # warm every instance
    subprocess.run([exe, "-c", "10", "-n", "1000", "-f", urls, *bases.split(",")], capture_output=True)
for c in levels:
    for label, bases in setups:
        n = str(max(1000, 150 * c))
        out = subprocess.run([exe, "-c", str(c), "-n", n, "-f", urls, *bases.split(",")], capture_output=True, text=True).stdout.strip()
        print(f"c={c:<3} {label:9} {out}")
