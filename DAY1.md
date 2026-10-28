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
