import dpkt, json, collections
from datetime import datetime
from Crypto.Cipher import AES
from Crypto.Util.Padding import unpad

e=json.load(open("/home/johannes/src/homeassistant/emaldo-capture/e2e_device_creds.json"))["Result"]["e2es"][0]
keys=[("chat",e["chat_secret"].encode()),("end",e["end_secret"].encode()),("chat16",e["chat_secret"].encode()[:16])]
DEV=b"\xc0\xa82\xb9"

def nonce_of(d):
    i=d.find(b"\x90\xa3")
    return d[i+2:i+18] if i>=0 else None

def dec_slide(d,nonce):
    if not nonce: return None
    idx=d.find(nonce)
    best=None
    for o in range(idx+16, len(d)-15):
        c=d[o:]
        if len(c)%16: continue
        for kn,k in keys:
            try:
                pt=unpad(AES.new(k,AES.MODE_CBC,nonce).decrypt(c),16)
                if pt[:1] in (b"{",b"["): return kn,o,pt
                if best is None: best=(kn,o,pt)
            except Exception: pass
    return best

rows=[]
f=open("/home/johannes/src/homeassistant/emaldo-capture/captures/e2e_transcript_0702.pcap","rb")
for ts,pkt in dpkt.pcap.Reader(f):
    ip=dpkt.ethernet.Ethernet(pkt).data; udp=ip.data; d=bytes(udp.data)
    src="dev" if bytes(ip.src)==DEV else "ext"
    nonce=nonce_of(d)
    r=dec_slide(d,nonce) if nonce else None
    meth=None
    if b"\x85\xf5alive" in d: meth="alive"
    elif b"\x89\xf5heartbeat" in d: meth="heartbeat"
    elif b"\x84\xf5wake" in d: meth="wake"
    elif b"\x82\xf5" in d:
        i=d.find(b"\x82\xf5"); meth=f"cmd{d[i+2]:02x}{d[i+3]:02x}"
    mid=d.find(b"and_")
    msgid=d[mid:mid+27].decode(errors='replace') if mid>=0 else None
    rows.append((datetime.fromtimestamp(ts).strftime("%H:%M:%S.%f")[:12],src,len(d),meth,(nonce or b'').hex(),msgid,r[0] if r else None,(r[2][:140].decode(errors='replace') if r else None)))
print(collections.Counter(x[3] for x in rows)); print(collections.Counter(x[1] for x in rows))
for x in rows[:30]: print(x[0],x[1],x[2],x[3],x[6] or "", (x[7] or "")[:110])
