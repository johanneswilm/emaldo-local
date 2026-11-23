#!/usr/bin/env python3
"""Minimal websocket sink (stdlib only): accepts the app's "hermes spy" feed and
logs every message. usage: ws_sink.py [port]   log: /sandbox/spy.out"""
import base64, hashlib, socket, struct, sys, threading, time
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18765
log = open("/sandbox/spy.out", "a", buffering=1)
def say(m): log.write(f"{time.strftime('%T')} {m}\n")
def rd(c, n):
    b = b""
    while len(b) < n:
        x = c.recv(n - len(b))
        if not x: raise EOFError
        b += x
    return b
def serve(c, peer):
    try:
        req = b""
        while b"\r\n\r\n" not in req: req += c.recv(4096)
        say(f"{peer} handshake: {req[:400]!r}")
        key = [l.split(b":", 1)[1].strip() for l in req.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key")][0]
        acc = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
        c.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + acc + b"\r\n\r\n")
        while True:
            h = rd(c, 2); op, ln = h[0] & 0xF, h[1] & 0x7F
            if ln == 126: ln = struct.unpack(">H", rd(c, 2))[0]
            elif ln == 127: ln = struct.unpack(">Q", rd(c, 8))[0]
            mask = rd(c, 4) if h[1] & 0x80 else b"\0\0\0\0"
            data = bytes(b ^ mask[i % 4] for i, b in enumerate(rd(c, ln)))
            if op == 9: c.sendall(b"\x8a\x00"); continue
            if op == 8: break
            say(f"msg op={op} len={ln}: {data[:3000]!r}")
    except Exception as e:
        say(f"{peer} closed: {e!r}")
    finally:
        c.close()
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", PORT)); s.listen(5); say(f"ws sink on {PORT}")
while True:
    c, a = s.accept()
    threading.Thread(target=serve, args=(c, a), daemon=True).start()
