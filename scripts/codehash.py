#!/usr/bin/env python3
"""Prints each wasm's NEAR code hash (base58 sha256) and size, to compare with view_account."""
import hashlib
import sys

ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

for path in sys.argv[1:]:
    data = open(path, "rb").read()
    n = int.from_bytes(hashlib.sha256(data).digest(), "big")
    out = ""
    while n:
        n, r = divmod(n, 58)
        out = ALPHABET[r] + out
    print(f"{out}  {len(data):>7} bytes  {path}")
