//! Attacks on the shared Guardian v1.3, on the real Rhea DCL 2.4.0 (mainnet wasm):
//!   A. a compromised agent: another pool, payouts, an exit the user never allowed, churning an
//!      earning range: every one refused;
//!   B. a pushed price (M1): a whale swap moves the pool, the agent acts at once, pokes at the pushed
//!      price, the price comes back: refused each time; only a price held for minutes counts, and
//!      then only within the user's policy;
//!   C. admin: strangers cannot propose or pause; staged code with a pending upgrade cannot be
//!      swapped; an upgrade whose `migrate` fails is undone whole: the contract keeps its code,
//!      records and funds, and the user still cashes out in full.
//! Uses the short-delay build (10-minute admin delay). Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const POOL: &str = "tkq.sandbox|wrap.sandbox|10000";
const TKQ: &str = "tkq.sandbox";
const WRAP: &str = "wrap.sandbox";
const G: &str = "guardian.sandbox";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn built(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}
fn built_short() -> Vec<u8> {
    let path = format!("{}/../target/near/kinu_guardian_shared_short/kinu_guardian_shared.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    /// "ok", "REFUSED: why" (a refusal inside a callback: the step ends, the transaction succeeds),
    /// or "FAIL: messages" (a panic, inner receipts included).
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        let out = Contract(to.parse().unwrap()).call_function(method, args).transaction().gas(NearGas::from_tgas(300)).deposit(deposit).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let mut errs: Vec<String> = Vec::new();
                for marker in ["ExecutionError(\"", "Smart contract panicked: "] {
                    let mut rest = dbg.as_str();
                    while let Some(i) = rest.find(marker) {
                        let tail = &rest[i + marker.len()..];
                        let end = tail.find('"').unwrap_or(tail.len().min(160));
                        let m = tail[..end].to_string();
                        if !errs.contains(&m) {
                            errs.push(m);
                        }
                        rest = &tail[end..];
                    }
                }
                let flat = dbg.replace('\\', "");
                if let Some(i) = flat.find("\"event\":\"refused\"") {
                    let tail = &flat[i..];
                    let why = tail.find("\"why\":\"").map(|w| tail[w + 7..].split('"').next().unwrap_or("").to_string()).unwrap_or_default();
                    return format!("REFUSED: {why}");
                }
                if r.is_success() && errs.is_empty() { "ok".into() } else { format!("FAIL: {}", errs.join(" | ")) }
            }
            Err(e) => format!("ERR {e}").chars().take(300).collect(),
        }
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn ft(&self, token: &str, id: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": id })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn near(&self, id: &str) -> f64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.amount.as_yoctonear() as f64 / 1e24).unwrap_or(0.0)
    }
    async fn point(&self) -> i64 {
        self.view("dcl.sandbox", "get_pool", json!({ "pool_id": POOL })).await["current_point"].as_i64().unwrap()
    }
}

fn expect(label: &str, r: String, with: &str) {
    println!("{label:<58} {r}");
    assert!(r.contains(with), "{label}: expected '{with}', got {r}");
}

async fn chain_secs(rpc: &str) -> u64 {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "block", "params": { "finality": "final" } });
    let b: Value = reqwest::Client::new().post(rpc).json(&body).send().await.unwrap().json().await.unwrap();
    b["result"]["header"]["timestamp"].as_u64().unwrap() / 1_000_000_000
}

async fn wait_secs(sandbox: &near_sandbox::Sandbox, secs: u64) {
    let start = chain_secs(&sandbox.rpc_addr).await;
    while chain_secs(&sandbox.rpc_addr).await < start + secs {
        sandbox.fast_forward(200).await.unwrap();
    }
    println!("{:<58} +{} s", "(sandbox clock)", chain_secs(&sandbox.rpc_addr).await - start);
}

