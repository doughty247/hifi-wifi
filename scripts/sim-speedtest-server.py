#!/usr/bin/env python3
"""Minimal speed-test server for scripts/sim-bufferbloat.sh (MODE=bench).

GET streams zeros until the client disconnects; POST reads and discards the upload.
Usage: sim-speedtest-server.py ADDRESS PORT
"""
import http.server
import socketserver
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        chunk = b"\0" * 65536
        try:
            while True:
                self.wfile.write(chunk)
        except OSError:
            pass

    def do_POST(self):
        try:
            while self.rfile.read(65536):
                pass
        except OSError:
            pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


Server((sys.argv[1], int(sys.argv[2])), Handler).serve_forever()
