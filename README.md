# KINU LP Guardian

An agent keeps your Rhea DCL (concentrated liquidity) position on NEAR in range and closes it at the loss
you chose, without your keys, and only within rules you set on chain. You put in one token; one shared
contract opens the position on your record; an agent running in an OutLayer TEE moves it back when the
price leaves the range. You can pause it, change the rules, or take everything back in one approval at
any time, with or without the agent.

App: [kinudefi.com/guardian](https://kinudefi.com/guardian) · Contract: `lp.kinuonnear.near` ·
Guide: [kinudefi.com/docs/guardian](https://kinudefi.com/docs/guardian)

## What runs on mainnet (11 Nov 2026)

| | |
| --- | --- |
| Contract | `lp.kinuonnear.near` v1.8.0, code hash `BrTcgXH9A8U9rtvoGJu3kjZaKjiTvFUs6htt8tMrfNRq` (CI build of `78ea017`) |
| Owner | the Sputnik DAO `kinu-guardian.sputnik-dao.near`: 3 council members, 2 must approve |
| Admin delay | every upgrade, recovery or owner change waits 48 hours in public (`get_pending`) |
| Pauser | `workerxxxx.near` may pause new starts at once; nobody can pause the ways out |
| State | paused since the mainnet tests of 11 Nov; exits, withdrawals and cash out work |
| Agent | OutLayer project `agent.kinuonnear.near/kinu-guardian`, active build `5aaad2670b5b…65dc` (commit `2496b01`), account `a18c4ac6…c810` |
| Rhea | the agent and the page treat DCL 2.4.2 (`3G34Nhjm…`) as the tested build; on any other code the agent holds re-centres and sales, exits still run |

The contract account has no access key: its last one was deleted on 11 Nov 2026
([tx](https://nearblocks.io/txns/JDob5Uba3wbvxaQNjtNyyCstu69VosMXuFT9me7y5Agc)). The code can change only by
the DAO's vote and then the 48 hours: anyone may stage a build (with a deposit for its storage), only the
DAO can propose it, and anyone can run it once the wait is over.

## Check it yourself

Each line is something to run, and what you should see.

| Run | You see |
| --- | --- |
| `./scripts/verify.sh` (about 2 min, needs curl and python3; cargo for the unit tests) | `PASS` for the code hash, the 48 h delay, the DAO owner, the contract account having no key, the agent's limited key, the Rhea build, the TEE check and the 18 refusals; `ALL CHECKS PASSED` |
| `python3 scripts/check-agent-tee.py` | 8 `PASS` lines: the attested run executed the active agent wasm, through the agent's project; input and output hash to the attested values; it returned the agent's account, whose only key it is; the five TDX measurements are approved on `worker.outlayer.near`; Intel's signature, approved build and binding (job 1353205) |
| `outlayer-verify bundle demo/tee-proof-5aaad267.json --offline` | `PROVEN`: the same proof with no network at all ([outlayer-verify](https://github.com/out-layer/outlayer-verify)) |
| [app.outlayer.ai/attestation/1353205](https://app.outlayer.ai/attestation/1353205?network=mainnet), press Verify | Authenticity, Identity and Binding pass; executed binary `5aaad267…` |
| `./scripts/reproduce-agent.sh 2496b01 5aaad2670b5b364cdd629bb642ddfa39edb87b039c046164929faa10258165dc` (Docker) | `PASS`: the agent wasm rebuilt from source equals the one OutLayer attests |
| `cargo near build reproducible-wasm --manifest-path contracts/guardian/Cargo.toml` at `78ea017` | the SHA-256 whose base58 is the code hash above; [the CI log](.github/workflows/build.yml) prints the same |
| [`demo/mainnet-run.md`](demo/mainnet-run.md) | a stolen agent key on mainnet: 18 attempts, 18 refused, each with its transaction; the victim's record byte for byte the same |
| [the agent's first re-centre on v1.8](https://nearblocks.io/txns/FNTG9ZZTPvp7fmYScwBrqKt32E6Gx3PSvefaWc2cEqjt) | `rebalance_approved`, the old position closed with its fees, a new one opened beside the price |

## Why the agent cannot take your funds

The agent can only *ask*. The contract checks every request against your policy and against the pool it
reads itself, and it pays out only to the record's own wallet.

| Guard | Where |
| --- | --- |
| The agent's key may call only `poke`, `request_rebalance` and `exit` on this contract; the protocol refuses anything else, transfers included | `agent/src/lib.rs`, `tests/tests/agent_key.rs` |
| Allowed pools, range width, distance from the live price, actions per window and slippage are checked on every move | `contracts/guardian/src/policy.rs` |
| A move, an exit or a sale needs its rule to hold now **and** at a price the contract read itself 3 to 60 minutes earlier, so one pushed price cannot trigger it | `get_seen`, `tests/tests/attacks.rs` |
| Changing the rules, naming an agent or dropping a sale needs the user's own one-yocto signature: an agent key, which cannot attach a deposit, is refused | `set_policy`, `set_agent`, `cancel_sale`; refused on mainnet in `demo/mainnet-run.md` |
| A pool can be joined only when both tokens are listed, or come from a launchpad or bridge the owner trusts, and never names one token twice | `join`, `list_tokens`, `list_factories`, `tests/tests/double_pay.rs`; listing by a stranger refused in `demo/mainnet-run.md` |
| After a cut loss the falling token is sold only at a price the contract confirmed, and the user can keep the tokens instead | `get_sale`, `tests/tests/sale.rs`, `tests/tests/sell_spike.rs` |
| Right after an ordinary step, for a few seconds only ways out (exit, sale, cash out, full withdrawal, leave) can take the contract's lock, so nobody can hold users in | `exit_window`, `tests/tests/v18.rs` |
| Deposits and payouts are measured by the balance before and after; a payout the wallet refuses is kept as `owed` until `claim` | `withdraw`, `claim`, `tests/tests/shared.rs` |
| Every callback has a gas floor for the whole chain after it: a position is never left half recorded | `tests/tests/gas_sweep.rs`, `tests/tests/stage_gas.rs` |
| The agent acts only while the contract runs the code hash it pins and Rhea runs the build it was tested on | `agent/src/main.rs`, `tests/tests/rhea_change.rs` |
| Code changes go through the DAO and then wait 48 hours; the owner and the pauser can stop new starts at once, never the ways out | `propose` / `execute`, `tests/tests/dao_owner.rs`, `tests/tests/brake_key.rs` |

Ways out need nobody: `cash_out`, `withdraw` and `claim` work from any wallet or the CLI, also while the
contract is paused.

## How a position is managed

- **Range.** Automated starts at ±25% around the price. The agent moves it only after the price passes an
  edge by the trigger (5%) and holds there; the new range sits right beside the price. After a fall its top
  stays just under the old one, so a recovery still sells near the prices it left from.
- **SL.** Chosen as a % of your capital: what you put in, each deposit at its own day's price, less what you
  took out. On chain it is a fall from your token's best confirmed price; the app works that point out from
  your capital, and moves it in the same approval when you add funds. Exit closes the position, then the
  falling token is sold for the other one, into your wallet.
- **Take profit.** A price target with its own direction, cleared with the position it was set for.
- **Costs.** A day of fees in the new range must pay for the agent's gas, or it does not move the position.

The formulas, with the numbers they were checked on, are in the app's guide and on the Performance page,
which shows capital, value, fees at today's price, and the result against simply holding.

## What went wrong before

The first design (28 to 29 Oct 2026) gave every user a vault contract of their own. Rhea DCL 2.4.0 needed
more gas to send tokens out of a position than the vault attached, and one test vault, `kinu-1`, could no
longer take its position back: 3.016 NEAR, 1.329 wNEAR and 0.0391 QTC were lost there, KINU's own test
funds. Guardian was stopped the same day and the rest returned.

What changed: one shared contract instead of a vault per user; every suite runs against the real Rhea DCL
code taken from mainnet, not a mock (`scripts/fetch_fixtures.py`, `tests/tests/realdcl.rs`); a gas floor
for every callback chain; deposits and payouts measured, not trusted; the agent and the app hold
re-centres when Rhea's code changes until it is tested (`scripts/check-new-dcl.sh`). The vault contracts
are kept under `contracts/vault`, `factory` and `cleanup` for the record.

## Tests

```bash
cargo test -p kinu-guardian-shared --lib --features legacy-suites   # contract unit tests
(cd agent && cargo test --lib)                                      # agent unit tests
./scripts/verify.sh --full                                          # every sandbox suite, about 90 min
```

The sandbox suites run on a local NEAR node against the real Rhea DCL and wrap.near code from mainnet
(`python3 scripts/fetch_fixtures.py`, then `./scripts/build.sh`). If the sandbox cannot download its
binary, set `NEAR_SANDBOX_BIN_PATH`. Rhea upgraded to DCL 2.4.2 on 5 Nov 2026; every suite passed on it
before the agent and the app accepted it.

## Layout

- `contracts/guardian`: the shared Guardian contract (`lib.rs`; rules in `policy.rs`).
- `agent`: the OutLayer agent (Rust, `wasm32-wasip2`): reads the chain, decides, signs inside the TEE.
- `tests/tests`: sandbox suites; `tests/fixtures`: earlier contract builds and mainnet wasm.
- `scripts`: `verify.sh`, `check-agent-tee.py`, `reproduce-agent.sh`, builds and fixtures.
- `demo`: the mainnet attack run and the TEE proof bundles.
- `contracts/vault`, `factory`, `probe`, `cleanup`, `mockdcl`, `testtoken`, `DAY1.md`, `agent/RUNS.md`: the
  first design and its tools, retired on 29 Oct 2026 and kept for the record.

## Built with

NEAR smart contracts (near-sdk 5.29, reproducible builds with cargo-near 0.21.1), Rhea DCL on NEAR
mainnet, OutLayer TEE agents (Intel TDX, keys bound to the build), Sputnik DAO (v2.3.1 code) as owner, near-api and
near-sandbox for the suites, outlayer-verify for the proof. NEAR Intents and Chain Signatures are not used
by Guardian.

Built for NEAR HACKATHON 2026; this repository starts on 28 Oct 2026. License: MIT.
