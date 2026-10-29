#!/usr/bin/env python3
"""Fetches the mainnet contracts the sandbox tests run against (Rhea DCL, wrap.near) into
tests/fixtures/, and checks each against the code hash the tests were written for.

    python3 scripts/fetch_fixtures.py
"""
import base64
import hashlib
import json
import os
import sys
import urllib.request

RPC = "https://free.rpc.fastnear.com"
FIXTURES = os.path.join(os.path.dirname(__file__), "..", "tests", "fixtures")
WANT = {
    # file: (account, NEAR code hash)
    "dcl-2.4.0.wasm": ("dclv2.ref-labs.near", "FvsWeo3ZBAAZs6mdFAatf2kvga37y5BiRkRnW8GvfZix"),
    "wrap-near.wasm": ("wrap.near", "DL2f5xmZ44cQRP6vBMt94rvseH5d5KKnAGis2WkTAsh1"),
}
ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58(data: bytes) -> str:
    n = int.from_bytes(data, "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = ALPHABET[r] + out
    return "1" * (len(data) - len(data.lstrip(b"\0"))) + out


def view_code(account: str) -> bytes:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "query", "params": {"request_type": "view_code", "finality": "final", "account_id": account}}).encode()
    req = urllib.request.Request(RPC, body, {"content-type": "application/json"})
    res = json.load(urllib.request.urlopen(req, timeout=60))
    if "error" in res:
        sys.exit(f"{account}: {res['error']}")
    return base64.b64decode(res["result"]["code_base64"])


ok = True
for name, (account, want) in WANT.items():
    code = view_code(account)
    got = b58(hashlib.sha256(code).digest())
    if got != want:
        print(f"{name}: {account} now runs {got}, the tests expect {want} (a new version: re-check before relying on it)")
        ok = False
        continue
    with open(os.path.join(FIXTURES, name), "wb") as f:
        f.write(code)
    print(f"{name}: {account} {got} ({len(code)} bytes)")
sys.exit(0 if ok else 1)
