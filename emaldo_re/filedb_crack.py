#!/usr/bin/env python3
"""Try to crack the app's local filedb record (baseinfo_v1).

/root/filedb/baseinfo_v1 in the emulator guest is 1900 bytes of ciphertext the
app rewrites at every boot. If the store key derives from values we already know
(device id, model, reg token, home id, the vendor APP_SECRET) then the record can
be forged locally - which is the cheapest way to get the appwizard out of the
Unconfigured state. It does NOT decode under APP_SECRET alone (checked: RC4 and
AES-CBC/ECB, all low entropy scores), so this sweeps plausible key derivations.

Usage: filedb_crack.py /tmp/baseinfo_v1.bin
"""
import hashlib
import itertools
import sys
from Crypto.Cipher import AES


def rc4(key, data):
    S = list(range(256))
    j = 0
    for i in range(256):
        j = (j + key[i % len(key)]) & 0xFF
        S[i], S[j] = S[j], S[i]
    out = bytearray()
    i = j = 0
    for c in data:
        i = (i + 1) & 0xFF
        j = (j + S[i]) & 0xFF
        S[i], S[j] = S[j], S[i]
        out.append(c ^ S[(S[i] + S[j]) & 0xFF])
    return bytes(out)


IDENT = {
    "APP_SECRET": b"FpF4Uqiio9k8p9VUSX36UZxy9wLs7ybT",
    "device_id": b"m8NKTSSPivOUNmbV",
    "regtoken_ascii": b"6d616c646f656d756c61746f72656d31",
    "regtoken_txt": b"emulator-reg-token-0001",
    "model": b"PC1-BAK15-HS10",
    "home_id": b"6ac8a4b4c4eee0acceaf7f77",
    "user_id": b"6ac8a4b4c4eee0acceaf7f72",
    "uid": b"johanneswilm.ha",
    "session_token": b"uid_3d5111115a28fc34e2e4546108d23dca",
    "empty": b"",
    # the store name itself is a likely salt: baseinfo_v1 / "baseinfo" as key material
    "key_baseinfo": b"baseinfo",
    "key_filedb": b"filedb",
    "key_v1": b"v1",
    "key_baseinfo_v1": b"baseinfo_v1",
}


def keys():
    out = dict(IDENT)
    for a, b in itertools.permutations(IDENT, 2):
        out[a + "+" + b] = IDENT[a] + IDENT[b]
    for n, v in IDENT.items():
        out[n + ".md5"] = hashlib.md5(v).digest()
        out[n + ".sha1"] = hashlib.sha1(v).digest()
        out[n + ".sha256"] = hashlib.sha256(v).digest()
        out[n + ".md5hex"] = hashlib.md5(v).hexdigest().encode()
        out[n + ".sha256hex"] = hashlib.sha256(v).hexdigest().encode()
    for n in ("APP_SECRET", "device_id", "model", "home_id"):
        for m in ("APP_SECRET", "device_id", "home_id"):
            out[n + "." + m + ".md5"] = hashlib.md5(IDENT[n] + IDENT[m]).digest()
    return out


def score(pt):
    """JSON-shaped output is the only real signal; random bytes start with an
    ASCII bracket 2/256 of the time, so require a key-ish prefix too."""
    if not pt:
        return 0, ""
    printable = sum(1 for b in pt[:96] if 32 <= b < 127 or b in (9, 10, 13)) / 96
    txt = pt[:70].decode("latin1").replace("\n", " ")
    head = pt[:2].decode("latin1")
    if head in ('{"', "[{", '["') and printable > 0.8:
        return 3, txt
    if printable > 0.95:
        return 2, txt
    return 0, txt


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/baseinfo_v1.bin"
    data = open(path, "rb").read()
    print("file %s len %d head %s" % (path, len(data), data[:16].hex()))
    hits = []
    for kname, k in keys().items():
        if not k:
            continue
        cands = {"rc4": rc4(k, data), "rc4[4:]": rc4(k, data[4:]),
                 "rc4[16:]": rc4(k, data[16:])}
        kk = k[:16] if len(k) >= 16 else (k + b"\0" * (16 - len(k)))
        try:
            cands["aes_cbc_ivself"] = AES.new(kk, AES.MODE_CBC, data[:16]).decrypt(
                data[16:16 + ((len(data) - 16) // 16) * 16])
            cands["aes_cbc_iv0"] = AES.new(kk, AES.MODE_CBC, b"\0" * 16).decrypt(
                data[:len(data) // 16 * 16])
            cands["aes_ecb"] = b"".join(
                AES.new(kk, AES.MODE_ECB).decrypt(data[i:i + 16])
                for i in range(0, len(data) // 16 * 16, 16))
            cands["aes_ctr"] = AES.new(kk, AES.MODE_CTR, nonce=data[:8]).decrypt(data[8:])
            # an AEAD store explains why every stream/CBC guess looked random:
            # if a GCM tag validates, the key and layout are proven at once.
            for nl in (12, 8, 16):
                try:
                    c = AES.new(kk, AES.MODE_GCM, nonce=data[:nl])
                    c.decrypt_and_verify(data[nl:-16], data[-16:])
                    cands["aes_gcm_n%d" % nl] = data[nl:-16]
                except (ValueError, KeyError, IndexError):
                    pass
            # deterministic output across boots => fixed IV; common fixed-IV
            # choices are the key itself or a hash of it, not zeros.
            for ivname, iv in (("ivkey", kk), ("ivmd5", hashlib.md5(k).digest()),
                               ("ivsha", hashlib.sha256(k).digest()[:16]),
                               ("ivseq", bytes(range(16)))):
                try:
                    cands["aes_cbc_" + ivname] = AES.new(kk, AES.MODE_CBC, iv).decrypt(
                        data[:len(data) // 16 * 16])
                except ValueError:
                    pass
            cands["aes_cfb"] = AES.new(kk, AES.MODE_CFB, data[:16]).decrypt(data[16:])
            cands["aes_ofb"] = AES.new(kk, AES.MODE_OFB, data[:16]).decrypt(data[16:])
        except ValueError:
            pass
        for mode, pt in cands.items():
            sc, txt = score(pt)
            if sc:
                hits.append((sc, kname, mode, txt))
    hits.sort(reverse=True)
    if not hits:
        print("no candidate produced readable output (key is not identity-derived)")
        return 0
    for sc, kname, mode, txt in hits[:12]:
        print("score=%d key=%s mode=%-16s %r" % (sc, kname, mode, txt))
    return 0


if __name__ == "__main__":
    sys.exit(main())
