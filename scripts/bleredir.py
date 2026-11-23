#!/usr/bin/env python3
"""BLE host re-point helpers for the Emaldo module (learn-phase).

usage: bleredir.py learn|restore|status
learn   -> api/data hosts = emaldo.lund.fiduswriter.org (our proxy)
restore -> api.emaldo.com / dp.emaldo.com (production)
status  -> get_version sanity only
"""
import asyncio
import sys

from bleak import BleakClient
from Crypto.Cipher import AES
from Crypto.Util.Padding import pad, unpad

KEY = b"fbade9e36a3f36d3d676c1b808451dd7"
IV = b"dfcf28d0734569a6"
W = "156df971-1dd5-44d6-a381-f271821f6804"
N = "1c7d885f-e617-49e9-8e5d-6ac6d9ed8765"
ADDR = os.environ["EMALDO_MODULE_MAC"]   # BLE MAC of your module (see README: find via bluetoothctl scan)
CLONE_HOST = os.environ["EMALDO_PROXY_HOST"]  # your learning proxy hostname (valid TLS cert for it)

CMDSETS = {
    "learn": [
        '{"cmd": "set_http_host", "data": "emaldo.lund.fiduswriter.org"}',
        '{"cmd": "set_data_host", "data": "emaldo.lund.fiduswriter.org"}',
        '{"cmd": "restart_network"}',
    ],
    "restore": [
        '{"cmd": "set_http_host", "data": "api.emaldo.com"}',
        '{"cmd": "set_data_host", "data": "dp.emaldo.com"}',
    ],
    "status": ['{"cmd": "get_version"}', '{"cmd": "get_model"}'],
}


def enc(s: str) -> bytes:
    return AES.new(KEY, AES.MODE_CBC, IV).encrypt(pad(s.encode(), 16))


def dec(b: bytes) -> str:
    try:
        return unpad(AES.new(KEY, AES.MODE_CBC, IV).decrypt(b), 16).decode()
    except Exception:
        return b.hex()


async def main() -> None:
    mode = sys.argv[1] if len(sys.argv) > 1 else "status"
    cmds = CMDSETS[mode]
    async with BleakClient(ADDR, timeout=20) as cl:
        got: list[bytes] = []
        await cl.start_notify(N, lambda _, d: got.append(bytes(d)))
        for cmd in cmds:
            got.clear()
            await cl.write_gatt_char(W, enc(cmd), response=True)
            for _ in range(12):
                await asyncio.sleep(0.25)
                if got:
                    break
            print(cmd, "->", [dec(g) for g in got])
        print(f"MODE={mode} DONE")


asyncio.run(main())
