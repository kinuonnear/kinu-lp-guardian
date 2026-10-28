# KINU LP Guardian

An agent may manage a Rhea DCL (concentrated liquidity) position on NEAR, but only inside rules the
owner sets on-chain. The vault checks every argument the agent sends (pool, range width, distance from
the live price, rate limit) against the owner's policy and the pool it reads itself. A compromised agent
gets refused on-chain. The owner can freeze, revoke the agent, and register a **shielded Zcash exit
address** (unified address with an Orchard receiver) for a "bunker exit".

Status (28 Oct 2026):
- The vault holds the Rhea DCL position itself; owner funding goes straight into its DCL balance.
- Agent rebalance: the vault reads the pool, checks pool / width / distance / rate, closes the old
  position and opens the new one with minimum amounts it computes itself.
- Exit: the owner any time; the agent only when the owner's rule is met (price fell `exit_drop_points`
  below the highest point the vault itself saw, 2877 points = -25%). Exited funds stay in the vault;
  only the owner sweeps them.
- Freeze, revoke, Orchard-only Zcash exit address (ZIP 316: bech32m + F4Jumble).
- Proven: 3 USDC → 0.002023 ZEC through NEAR Intents to a Zodl (formerly Zashi) unified address with
  Sapling + Orchard receivers only (28 Oct, mainnet).
- **Bunker exit** (`bunker_exit`): the vault pays a NEAR Intents 1Click deposit address only for a quote
  1Click signed (ed25519 checked on-chain with 1Click's manager key) whose recipient is the owner's Orchard
  address, paying native ZEC, refunding to the vault, still live, and never used before. The agent may
  call it only after an exit, so a crash can end in the owner's shielded pool without the owner online;
  a deposit address swapped in by a compromised agent fails the signature check.
  `tools/bunker-quote.mjs` fetches a live quote and prints the two arguments.
- Mainnet proofs in DAY1.md (28 Oct): a contract opened and closed a Rhea DCL position; the vault ran a
  Bunker exit: 3 USDC -> 0.00203427 ZEC into a shielded Zodl address (Zcash tx 362d709d...).
- **Agent in a TEE** (agent/): runs on OutLayer, key bound to its exact build; on mainnet it opened a
  Rhea position and later ran the Bunker exit to shielded ZEC, every step signed in the TEE (agent/RUNS.md).
- Next: exact in-range minimum amounts (CLMM math); KINU page for owners.

## Try it

```bash
./scripts/test.sh   # builds all contracts, 28 unit tests + 1 end-to-end sandbox test
```

The sandbox test (tests/tests/flow.rs) runs the whole story on a local NEAR node: the owner funds the
vault; a compromised agent tries another pool, an absurd range, a range far from the price, changing the
exit address, and a stranger tries to act — all refused on-chain; the honest rebalance opens a position
owned by the vault; the agent's early exit is refused; after a 25% drop the exit goes through; only the
owner sweeps the funds; a revoked agent is refused. Rhea DCL is replaced by contracts/mockdcl (same
methods and argument shapes) because the DCL testnet deposit path is broken (see DAY1.md).

- `contracts/vault`  the Guardian vault (`policy.rs` rules, `zcash.rs` exit-address validation)
- `contracts/probe`  day-1 test: does Rhea DCL treat a contract caller like a user? See DAY1.md
- `contracts/mockdcl`  test-only Rhea DCL stand-in; `contracts/testtoken` testnet/sandbox NEP-141 token

Built for NEAR HACKATHON (1–14 Nov 2026). License: MIT.
