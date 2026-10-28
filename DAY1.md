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
