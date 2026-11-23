#!/usr/bin/env python3
"""TLS probe: terminate TLS offered to the Emaldo module and record what it sends.

Listens on 127.0.0.1:3443 (redirected https) and 0.0.0.0:3883 (redirected MQTT)
inside the capture container. If the module ignores certificate validity it
completes the handshake and sends app data — that is the experiment's answer.
For MQTT we answer CONNACK=accepted so the module reveals its topic scheme.
Everything is logged; nothing is forwarded — restore iptables to heal.
"""
import socket
import ssl
import threading
import time

CERT = "/app/mitm/probe.pem"
LOG = open("/app/mitm/probe.log", "a", buffering=1)


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=LOG)


def hexdump(b):
    return b[:400].hex() + ("..." if len(b) > 400 else "")


def ascii_dump(b):
    return "".join(chr(c) if 32 <= c < 127 else "." for c in b[:200])


def handle(conn, kind, answer_mqtt):
    peer = conn.getpeername()
    log(f"{kind}: TCP connection from {peer}")
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(CERT)
    ctx.set_ciphers("ALL:@SECLEVEL=0")
    sni = {}

    def sni_cb(server, name, data):
        sni["v"] = name

    ctx.sni_callback = sni_cb
    try:
        tls = ctx.wrap_socket(conn, server_side=True)
    except Exception as e:
        log(f"{kind}: TLS handshake FAILED from {peer}: {e}")
        conn.close()
        return
    log(f"{kind}: TLS established peer={peer} sni={sni.get('v')!r} "
        f"version={tls.version()} cipher={tls.cipher()[0]}")
    tls.settimeout(4)
    try:
        first = tls.recv(2048)
    except Exception as e:
        log(f"{kind}: no app data after handshake: {e}")
        tls.close()
        return
    log(f"{kind}: first {len(first)}B hex={hexdump(first)}")
    log(f"{kind}: first ascii={ascii_dump(first)}")
    if answer_mqtt and first and first[0] == 0x10:  # CONNECT
        connack = bytearray(b"\x20\x02\x00\x00")
        tls.sendall(bytes(connack))
        log("mqtt: sent CONNACK accepted")
        # keep session a bit to capture SUBSCRIBEs / publishes
        t_end = time.time() + 12
        while time.time() < t_end:
            try:
                d = tls.recv(2048)
            except Exception:
                break
            if not d:
                break
            log(f"mqtt: +{len(d)}B hex={hexdump(d)} ascii={ascii_dump(d)}")
    tls.close()


def listen(port, kind, answer_mqtt=False):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    s.listen(5)
    log(f"listening :{port} ({kind})")
    while True:
        c, _ = s.accept()
        threading.Thread(target=handle, args=(c, kind, answer_mqtt), daemon=True).start()


threading.Thread(target=listen, args=(3443, "https"), daemon=True).start()
threading.Thread(target=listen, args=(3883, "mqtt", True), daemon=True).start()
while True:
    time.sleep(3600)
