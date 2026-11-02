//! Can a contract stand as a Guardian user? Real Rhea wasm: join, deposit, open, agent, freeze, cash out.

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

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    /// "ok (burnt)" or the contract messages of a failed call, inner receipts included.
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        let out = Contract(to.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(300))
            .deposit(deposit)
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await;
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
                let burnt = dbg.split("total_gas_burnt: NearGas { inner: ").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1e12;
                // A refusal inside a callback is an event, not a failure: the step ends, the user is unlocked.
                let flat = dbg.replace('\\', "");
                if let Some(i) = flat.find("\"event\":\"refused\"") {
                    let tail = &flat[i..];
                    let why = tail.find("\"why\":\"").map(|w| tail[w + 7..].split('"').next().unwrap_or("").to_string()).unwrap_or_default();
                    return format!("REFUSED ({burnt:.1}T): {why}");
                }
                if r.is_success() && errs.is_empty() { format!("ok ({burnt:.1}T)") } else { format!("FAIL ({burnt:.1}T): {}", errs.join(" | ")) }
            }
            Err(e) => format!("ERR {e}").chars().take(300).collect(),
        }
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn storage(&self, id: &str) -> u64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.storage_usage).unwrap_or(0)
    }
    async fn near(&self, id: &str) -> f64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.amount.as_yoctonear() as f64 / 1e24).unwrap_or(0.0)
    }
    async fn ft(&self, token: &str, id: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": id })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn ledger(&self, user: &str) -> (Option<Value>, Vec<(String, u128)>) {
        let a = self.view(G, "get_account", json!({ "user": user })).await;
        let ledger = a["ledger"].as_array().cloned().unwrap_or_default().iter().map(|e| (e[0].as_str().unwrap().to_string(), e[1].as_str().unwrap().parse().unwrap())).collect();
        (if a["position"].is_object() { Some(a["position"].clone()) } else { None }, ledger)
    }
}

fn ok(label: &str, r: String) {
    println!("{label:<42} {r}");
    assert!(r.starts_with("ok ("), "{label}: {r}");
}

/// The latest final block's time, seconds.
async fn chain_secs(rpc: &str) -> u64 {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "block", "params": { "finality": "final" } });
    let b: Value = reqwest::Client::new().post(rpc).json(&body).send().await.unwrap().json().await.unwrap();
    b["result"]["header"]["timestamp"].as_u64().unwrap() / 1_000_000_000
}

/// Moves the sandbox clock forward by at least `secs` (blocks are skipped, not waited for).
async fn wait_secs(sandbox: &near_sandbox::Sandbox, _t: &T, secs: u64) {
    let start = chain_secs(&sandbox.rpc_addr).await;
    while chain_secs(&sandbox.rpc_addr).await < start + secs {
        sandbox.fast_forward(200).await.unwrap();
    }
    println!("{:<42} +{} s", "(sandbox clock)", chain_secs(&sandbox.rpc_addr).await - start);
}
fn fails(label: &str, r: String, with: &str) {
    println!("{label:<42} {r}");
    assert!(r.contains(with), "{label}: expected '{with}', got {r}");
}

