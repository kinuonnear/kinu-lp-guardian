# Day-1 go / no-go on testnet (owner runs the signing steps)

Goal: a contract account opens a Rhea DCL position and `list_liquidities` shows it as the contract's.

1. Owner imports the key of kinu-agent.testnet into near CLI (never paste it in chat).
2. Create sub-account probe.kinu-agent.testnet (~5 NEAR) and deploy target/near/kinu_guardian_probe/kinu_guardian_probe.wasm,
   init: new({"owner":"kinu-agent.testnet","dcl":"dclv2.ref-dev.testnet"}).
3. probe: register_dcl (attach 0.5 NEAR); register_token for both pool tokens (attach 0.00125 NEAR each).
4. kinu-agent.testnet: ft_transfer_call small amounts of usdt.fakes.testnet and wrap.testnet to the probe (msg "").
5. probe: deposit_to_dcl for each token, then add_liquidity on usdt.fakes.testnet|wrap.testnet|2000 around the current point.
6. View: dclv2.ref-dev.testnet list_liquidities({"account_id":"probe.kinu-agent.testnet"}).
   Pass = the position belongs to the probe. Then remove_liquidity, withdraw_asset, send_to_owner.

## Result (28 Oct 2026)

**Go — a contract can hold a Rhea DCL position — by on-chain evidence, not by our own run.**

- Evidence: tx `5Dj8ciYSQ5UVaDaC4Jyycotqhw4pw1KU1P4aEEfVUmP1` (testnet, 27 Sep 2026): the contract
  `lock.app3.nearpaid-dev.testnet` deposits with `ft_transfer_call` msg `"Deposit"` (DCL `ft_on_transfer`
  burnt 20 TGas) and then calls `add_liquidity` itself; the position is the contract's. Same flow as the probe.
- Our run: probe.kinu-agent.testnet deployed, registered on DCL (40 slots) and on the tokens. Every DCL
  deposit failed with "Exceeded the prepaid gas" in `ft_on_transfer` (150–270 TGas burnt), for the probe
  **and for a plain account**, with usdt.fakes.testnet, wrap.testnet and two fresh tokens (gta/gtb,
  pool `gta.kinu-agent.testnet|gtb.kinu-agent.testnet|2000` created). Funds were refunded each time.
- Cause: the DCL testnet code changed after 27 Sep (code hash 7jHmiuDC… at block 270420201 →
  CCzPD7sT… now, version 2.3.14, 11,647 pools). Deposits on testnet are broken for everyone, not for contracts.
- Next: vault integration tests against a mock DCL in near-sandbox (reproducible for judges), and one
  tiny mainnet run of the probe for the demo (needs the owner's yes); ask Rhea about testnet.

## Mainnet run (28 Oct 2026): GO, proven by our own transactions

Test account (implicit, funded by the owner with 4 NEAR + 3 USDC):
`74890a9ca6436e1da0b9a730a020223756e520c9812b97804b5c4a13be19e6c1`, probe contract deployed on it.

1. register_dcl on `dclv2.ref-labs.near` (0.5 NEAR) and USDC storage: ok.
2. deposit_to_dcl 3 USDC (`ft_transfer_call` msg `"Deposit"`): tx `D1MbfXDdiPZEvMMCnXB6D1ekms3xDgMAK5Dyc7iT4oHp`, DCL balance 3,000,000.
3. add_liquidity on `USDC|wrap.near|100`, one-sided range 399213..399413 above the price (399193):
   tx `948npgZe1nvQMJjPPFrMcN3vJysasKSYJbXL6wAFtLUp`. `list_liquidities` returns lpt `...|100#23856`
   with **owner_id = the contract**. A contract can hold a Rhea DCL position on mainnet.
4. Bug found: `remove_liquidity` with `amount: "0"` only claims fees (DCL event `removed_amount: "0"`),
   so the position stayed intact. Closing needs the position's liquidity from `get_liquidity`. Fixed in
   the probe and the vault (the vault reads `get_liquidity` before every close); the mock DCL now
   behaves the same, so the sandbox test would catch it.
5. remove_liquidity with amount 7024520687189: tx `HJ7MNLfjzVUrRZAiNyPU1d5ohX7sCMFudcR5pWUht8fp`;
   withdraw_asset: tx `2gbXhLsrmkzeBRRgBKW8JLqKLB9TkHKDKQ2YYtKJFbft`; 2.999999 USDC returned to
   kinuonnear.near: tx `BFvfGS7VoZY5M7gPS3av41Q3BWAnZwmw42xThLG6Kuwq`.

## Bunker exit on mainnet (28 Oct 2026): PASSED

The vault contract (this repo, commit 59697c2) deployed on the test account
`74890a9ca6436e1da0b9a730a020223756e520c9812b97804b5c4a13be19e6c1`, owner = that account,
exit address = the owner's Zodl (formerly Zashi) unified address `u1mpwfx74…n7r5p`
(receivers checked on-chain: Sapling + Orchard, no transparent).

1. `set_exit_zec`: tx `9Bfa3xxN5CkfxXWgvwXieLosuhtfBAS1nkDkZTnG3Roz`, event receivers [2, 3].
2. The owner sent 3 USDC to the vault; `tools/bunker-quote.mjs` fetched a live 1Click quote
   (deposit address `429511c24496456e33d1b87d372ac1eb15108daa3e319c3c94dfd4fef7e70031`, refundTo = the vault).
3. `bunker_exit(token, signed, signature)`: tx `D7dmWwZBe1DRvkRLMujrsBFRRsrTGx1mGNrwKjFEThCE`.
   The vault verified 1Click's ed25519 signature on-chain, checked recipient / ZEC / refund / deadline,
   paid the deposit address 3,000,000 USDC, events `bunker_exit_started` → `bunker_exit_sent`.
4. NEAR Intents status SUCCESS about 2 minutes later: **0.00203427 ZEC** delivered on Zcash,
   tx `362d709d6d5f5828efb4fe06a65034a9a722d1a06d22a52b9d60a65de59ae5f1`, into the shielded pool
   (the address has no transparent receiver).
