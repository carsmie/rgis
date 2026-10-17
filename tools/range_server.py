"""Minimal static file server with HTTP Range support (like S3), logging each range."""
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT, PORT = sys.argv[1], int(sys.argv[2])


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        path = os.path.join(ROOT, os.path.basename(self.path.split("?")[0]))
        if not os.path.isfile(path):
            self.send_error(404)
            return
        size = os.path.getsize(path)
        m = re.match(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
        start, end = (int(m.group(1)), int(m.group(2) or size - 1)) if m else (0, size - 1)
        end = min(end, size - 1)
        with open(path, "rb") as f:
            f.seek(start)
            body = f.read(end - start + 1)
        self.send_response(206 if m else 200)
        if m:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        sys.stderr.write(f"{self.path} Range={self.headers.get('Range')}\n")


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