#[tokio::test]
async fn a_contract_can_be_the_guardian_user() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "bob", "wall"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(100)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let yocto = NearToken::from_yoctonear(1);
    let zero = NearToken::from_near(0);

    // Rhea as on mainnet: the real DCL and wrap.near, a 12-decimal token, whitelisted, one 1% pool.
    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy("dcl.sandbox", kinu_guardian_tests::dcl_wasm(), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    t.call("owner.sandbox", "dcl.sandbox", "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await;
    for who in ["dcl.sandbox", "lp.sandbox", "alice.sandbox", "bob.sandbox"] {
        t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
        t.call("owner.sandbox", WRAP, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
    }
    ok("create_pool", t.call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 10000, "init_point": 304800 }), NearToken::from_near(1)).await);
    // Deep liquidity from another LP so swaps can move the price.
    t.call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await;
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "100000000000000000" }), yocto).await;
    t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(40)).await;
    for (tok, amt) in [(TKQ, "50000000000000000"), (WRAP, "30000000000000000000000000")] {
        t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), yocto).await;
    }
    ok("lp wide liquidity", t.call("lp.sandbox", "dcl.sandbox", "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": "20000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0" }), zero).await);

    // The shared Guardian: KINU deploys it and pays its Rhea / token storage once.
    deploy(G, built("kinu_guardian_shared"), json!({ "dcl": "dcl.sandbox", "wnear": WRAP, "owner": "owner.sandbox" })).await;
    ok("register_dcl (KINU, once)", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    ok("register_token TKQ (once)", t.call("owner.sandbox", G, "register_token", json!({ "token": TKQ }), NearToken::from_millinear(125)).await);
    ok("register_token wNEAR (once)", t.call("owner.sandbox", G, "register_token", json!({ "token": WRAP }), NearToken::from_millinear(125)).await);

    // The wall is a contract; Alice is only its owner. It holds the tokens and is the Guardian user.
    const W: &str = "wall.sandbox";
    deploy(W, built("kinu_guardian_wallprobe"), json!({ "owner": "alice.sandbox" })).await;
    for tok in [TKQ, WRAP] {
        t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": W, "registration_only": true }), NearToken::from_millinear(125)).await;
    }
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": W, "amount": "2000000000000000" }), yocto).await;
    t.call("lp.sandbox", WRAP, "ft_transfer", json!({ "receiver_id": W, "amount": "3000000000000000000000000" }), yocto).await;
    let via = |to: &str, method: &str, args: Value, deposit: NearToken| {
        let t = &t;
        let body = json!({ "receiver": to, "method": method, "args": args.to_string(), "deposit": deposit.as_yoctonear().to_string() });
        async move { t.call("alice.sandbox", W, "forward", body, NearToken::from_near(0)).await }
    };
    fails("a stranger cannot use the wall", t.call("bob.sandbox", W, "forward", json!({ "receiver": G, "method": "cash_out", "args": "{}", "deposit": "0" }), zero).await, "Only the owner");

    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    ok("wall join", via(G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), NearToken::from_millinear(10)).await);
    ok("wall deposit TKQ", via(TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "100000000000", "msg": "" }), yocto).await);
    ok("wall deposit wNEAR", via(WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "2000000000000000000000000", "msg": "" }), yocto).await);
    ok("wall opens", via(G, "open", json!({ "left_point": 303400, "right_point": 306200 }), zero).await);
    let (pos, led) = t.ledger(W).await;
    println!("wall: position {} ledger {:?}", pos.as_ref().map(|p| p["lpt_id"].to_string()).unwrap_or("none".into()), led);
    assert!(pos.is_some(), "the wall holds a position");

    // The agent serves the wall as it serves a wallet.
    let sell = json!({ "Swap": { "pool_ids": [POOL], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    ok("swap TKQ -> wNEAR", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "2000000000000", "msg": sell }), yocto).await);
    let point = t.view("dcl.sandbox", "get_pool", json!({ "pool_id": POOL })).await["current_point"].as_i64().unwrap();
    let left = (point + 20).div_euclid(200) * 200 + 200;
    let move_it = json!({ "user": W, "request": { "pool_id": POOL, "left_point": left, "right_point": left + 1400 } });
    ok("agent pokes the wall", t.call("agent.sandbox", G, "poke", json!({ "user": W }), zero).await);
    wait_secs(&sandbox, &t, 200).await;

    // The tripwire's only move on Guardian: freeze the agent. It needs no Guardian change.
    ok("wall freezes the agent", via(G, "set_frozen", json!({ "frozen": true }), zero).await);
    let refused = t.call("agent.sandbox", G, "request_rebalance", move_it.clone(), zero).await;
    println!("{:<42} {refused}", "frozen agent tries to move");
    assert!(!refused.starts_with("ok ("), "a frozen agent must not move: {refused}");
    ok("wall unfreezes the agent", via(G, "set_frozen", json!({ "frozen": false }), zero).await);
    ok("agent pokes the wall again", t.call("agent.sandbox", G, "poke", json!({ "user": W }), zero).await);
    wait_secs(&sandbox, &t, 200).await;
    ok("agent rebalances the wall", t.call("agent.sandbox", G, "request_rebalance", move_it, zero).await);

    // The whole way out, through one forward: this is the gas question.
    let (near_before, tkq_before, wrap_before) = (t.near(W).await, t.ft(TKQ, W).await, t.ft(WRAP, W).await);
    let alice_tkq = t.ft(TKQ, "alice.sandbox").await;
    ok("wall cash_out(leave)", via(G, "cash_out", json!({ "leave": true }), zero).await);
    let (got_near, got_tkq, got_wrap) = (t.near(W).await - near_before, t.ft(TKQ, W).await - tkq_before, t.ft(WRAP, W).await - wrap_before);
    println!("the wall got {got_near:.4} NEAR, {got_wrap} wNEAR and {got_tkq} TKQ");
    assert!(t.view(G, "get_account", json!({ "user": W })).await.is_null(), "the wall left Guardian");
    assert!(got_near > 0.0 || got_wrap > 0, "wNEAR side arrived");
    assert!(got_tkq > 0, "TKQ arrived");
    assert_eq!(t.ft(TKQ, "alice.sandbox").await, alice_tkq, "nothing skipped the wall");
    assert_eq!((t.ft(TKQ, G).await, t.ft(WRAP, G).await), (0, 0), "Guardian's wallets empty");
    Ok(())
}
