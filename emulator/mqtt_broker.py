#!/usr/bin/env python3
"""Minimal logging MQTT broker for the emulator's sandbox (stdlib only).

The app's cloud MQTT client (hps-mqtt, MQTT 5 over TLS) only needs a broker
that accepts it; this one does that and writes down everything it is told:
the CONNECT (client id, user name, password, will), every SUBSCRIBE and every
PUBLISH. It speaks MQTT 3.1.1 and 5, QoS 0-2, and routes publishes between
its clients (with + and # wildcards), so a test client can also send the app
a message. No retained messages, no sessions, no authentication.

Environment:
  MQTT_PORT   listen port (default 8883), all addresses
  MQTT_TLS    0 = plain TCP (default 1: TLS with the sandbox certificate)
  MQTT_LOG    JSON lines (default /sandbox/mqtt.log)
  SB_DIR      where sandbox.crt / sandbox.key are (default /sandbox)
"""
import json, os, socketserver, ssl, threading, time

SB = os.environ.get("SB_DIR", "/sandbox")
LOG = os.environ.get("MQTT_LOG", SB + "/mqtt.log")
PORT = int(os.environ.get("MQTT_PORT", "8883"))
lock = threading.Lock()
clients = {}            # handler -> list of topic filters


def note(**rec):
    rec = dict(t=time.strftime("%H:%M:%S"), **rec)
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(rec) + "\n")


def text(b):
    try:
        s = b.decode("utf-8")
        return s if s.isprintable() or "\n" in s else b.hex()
    except UnicodeDecodeError:
        return b.hex()


class Reader:
    def __init__(self, data):
        self.d, self.i = data, 0

    def take(self, n):
        out = self.d[self.i:self.i + n]
        self.i += n
        return out

    def u8(self): return self.take(1)[0]
    def u16(self): return int.from_bytes(self.take(2), "big")
    def bin(self): return self.take(self.u16())
    def str(self): return self.bin().decode("utf-8", "replace")
    def rest(self): return self.take(len(self.d) - self.i)

    def varint(self):
        n = shift = 0
        while True:
            b = self.u8()
            n |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                return n

    def props(self):
        """MQTT 5 properties, decoded as far as their types are known."""
        r, out = Reader(self.take(self.varint())), {}
        kinds = {1: "u8", 2: "u32", 3: "str", 8: "str", 9: "bin", 11: "var", 17: "u32", 18: "str",
                 19: "u16", 21: "str", 22: "bin", 23: "u8", 24: "u32", 25: "u8", 26: "str", 28: "str",
                 31: "str", 33: "u16", 34: "u16", 35: "u16", 36: "u8", 37: "u8", 38: "pair", 39: "u32",
                 40: "u8", 41: "u8", 42: "u8"}
        names = {2: "message_expiry", 3: "content_type", 8: "response_topic", 9: "correlation_data",
                 17: "session_expiry", 21: "auth_method", 22: "auth_data", 33: "receive_maximum",
                 34: "topic_alias_maximum", 35: "topic_alias", 38: "user", 39: "maximum_packet_size"}
        try:
            while r.i < len(r.d):
                k = r.varint()
                kind = kinds.get(k)
                if kind is None:
                    out["unparsed"] = r.rest().hex()
                    break
                v = {"u8": r.u8, "u16": r.u16, "u32": lambda: int.from_bytes(r.take(4), "big"),
                     "str": r.str, "bin": lambda: r.bin().hex(), "var": r.varint,
                     "pair": lambda: [r.str(), r.str()]}[kind]()
                name = names.get(k, "p%d" % k)
                if kind == "pair":
                    out.setdefault(name, []).append(v)
                else:
                    out[name] = v
        except IndexError:
            out["error"] = "short properties"
        return out


def varint(n):
    out = b""
    while True:
        b, n = n & 0x7F, n >> 7
        out += bytes([b | (0x80 if n else 0)])
        if not n:
            return out


def packet(first, body):
    return bytes([first]) + varint(len(body)) + body


def matches(flt, topic):
    f, t = flt.split("/"), topic.split("/")
    for i, part in enumerate(f):
        if part == "#":
            return True
        if i >= len(t) or (part != "+" and part != t[i]):
            return False
    return len(f) == len(t)


