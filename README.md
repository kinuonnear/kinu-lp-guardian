# KINU LP Guardian

An agent keeps your Rhea DCL (concentrated liquidity) position on NEAR in range, without your keys,
and only within rules you set on-chain. You put in one token; one shared contract opens the position
on your record; an agent running in a TEE moves it back when the price leaves the range. You can pause
it, change the rules, or take everything back in one approval at any time.

App: [kinudefi.com/guardian](https://kinudefi.com/guardian) · Contract: `lp.kinuonnear.near` ·
Guide: [kinudefi.com/docs/guardian](https://kinudefi.com/docs/guardian)

## Why the agent cannot hurt you

The agent can only *ask*. The contract checks every request against your policy and against the
pool it reads itself, and it pays out only to your own account.

| Guard | Where |
| --- | --- |
| The agent may call only `poke`, `request_rebalance` and `exit`; its key is restricted to those three methods on this contract (`restrict_key`), so a transfer or another contract is refused by the protocol | `agent/src/lib.rs`, `tests/tests/agent_key.rs` |
| Allowed pools, range width, distance from the live price, actions per window, slippage: checked by the contract on every move | `contracts/guardian/src/policy.rs` |
| A move or an exit needs the rule to hold now **and** at a price the contract recorded itself 3–60 minutes earlier, so one pushed price cannot trigger it | `check_seen_age`, `tests/tests/attacks.rs` |
| Exit rule (a % fall from the best price) and take profit (a price target with its own direction) are the user's; the target is cleared with the position it was set for | `set_policy`, `set_take_profit`, `tests/tests/v14.rs` |
| Agent key derived inside an OutLayer TEE and bound to the exact build; the agent acts only on the contract code hash it pins | `agent/`, `agent/manifest.json` |
| Admin actions (code upgrade, recovering a stranded position or credit, a new owner) wait 48 hours in public (`get_pending`); the owner can pause new starts at once, never the ways out | `propose` / `execute`, `tests/tests/migrate.rs` |
| Every callback has a gas floor for the whole chain after it: a position is never left half-recorded, from 300 down to 100 TGas | `tests/tests/gas_sweep.rs` |

Ways out need nobody: `cash_out`, `withdraw` and `claim` work from any wallet or the CLI, also while
the contract is paused. A payout your wallet refuses is kept as `owed` until you `claim` it.

## Verify what runs on mainnet

The contract is built reproducibly (cargo-near, Docker image pinned by digest) by
[GitHub Actions](.github/workflows/build.yml); the log prints the SHA-256 that must equal the
account's code hash.

```bash
# the code hash on chain
curl -s https://rpc.mainnet.near.org -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"view_account","finality":"final","account_id":"lp.kinuonnear.near"}}' \
  | grep -o '"code_hash":"[^"]*"'

# the same build yourself (needs Docker and cargo-near 0.21.1)
cargo near build reproducible-wasm --manifest-path contracts/guardian/Cargo.toml
```

The wasm also carries NEP-330 metadata (`contract_source_metadata`): the commit it was built from and
the build image. `get_config` returns the version, owner, admin delay and pause state.

## Tests

```bash
cargo test -p kinu-guardian-shared --lib          # contract unit tests
(cd agent && cargo test --lib)                    # agent unit tests
python3 scripts/fetch_fixtures.py                 # mainnet Rhea DCL 2.4.0 + wrap.near wasm
./scripts/build.sh                                # contract builds the sandbox tests deploy
cargo test --test shared --test migrate --test attacks --test agent_key --test gas_sweep --test v14
```

The sandbox tests run on a local NEAR node against the real Rhea DCL code from mainnet. If the
sandbox can not download its binary, set `NEAR_SANDBOX_BIN_PATH`.

## Layout

- `contracts/guardian`: the shared Guardian contract (`lib.rs`; rules in `policy.rs`).
- `agent`: the OutLayer agent (Rust, `wasm32-wasip2`): reads the chain, decides, signs in the TEE.
- `tests/tests`: sandbox tests; `tests/fixtures`: earlier contract builds and mainnet wasm.
- `contracts/vault`, `factory`, `probe`, `cleanup`, `mockdcl`, `testtoken`, `DAY1.md`, `agent/RUNS.md`:
  the first design, one vault contract per user, retired on 29 Oct 2026 and kept for the record.

Built for NEAR HACKATHON 2026. License: MIT.
