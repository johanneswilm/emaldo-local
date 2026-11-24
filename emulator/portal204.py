#!/usr/bin/env python3
"""Captive-portal / connectivity probe responder for the firmware VM.

The firmware's netmonitor judges link state by probing a well-known 204
endpoint (measured: it queries cp.cloudflare.com every ~10 s). Inside the jail
those probes must be answered locally, or the app declares the link down and
never starts its cloud stage. Answer 204 for everything, log the path.

    usage: portal204.py [port]      (default 80; listens on 127.0.0.1)
"""

import http.server
import sys
import time

LOG = "/var/log/portal204.log"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _answer(self):
        try:
            with open(LOG, "a") as fh:
                fh.write("%s %s %s\n" % (time.strftime("%H:%M:%S"), self.command, self.path))
        except OSError:
            pass
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = _answer
    do_HEAD = _answer
    do_POST = _answer

    def log_message(self, fmt, *args):
        pass


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 80
    # all addresses: the app's sockets are bound to eth0, loopback is not enough
    srv = http.server.ThreadingHTTPServer(("0.0.0.0", port), Handler)
    srv.serve_forever()


if __name__ == "__main__":
    main()
