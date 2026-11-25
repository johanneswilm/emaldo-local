#!/usr/bin/env python3
# Minimal MQTT 3.1.1 sink for testing the driver: accepts CONNECT,
# SUBSCRIBE, PUBLISH (QoS 0/1), PINGREQ; logs every published topic.
import socket, struct, sys, threading

def read_packet(conn):
    hdr = conn.recv(1)
    if not hdr:
        return None, None
    mult, rem = 1, 0
    while True:
        b = conn.recv(1)[0]
        rem += (b & 0x7F) * mult
        if not b & 0x80:
            break
        mult *= 128
    data = b""
    while len(data) < rem:
        chunk = conn.recv(rem - len(data))
        if not chunk:
            return None, None
        data += chunk
    return hdr[0], data

def handle(conn, addr):
    try:
        while True:
            hdr, data = read_packet(conn)
            if hdr is None:
                return
            t = hdr >> 4
            if t == 1:  # CONNECT
                conn.sendall(b"\x20\x02\x00\x00")
            elif t == 8:  # SUBSCRIBE
                mid = data[:2]
                conn.sendall(b"\x90\x03" + mid + b"\x00")
            elif t == 3:  # PUBLISH
                tl = struct.unpack(">H", data[:2])[0]
                topic = data[2:2+tl].decode()
                rest = data[2+tl:]
                qos = (hdr >> 1) & 3
                if qos:
                    mid = rest[:2]
                    payload = rest[2:]
                    conn.sendall(b"\x40\x02" + mid)
                else:
                    payload = rest
                print(f"PUB {topic} = {payload[:120]!r}", flush=True)
            elif t == 12:  # PINGREQ
                conn.sendall(b"\xd0\x00")
    except OSError:
        return

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 1883))
srv.listen(8)
print("fake broker on 127.0.0.1:1884", flush=True)
while True:
    c, a = srv.accept()
    threading.Thread(target=handle, args=(c, a), daemon=True).start()
