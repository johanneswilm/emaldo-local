#!/usr/bin/env python3
"""Publish one message through the sandbox MQTT broker and print what comes back.

  mqtt_pub.py '{"cmd":"set_fcr_schedule"}'            to iot/<device id>/down/cmd
  mqtt_pub.py some/topic 'payload' [seconds to listen]

The device id is the client id of the app's last CONNECT in the broker log.
Run inside the guest (broker on 127.0.0.1:8883, TLS, certificate not checked).
"""
import json, os, socket, ssl, sys, time

LOG = os.environ.get("MQTT_LOG", "/sandbox/mqtt.log")


def device_id():
    last = None
    for line in open(LOG):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        if r.get("ev") == "connect" and r.get("client") != "sandbox-pub":
            last = r["client"]
    return last


def pkt(first, body):
    n, out = len(body), b""
    while True:
        b, n = n & 0x7F, n >> 7
        out += bytes([b | (0x80 if n else 0)])
        if not n:
            return bytes([first]) + out + body


def s(x):
    x = x.encode() if isinstance(x, str) else x
    return len(x).to_bytes(2, "big") + x


def main():
    a = sys.argv[1:]
    if len(a) == 1:
        a = ["iot/%s/down/cmd" % device_id()] + a
    topic, payload, wait = a[0], a[1], float(a[2]) if len(a) > 2 else 4
    ctx = ssl.create_default_context()
    ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE
    c = ctx.wrap_socket(socket.create_connection(("127.0.0.1", int(os.environ.get("MQTT_PORT", "8883")))))
    c.sendall(pkt(0x10, s("MQTT") + bytes([4, 2, 0, 60]) + s("sandbox-pub")))      # MQTT 3.1.1, clean
    c.recv(4)
    c.sendall(pkt(0x82, b"\x00\x01" + s("#") + b"\x00"))                           # hear everything
    c.recv(5)
    c.sendall(pkt(0x30, s(topic) + payload.encode()))
    print("sent to", topic)
    c.settimeout(0.5)
    end, buf = time.time() + wait, b""
    while time.time() < end:
        try:
            buf += c.recv(65536)
        except (socket.timeout, ssl.SSLError):
            continue
        while len(buf) >= 2:                                                       # split PUBLISH packets
            n = shift = 0
            i = 1
            while True:
                n |= (buf[i] & 0x7F) << shift
                shift += 7
                i += 1
                if not buf[i - 1] & 0x80:
                    break
            if len(buf) < i + n:
                break
            body, first, buf = buf[i:i + n], buf[0], buf[i + n:]
            if first >> 4 == 3:
                tl = int.from_bytes(body[:2], "big")
                print("got", body[2:2 + tl].decode(), "->", body[2 + tl:].decode("utf-8", "replace"))


if __name__ == "__main__":
    main()