class Client(socketserver.BaseRequestHandler):
    def read_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.request.recv(n - len(buf))
            if not chunk:
                raise ConnectionError
            buf += chunk
        return buf

    def send(self, data):
        with self.wlock:
            self.request.sendall(data)

    def handle(self):
        self.wlock, self.v5, self.name = threading.Lock(), False, "?"
        peer = "%s:%s" % self.client_address[:2]
        try:
            while True:
                first = self.read_exact(1)[0]
                n = shift = 0
                while True:
                    b = self.read_exact(1)[0]
                    n |= (b & 0x7F) << shift
                    shift += 7
                    if not b & 0x80:
                        break
                self.packet(first, Reader(self.read_exact(n)), peer)
        except (ConnectionError, OSError, IndexError, ssl.SSLError) as e:
            note(ev="closed", client=self.name, peer=peer, why=type(e).__name__)
        finally:
            with lock:
                clients.pop(self, None)

    def packet(self, first, r, peer):
        kind, flags = first >> 4, first & 0x0F
        if kind == 1:                                          # CONNECT
            proto, level, cf, keep = r.str(), r.u8(), r.u8(), r.u16()
            self.v5 = level >= 5
            rec = dict(ev="connect", peer=peer, proto="%s/%d" % (proto, level), keepalive=keep,
                       clean=bool(cf & 2))
            if self.v5:
                rec["props"] = r.props()
            self.name = rec["client"] = r.str()
            if cf & 4:                                         # will
                if self.v5:
                    rec["will_props"] = r.props()
                rec["will_topic"], rec["will"] = r.str(), text(r.bin())
            if cf & 0x80:
                rec["username"] = r.str()
            if cf & 0x40:
                rec["password"] = text(r.bin())
            note(**rec)
            with lock:
                clients[self] = []
            self.send(packet(0x20, b"\x00\x00" + (b"\x00" if self.v5 else b"")))
        elif kind == 3:                                        # PUBLISH
            qos = (flags >> 1) & 3
            topic = r.str()
            pid = r.u16() if qos else None
            props = r.props() if self.v5 else {}
            payload = r.rest()
            rec = dict(ev="publish", client=self.name, topic=topic, qos=qos, retain=bool(flags & 1),
                       payload=text(payload))
            if props:
                rec["props"] = props
            note(**rec)
            if qos == 1:
                self.send(packet(0x40, pid.to_bytes(2, "big")))
            elif qos == 2:
                self.send(packet(0x50, pid.to_bytes(2, "big")))
            with lock:
                targets = [c for c, flts in clients.items() if c is not self and any(matches(f, topic) for f in flts)]
            for c in targets:                                  # forwarded at QoS 0
                body = len(topic.encode()).to_bytes(2, "big") + topic.encode() + (b"\x00" if c.v5 else b"") + payload
                try:
                    c.send(packet(0x30, body))
                except OSError:
                    pass
        elif kind == 6:                                        # PUBREL -> PUBCOMP
            self.send(packet(0x70, r.take(2)))
        elif kind == 8:                                        # SUBSCRIBE
            pid = r.take(2)
            props = r.props() if self.v5 else {}
            topics = []
            while r.i < len(r.d):
                t = r.str()
                topics.append((t, r.u8() & 3))
            with lock:
                clients.setdefault(self, []).extend(t for t, _ in topics)
            note(ev="subscribe", client=self.name, topics=[t for t, _ in topics], **({"props": props} if props else {}))
            self.send(packet(0x90, pid + (b"\x00" if self.v5 else b"") + bytes(q for _, q in topics)))
        elif kind == 10:                                       # UNSUBSCRIBE
            pid = r.take(2)
            if self.v5:
                r.props()
            topics = []
            while r.i < len(r.d):
                topics.append(r.str())
            with lock:
                clients[self] = [t for t in clients.get(self, []) if t not in topics]
            note(ev="unsubscribe", client=self.name, topics=topics)
            self.send(packet(0xB0, pid + (b"\x00" + b"\x00" * len(topics) if self.v5 else b"")))
        elif kind == 12:                                       # PINGREQ
            self.send(b"\xd0\x00")
        elif kind == 14:                                       # DISCONNECT
            note(ev="disconnect", client=self.name, reason=r.u8() if self.v5 and len(r.d) else 0)
            raise ConnectionError
        elif kind not in (4, 5, 7):                            # acks to our own packets are fine
            note(ev="packet", client=self.name, kind=kind, body=r.rest().hex()[:200])


class Server(socketserver.ThreadingMixIn, socketserver.TCPServer):
    daemon_threads = True
    allow_reuse_address = True
    ctx = None

    def get_request(self):
        sock, addr = super().get_request()
        if self.ctx:
            sock.settimeout(15)
            try:
                sock = self.ctx.wrap_socket(sock, server_side=True)
            except (ssl.SSLError, OSError) as e:
                note(ev="tls-failed", peer="%s:%s" % addr[:2], why=str(e)[:120])
                raise
            sock.settimeout(None)
        return sock, addr

    def handle_error(self, request, client_address):
        pass


if __name__ == "__main__":
    with Server(("0.0.0.0", PORT), Client) as srv:
        if os.environ.get("MQTT_TLS", "1") != "0":
            srv.ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            srv.ctx.load_cert_chain(os.path.join(SB, "sandbox.crt"), os.path.join(SB, "sandbox.key"))
        note(ev="listening", port=PORT, tls=bool(srv.ctx))
        srv.serve_forever()
