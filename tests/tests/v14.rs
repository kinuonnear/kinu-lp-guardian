//! v1.3 -> v1.4 through the owner and the delay (the way every upgrade goes once the contract key
//! is gone), with a user inside, on the real Rhea DCL 2.4.0; then take profit on real prices:
//! a target reached for one moment is refused, a target held for minutes lets the agent close the
//! position (reason "take_profit"), the target goes with the position, and the user takes
//! everything out. Both builds are the short-delay test builds (10-minute admin delay).
//! Run ./scripts/build.sh first.

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
                let reason = flat.find("\"reason\":\"").map(|i| flat[i + 10..].split('"').next().unwrap_or("").to_string());
                if r.is_success() && errs.is_empty() { reason.map(|x| format!("ok ({x})")).unwrap_or_else(|| "ok".into()) } else { format!("FAIL: {}", errs.join(" | ")) }
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
async fn v13_to_v14_then_take_profit_on_real_prices() -> testresult::TestResult<()> {
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

    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy("dcl.sandbox", fixture("dcl-2.4.0.wasm"), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    t.call("owner.sandbox", "dcl.sandbox", "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await;
    for who in ["dcl.sandbox", "lp.sandbox", "alice.sandbox"] {
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

    // v1.3 (the test build of commit efc29a8) with Alice inside: an open range, the agent named.
    deploy(G, fixture("shared-v1.3-short.wasm"), json!({ "dcl": "dcl.sandbox", "wnear": WRAP, "owner": "dao.sandbox" })).await;
    t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await;
    for tok in [TKQ, WRAP] {
        t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), NearToken::from_millinear(125)).await;
    }
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "alice.sandbox", "amount": "2000000000000000" }), yocto).await;
    t.call("alice.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(4)).await;
    // No exit rule and a wide buffer: take profit is the only reason the agent may close.
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 20000 });
    expect("alice joins v1.3", t.call("alice.sandbox", G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), NearToken::from_millinear(10)).await, "ok");
    expect("alice deposit TKQ", t.call("alice.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "100000000000", "msg": "" }), yocto).await, "ok");
    let open = json!({ "left_point": 303400, "right_point": 306200 }).to_string();
    expect("alice deposit wNEAR + open", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "2000000000000000000000000", "msg": open }), yocto).await, "ok");
    expect("v1.3 has no take profit", t.call("alice.sandbox", G, "set_take_profit", json!({ "point": 306000 }), zero).await, "FAIL");
    let alice_v13 = t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await;

    // v1.3 -> v1.4: stage, the DAO proposes, the delay passes, a stranger runs it.
    let stage = Contract(G.parse()?).call_function_raw("stage_code", built_short()).transaction().gas(NearGas::from_tgas(300)).deposit(NearToken::from_near(5)).with_signer("lp.sandbox".parse()?, signer.clone()).send_to(&net).await?;
    assert!(stage.is_success(), "stage: {stage:?}");
    let hash = t.view(G, "get_config", json!({})).await["staged"]["code_hash"].as_str().unwrap().to_string();
    expect("dao proposes v1.4", t.call("dao.sandbox", G, "propose", json!({ "action": { "Upgrade": { "code_hash": hash } } }), zero).await, "ok");
    wait_secs(&sandbox, 630).await;
    expect("a stranger runs the upgrade", t.call("stranger.sandbox", G, "execute", json!({ "id": 0 }), zero).await, "ok");
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("{:<58} version {} owner {} users {}", "after the upgrade", cfg["version"], cfg["owner"], cfg["users"]);
    assert_eq!(cfg["version"], "1.4.0");
    assert_eq!(cfg["owner"], "dao.sandbox", "owner kept");
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await, alice_v13, "alice's record kept through the upgrade");

    // Take profit on real prices: Alice sets a target 1.5% above the price.
    let start = t.point().await;
    let target = start + 150;
    expect("a stranger sets Alice's target", t.call("stranger.sandbox", G, "set_take_profit", json!({ "point": target }), zero).await, "has not joined");
    expect("alice sets take profit", t.call("alice.sandbox", G, "set_take_profit", json!({ "point": target }), zero).await, "ok");
    assert_eq!(t.view(G, "get_take_profit", json!({ "user": "alice.sandbox" })).await, json!(target));
    expect("agent exits before the target", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "REFUSED");
    // A whale pushes the price past the target for a moment, the agent pokes there, arbitrage undoes it.
    let lp_tkq = t.ft(TKQ, "lp.sandbox").await;
    let buy = json!({ "Swap": { "pool_ids": [POOL], "output_token": TKQ, "min_output_amount": "0" } }).to_string();
    expect("whale buys: price pushed past the target", t.call("lp.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "1000000000000000000000000", "msg": buy }), yocto).await, "ok");
    let pushed = t.point().await;
    println!("{:<58} {start} -> {pushed} (target {target})", "pool point");
    assert!(pushed >= target);
    expect("agent exits on the pushed price at once", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "3 to 60 minutes");
    expect("agent pokes at the pushed price", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "ok");
    let got = t.ft(TKQ, "lp.sandbox").await - lp_tkq;
    let sell = json!({ "Swap": { "pool_ids": [POOL], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    expect("arbitrage sells back", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": got.to_string(), "msg": sell }), yocto).await, "ok");
    println!("{:<58} {}", "pool point after arbitrage", t.point().await);
    wait_secs(&sandbox, 240).await;
    expect("agent exits 4 min later (price back below)", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "REFUSED");
    assert!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await["position"].is_object(), "still open");
    // The price really rises and stays: poke, wait, the agent takes profit.
    expect("the price really rises", t.call("lp.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "1000000000000000000000000", "msg": buy }), yocto).await, "ok");
    println!("{:<58} {}", "pool point", t.point().await);
    expect("agent pokes", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "ok");
    wait_secs(&sandbox, 240).await;
    expect("agent takes profit", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "ok (take_profit)");
    let a = t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await;
    assert!(a["position"].is_null(), "closed");
    assert!(t.view(G, "get_take_profit", json!({ "user": "alice.sandbox" })).await.is_null(), "the target went with the position");
    expect("agent cannot re-open", t.call("agent.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": 303400, "right_point": 306200 } }), zero).await, "only the user opens one");

    // Alice takes everything out; nothing is left on Rhea.
    let tkq0 = t.ft(TKQ, "alice.sandbox").await;
    expect("alice cash_out(leave)", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true }), zero).await, "ok");
    println!("{:<58} +{} TKQ", "alice received", t.ft(TKQ, "alice.sandbox").await - tkq0);
    let rhea = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": G })).await;
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"), "nothing left on Rhea: {rhea}");
    assert_eq!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": G })).await, json!([]));
    Ok(())
}
