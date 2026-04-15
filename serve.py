"""HTTP server for calcite web UI with no-cache headers.

Replaces `python -m http.server` — same thing but sends Cache-Control: no-store
on every response so reloads always fetch fresh wasm, CSS, and JS. Avoids the
"am I looking at old code?" trap.
"""
import sys
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class NoCacheHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Cache-Control', 'no-store, no-cache, must-revalidate, max-age=0')
        self.send_header('Pragma', 'no-cache')
        self.send_header('Expires', '0')
        super().end_headers()


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    with ThreadingHTTPServer(('0.0.0.0', port), NoCacheHandler) as httpd:
        print(f'Serving on http://localhost:{port}/ (no-cache)')
        httpd.serve_forever()


if __name__ == '__main__':
    main()
