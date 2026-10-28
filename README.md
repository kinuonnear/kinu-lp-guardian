# KINU LP Guardian

An agent may manage a Rhea DCL (concentrated liquidity) position on NEAR, but only inside rules the
owner sets on-chain. The vault checks every argument the agent sends (pool, range width, distance from
the live price, rate limit) against the owner's policy and the pool it reads itself. A compromised agent
gets refused on-chain. The owner can freeze, revoke the agent, and register a **shielded Zcash exit
address** (unified address with an Orchard receiver) for a "bunker exit".

Status (28 Oct 2026): first slice. Policy checks, rate limit, freeze/revoke, Orchard-only exit address
validation (ZIP 316: bech32m + F4Jumble), and the rebalance request that reads the pool from Rhea DCL.
Holding the position, executing rebalances and the exit via NEAR Intents come next.

## Try it

```bash
cargo test                      # 16 unit tests: policy refusals, rate limit, Zcash address rules
cargo near build non-reproducible-wasm --no-abi --manifest-path contracts/vault/Cargo.toml
```

- `contracts/vault`  the Guardian vault (`policy.rs` rules, `zcash.rs` exit-address validation)
- `contracts/probe`  day-1 test: does Rhea DCL treat a contract caller like a user? See DAY1.md

Built for NEAR HACKATHON (1–14 Nov 2026). License: MIT.
