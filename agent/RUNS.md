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
| `8c9a3c12…fbb6` | `5fe922cf…9989` | one-sided margin 20 points + `retire`; opened the position (below); retired: 0.3077 NEAR back to the owner, tx `2782fLPMiqrS4c74x3gHHoxg9CEzFGyWirvZD3aEG1BW` |
| `fa3f2004…dd6c` | `64af1438…95a4` | pins vault build `BEjFkn4o…` (gas fix); ran the Bunker exit; retired: 0.309 NEAR back, tx `3BtD8PqSQ3umvYFb6PxJZHTfNc4cLYVhfRaHLyQmWBG2` |

How to run (owner logged in with `outlayer login`):

```bash
H=fa3f20041c4d7c2f4e6cf08fea488d9856e0a68f6770b1d9db700c6a006cdd6c
U=https://main.fastfs.io/74890a9ca6436e1da0b9a730a020223756e520c9812b97804b5c4a13be19e6c1/outlayer.near/$H.wasm
echo '{"cmd":"pubkey"}' > in.json && outlayer run --wasm $U --hash $H --input in.json
echo '{"cmd":"tick","vault":"<vault account>"}' > in.json && outlayer run --wasm $U --hash $H --input in.json
```

## Full cycle on mainnet, 29 Oct 2026 — every agent step signed in the TEE

Vault `74890a9c…e6c1` (owner = itself), pool `USDC|wrap.near|100` on `dclv2.ref-labs.near`,
exit address = the owner's Zodl (formerly Zashi) unified address (Sapling + Orchard only).

1. Owner deposited 3 USDC into the vault's Rhea DCL balance (tx `5vFncp88ucLkYgXRFrDAmkKp4vHZnu9HocaWBsKAkGHE`).
2. **Agent tick → opened the position.** Read point 399259, chose a one-sided range 399280..399480
   (USDC only), signed `request_rebalance` in the TEE: tx `959zMuUujkg8TNx12N7kZGPknXiR5X3B2V8xgeBjJqNG`.
   The vault re-checked it and opened lpt `…|100#23861`, owner_id = the vault.
3. Found: a full `remove_liquidity` fails with 30 TGas on mainnet ("Exceeded the prepaid gas"); the
   vault reported `exited` anyway. Fixed: remove gets 110 TGas, budgets re-measured, and `on_exited`
   emits `exit_failed` unless Rhea really closed the position. Vault code upgraded in place
   (state kept) to build `BEjFkn4oiy48fyjgZm2vPHfFLabV1rWwes2Q1DBfAzW6`.
4. Owner exit: `liquidity_removed` → `position_closed` → `exited`; 2,999,999 USDC back in the vault wallet.
5. **Agent tick → Bunker exit.** Saw the exited USDC, fetched a live 1Click quote from inside the TEE,
   signed `bunker_exit`: tx `HtPnu65aV4fVZqpJjCuHfFboW1sCMi1PbLfxVy7o3DUC`. The vault verified 1Click's
   signature on-chain (event `bunker_exit_started` by agent → `bunker_exit_sent`).
6. NEAR Intents SUCCESS: **0.00202477 ZEC** delivered on Zcash, tx
   `42a02880183611ef3154e1dfe3eb720e4b0965f6f364523633d6c8bdfee4b1ba`, into the shielded pool.
7. **Agent retire** returned its gas to the owner (0.309 NEAR).
