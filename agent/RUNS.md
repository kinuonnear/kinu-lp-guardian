# Guardian agent on OutLayer (mainnet)

The agent is a `wasm32-wasip2` component run directly from its FastFS URL (`bind: "wasm"`), started by
the vault owner's account `74890a9c…e6c1` with `request_execution` (≈0.002 NEAR per run). Its signing
key `agent` is derived inside the TEE from the exact build and the caller; its NEAR account is that
key's implicit account. A new build has a new key, so the owner must name the new agent on the vault.

| Build (sha256) | Agent account (TEE key) | Notes |
|---|---|---|
| `285b801f…0850` | `cb226f93…a73b` | first run; read view_account wrongly (whole JSON-RPC answer) |
| `604603d8…62e2` | `f6451038…79b1` | fixed; tick on the vault → `Noop("nothing to manage")` at point 399270; tick on `wrap.near` → refused, "not the published vault build" (code hash DL2f5x…) |
| `48e4d22b…d972` | `7cd4abef…ff43` | opens a one-sided range when the vault holds one token only |

How to run (owner logged in with `outlayer login`):

```bash
H=48e4d22b8fe9859c7e7adad43e020083f9bb2ba2e1c2115af2058b57d706d972
U=https://main.fastfs.io/74890a9ca6436e1da0b9a730a020223756e520c9812b97804b5c4a13be19e6c1/outlayer.near/$H.wasm
echo '{"cmd":"pubkey"}' > in.json && outlayer run --wasm $U --hash $H --input in.json
echo '{"cmd":"tick","vault":"<vault account>"}' > in.json && outlayer run --wasm $U --hash $H --input in.json
```
