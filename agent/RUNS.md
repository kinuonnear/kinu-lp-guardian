# Guardian agent on OutLayer (mainnet)

The agent is a `wasm32-wasip2` component run directly from its FastFS URL (`bind: "wasm"`), started by
the caller account (since build 5e0643de: `guardian.kinuonnear.near`; before: the test account `74890a9c…e6c1`) with `request_execution` (≈0.002 NEAR per run). Its signing
key `agent` is derived inside the TEE from the exact build and the caller; its NEAR account is that
key's implicit account. A new build has a new key, so the owner must name the new agent on the vault.

| Build (sha256) | Agent account (TEE key) | Notes |
|---|---|---|
| `285b801f…0850` | `cb226f93…a73b` | first run; read view_account wrongly (whole JSON-RPC answer) |
| `604603d8…62e2` | `f6451038…79b1` | fixed; tick on the vault → `Noop("nothing to manage")` at point 399270; tick on `wrap.near` → refused, "not the published vault build" (code hash DL2f5x…) |
| `48e4d22b…d972` | `7cd4abef…ff43` | opens a one-sided range when the vault holds one token only |
| `8c9a3c12…fbb6` | `5fe922cf…9989` | one-sided margin 20 points + `retire`; opened the position (below); retired: 0.3077 NEAR back to the owner, tx `2782fLPMiqrS4c74x3gHHoxg9CEzFGyWirvZD3aEG1BW` |
| `fa3f2004…dd6c` | `64af1438…95a4` | pins vault build `BEjFkn4o…` (gas fix); ran the Bunker exit; retired: 0.309 NEAR back, tx `3BtD8PqSQ3umvYFb6PxJZHTfNc4cLYVhfRaHLyQmWBG2` |
| `5e0643de…38b3` | `50052ebe…076a` | **current.** Caller `guardian.kinuonnear.near`. Pins vault build `8SKHf63T…` (rebalance trigger, exit on rise, owner `set_range`, `withdraw_idle`, `close_vault`); no re-entry churn; cost guard (a day of fees must pay the gas, GeckoTerminal in the TEE); no pokes with the exit rule off. Upload tx `CA8QSytEGrr3CEaZHBDvtF9pLpUL3xXnYukuwAgmtUmy`, pubkey run `5WiyGjvi6YHuYWSWVFR1ZFZcHut52mmdHWBGmJDhW6UD` (29 Oct 2026) |

From build `5e0643de…` the agent is started by the factory account `guardian.kinuonnear.near`, so its
key (and account) belongs to KINU's official deployment rather than a test account.

How to run (logged in with `outlayer login` as `guardian.kinuonnear.near`):

```bash
H=5e0643def1582ba6bb2a39b3ef5c0259978457f1b7374309fc1ef6334c4d38b3
U=https://main.fastfs.io/guardian.kinuonnear.near/outlayer.near/$H.wasm
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

## Shared contract build (29 Oct 2026)

After the per-user vaults were retired, the agent serves the shared contract `contracts/guardian`:
pinned account `lp.kinuonnear.near` running code `Gm53qinU…` (else noop), input
`{"cmd":"tick","users":[..]}` (at most 5, one transaction each), calls `request_rebalance(user, ..)`,
`exit(user)` (only by the user's exit rule) and `poke(user)`; it never opens a position and never
moves funds out. Bunker / 1Click removed (manifest allows only api.geckoterminal.com). Cost guard
uses 0.006 NEAR per move (48.8 TGas measured on DCL 2.4.0). `retire` returns gas to `kinuonnear.near`.
Build sha256 `d411bd89…ad28` (518,727 B), uploaded by the caller `agent.kinuonnear.near` to
`https://main.fastfs.io/agent.kinuonnear.near/outlayer.near/<sha256>.wasm`. Pubkey run
`DDSCB52jA6tv8PEkxdMftKyfEXRBaVpcWWTMHQmaYB5y` (≈0.002 NEAR): agent account
`feb959d818bae114c9bafa0e13aa279ed75695ac98941a40be10b952c058b6ba`
(`ed25519:J9LUsavSPs22MqEBeaNyJC2VDLiyx9tZPPiUZF5uaDah`). It must be rebuilt if the contract hash changes.

Mainnet cycle on `lp.kinuonnear.near`, 29 Oct 2026 (test user `cbe69e77…ccfc54`, 1 NEAR, USDC/NEAR 0.01%):
user join → one wNEAR deposit opened lpt `#23895` (398410..399364) → agent tick **Poke**, tx
`ABzHKj4auevM3ziPhwkmBuRBePXpHdYaek9SwugSMAMz` (`point_noted`) → user set `exit_on_rise` → agent tick
**Exit** by the user's rule, tx `J4YwcBcdzgope46MxwjrrndWZyg6VBKZuv6zN8WjXtTG` (`exit_started` by agent,
`position_closed`, 1 NEAR on the user's ledger) → next tick `Noop("no open position")` → user `cash_out`
returned 1.01 NEAR; contract Rhea balance `{}`. The first tick failed `NotEnoughBalance`: the agent
attaches 300 TGas, so its account must keep more than ~0.31 NEAR free (0.5 NEAR now).