#[tokio::test]
async fn attacks_are_refused() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "dao", "stranger"] {
        sandbox.create_account(format!("{name}.sandbox").parse::<AccountId>()?).initial_balance(NearToken::from_near(100)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let (yocto, zero) = (NearToken::from_yoctonear(1), NearToken::from_near(0));

    // Rhea as on mainnet: real DCL and wrap.near, a 12-decimal token, one 1% pool with deep liquidity.
    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy("dcl.sandbox", kinu_guardian_tests::dcl_wasm(), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    t.call("owner.sandbox", "dcl.sandbox", "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await;
    for who in ["dcl.sandbox", "lp.sandbox", "alice.sandbox", "stranger.sandbox"] {
        t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
        t.call("owner.sandbox", WRAP, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
    }
    expect("create_pool", t.call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 10000, "init_point": 304800 }), NearToken::from_near(1)).await, "ok");
    t.call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await;
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "100000000000000000" }), yocto).await;
    t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(60)).await;
    for (tok, amt) in [(TKQ, "50000000000000000"), (WRAP, "30000000000000000000000000")] {
        t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), yocto).await;
    }
    expect("lp wide liquidity", t.call("lp.sandbox", "dcl.sandbox", "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": "20000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0" }), zero).await, "ok");

    // The Guardian (owner: the DAO) and Alice, who names the agent, with a range around the price.
    deploy(G, built_short(), json!({ "dcl": "dcl.sandbox", "wnear": WRAP, "owner": "dao.sandbox" })).await;
    t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await;
    for tok in [TKQ, WRAP] {
        t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), NearToken::from_millinear(125)).await;
    }
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "alice.sandbox", "amount": "2000000000000000" }), yocto).await;
    t.call("alice.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(4)).await;
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    expect("alice join (agent: agent.sandbox)", t.call("alice.sandbox", G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), NearToken::from_millinear(10)).await, "ok");
    expect("alice deposit TKQ", t.call("alice.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "100000000000", "msg": "" }), yocto).await, "ok");
    expect("alice deposit wNEAR", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "2000000000000000000000000", "msg": "" }), yocto).await, "ok");
    expect("alice opens around the price", t.call("alice.sandbox", G, "open", json!({ "left_point": 303400, "right_point": 306200 }), zero).await, "ok");
    let alice_before = t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await;
    assert!(alice_before["position"].is_object());

    // ---------- A. a compromised agent ----------
    println!("--- A. compromised agent");
    let other_pool = json!({ "user": "alice.sandbox", "request": { "pool_id": "tkq.sandbox|wrap.sandbox|2000", "left_point": 303400, "right_point": 306200 } });
    expect("agent: move Alice into another pool", t.call("agent.sandbox", G, "request_rebalance", other_pool, zero).await, "not in the owner's allowed list");
    expect("agent: withdraw (only ever its own record)", t.call("agent.sandbox", G, "withdraw", json!({ "token": WRAP }), zero).await, "has not joined");
    expect("agent: cash_out (only ever its own record)", t.call("agent.sandbox", G, "cash_out", json!({ "leave": false }), zero).await, "has not joined");
    expect("agent: exit Alice (she allowed no exit)", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "has not allowed the agent to exit");
    let churn = json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": 303600, "right_point": 306400 } });
    expect("agent: churn a range that is earning", t.call("agent.sandbox", G, "request_rebalance", churn, zero).await, "not far enough outside");
    expect("stranger: set Alice's agent", t.call("stranger.sandbox", G, "set_agent", json!({ "agent": "stranger.sandbox" }), zero).await, "has not joined");
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await, alice_before, "alice untouched by every attempt");

    // ---------- B. a pushed price (M1) ----------
    println!("--- B. pushed price");
    let start = t.point().await;
    let lp_wnear = t.ft(WRAP, "lp.sandbox").await;
    let sell = json!({ "Swap": { "pool_ids": [POOL], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    expect("whale sells TKQ: price pushed down", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "2000000000000", "msg": sell }), yocto).await, "ok");
    let pushed = t.point().await;
    println!("{:<58} {start} -> {pushed}", "pool point");
    assert!(pushed < 303400 - 200, "pushed below Alice's range: {pushed}");
    let left = (pushed + 20).div_euclid(200) * 200 + 200;
    let at_pushed = json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": left, "right_point": left + 1400 } });
    expect("agent acts on the pushed price at once", t.call("agent.sandbox", G, "request_rebalance", at_pushed.clone(), zero).await, "3 to 60 minutes");
    expect("agent pokes at the pushed price", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "ok");
    expect("agent acts right after its poke", t.call("agent.sandbox", G, "request_rebalance", at_pushed.clone(), zero).await, "3 to 60 minutes");
    // Arbitrage brings the price back within minutes.
    let got = t.ft(WRAP, "lp.sandbox").await - lp_wnear;
    let buy = json!({ "Swap": { "pool_ids": [POOL], "output_token": TKQ, "min_output_amount": "0" } }).to_string();
    expect("arbitrage buys back", t.call("lp.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": got.to_string(), "msg": buy }), yocto).await, "ok");
    let back = t.point().await;
    println!("{:<58} {back}", "pool point after arbitrage");
    assert!((303400..306200).contains(&back), "back inside Alice's range: {back}");
    wait_secs(&sandbox, 200).await;
    expect("agent acts 3+ min later on the old pushed read", t.call("agent.sandbox", G, "request_rebalance", at_pushed.clone(), zero).await, "REFUSED"); // the pushed range is now far from the real price
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await["position"], alice_before["position"], "alice's range unchanged");
    // A price held down for minutes is a real price: the agent may then move Alice, but only by her
    // policy (width, distance, actions per day), never to another pool or to itself.
    expect("whale sells again and holds", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "2000000000000", "msg": sell }), yocto).await, "ok");
    let held = t.point().await;
    let left = (held + 20).div_euclid(200) * 200 + 200;
    let wide = json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": left, "right_point": left + 8000 } });
    expect("agent pokes", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "ok");
    wait_secs(&sandbox, 200).await;
    expect("agent: a range wider than Alice allows", t.call("agent.sandbox", G, "request_rebalance", wide, zero).await, "wider than the owner's maximum");
    let fair = json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": left, "right_point": left + 1400 } });
    expect("agent pokes again", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "ok");
    wait_secs(&sandbox, 200).await;
    expect("agent: within her policy after 3+ min held", t.call("agent.sandbox", G, "request_rebalance", fair, zero).await, "ok");

    // ---------- C. admin ----------
    println!("--- C. admin");
    expect("stranger proposes", t.call("stranger.sandbox", G, "propose", json!({ "action": { "SetOwner": { "owner": "stranger.sandbox" } } }), zero).await, "Only the owner");
    expect("stranger pauses", t.call("stranger.sandbox", G, "set_paused", json!({ "paused": true }), zero).await, "Only the owner");
    expect("stranger cancels", t.call("stranger.sandbox", G, "cancel", json!({ "id": 0 }), zero).await, "Only the owner");
    expect("execute a proposal that does not exist", t.call("stranger.sandbox", G, "execute", json!({ "id": 7 }), zero).await, "No such proposal");
    // Code whose migrate cannot read this state (v1.2 expects v1.1 records): staged, proposed, then
    // a stranger tries to swap the staged code under the pending upgrade.
    let bad = Contract(G.parse()?).call_function_raw("stage_code", fixture("shared-v1.2.wasm")).transaction().gas(NearGas::from_tgas(300)).deposit(NearToken::from_near(5)).with_signer("stranger.sandbox".parse()?, signer.clone()).send_to(&net).await?;
    assert!(bad.is_success(), "stage: {bad:?}");
    let hash = t.view(G, "get_config", json!({})).await["staged"]["code_hash"].as_str().unwrap().to_string();
    expect("dao proposes that upgrade", t.call("dao.sandbox", G, "propose", json!({ "action": { "Upgrade": { "code_hash": hash } } }), zero).await, "ok");
    let swap = Contract(G.parse()?).call_function_raw("stage_code", built_short()).transaction().gas(NearGas::from_tgas(300)).deposit(NearToken::from_near(5)).with_signer("lp.sandbox".parse()?, signer.clone()).send_to(&net).await?;
    let swap_dbg = format!("{swap:?}");
    println!("{:<58} {}", "someone swaps the staged code under it", if swap.is_success() { "ok (!)" } else { "refused" });
    assert!(!swap.is_success() && swap_dbg.contains("pending upgrade"), "staged code is locked while proposed");
    wait_secs(&sandbox, 630).await;
    let stranger_near = t.near("stranger.sandbox").await;
    let r = t.call("stranger.sandbox", G, "execute", json!({ "id": 0 }), zero).await;
    println!("{:<58} {r}", "execute the upgrade whose migrate fails");
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("{:<58} version {} staged {} pending {}", "contract after", cfg["version"], cfg["staged"], cfg["pending"]);
    assert_eq!(cfg["version"], "1.5.0", "the failed deploy was undone: still the current code");
    assert!(t.near("stranger.sandbox").await - stranger_near > 4.9, "the stager's storage deposit came back");
    let alice_now = t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await;
    assert!(alice_now["position"].is_object(), "alice's record intact");

    // Alice still takes everything out; the contract ends with nothing on Rhea.
    let (near0, tkq0) = (t.near("alice.sandbox").await, t.ft(TKQ, "alice.sandbox").await);
    expect("alice cash_out(leave) after all of it", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true }), zero).await, "ok");
    println!("{:<58} +{:.4} NEAR, +{} TKQ", "alice received", t.near("alice.sandbox").await - near0, t.ft(TKQ, "alice.sandbox").await - tkq0);
    assert!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await.is_null());
    let rhea = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": G })).await;
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"), "nothing left on Rhea: {rhea}");
    assert_eq!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": G })).await, json!([]));
    Ok(())
}
