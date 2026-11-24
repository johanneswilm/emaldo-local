#!/usr/bin/env python3
"""Captive-portal / connectivity probe responder for the firmware VM.

The firmware's netmonitor judges link state by probing a well-known 204
endpoint (measured: it queries cp.cloudflare.com every ~10 s). Inside the jail
those probes must be answered locally, or the app declares the link down and
never starts its cloud stage. Answer 204 for everything, log the path and the
client address so a probe line can be attributed.

    usage: portal204.py [port]      (default 80; PORTAL_BIND, default 0.0.0.0)

Bind: the firmware's HTTP client sets SO_BINDTODEVICE eth0. A socket with that
option reaches a 0.0.0.0 listener on port 80 at a loopback destination (the
route to a local address resolves to "local delivery via eth0" - measured), but
it can no longer reach one bound to 127.0.0.1 once the 1.1.1.1/8.8.8.8 aliases
are removed from lo, so the default bind here is the wildcard address.
"""

import http.server
import os
import sys
import time

LOG = os.environ.get("PORTAL_LOG", "/var/log/portal204.log")
BIND = os.environ.get("PORTAL_BIND", "0.0.0.0")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _answer(self):
        try:
            with open(LOG, "a") as fh:
                fh.write("%s %s %s from %s\n" % (time.strftime("%H:%M:%S"),
                                                 self.command, self.path,
                                                 self.client_address[0]))
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
    try:
        with open(LOG, "a") as fh:
            fh.write("%s serving on %s:%d\n" % (time.strftime("%H:%M:%S"), BIND, port))
    except OSError:
        pass
    srv = http.server.ThreadingHTTPServer((BIND, port), Handler)
    srv.serve_forever()


if __name__ == "__main__":
    main()
