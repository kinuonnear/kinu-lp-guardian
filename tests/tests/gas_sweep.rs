//! The 29 Oct 2026 mainnet failure, replayed: a deposit that opens a position, given less and less
//! gas (300 down to 100 TGas), on the contract as deployed (v1.0, Gm53qinU) and on v1.1.
//!
//! After every attempt one rule must hold: every position this contract owns on Rhea is on its
//! owner's record, and the contract's Rhea balance equals the sum of all ledgers. v1.0 could break
//! it (the add succeeded, the callback that records it ran out of gas); v1.1 must never: it either
//! opens fully or refuses at the start and the token comes back whole.
//!
//! Then the recovery: the v1.0 contract with a stranded position is upgraded in place to v1.1, its
//! own key records the position and the leftover for the user, and the user cashes out in full.
//! Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

const POOL: &str = "tkq.sandbox|wrap.sandbox|10000";
const TKQ: &str = "tkq.sandbox";
const WRAP: &str = "wrap.sandbox";

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
    /// "ok", or the first contract error of the transaction (inner receipts included).
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken, tgas: u64) -> String {
        let out = Contract(to.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(tgas))
            .deposit(deposit)
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await;
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let err = ["ExecutionError(\"", "Smart contract panicked: "].iter().find_map(|m| dbg.find(m).map(|i| dbg[i + m.len()..].split('"').next().unwrap_or("").to_string()));
                match (r.is_success(), err) {
                    (true, None) => "ok".into(),
                    (_, Some(e)) => format!("ERR {e}"),
                    (false, None) => "ERR (failed)".into(),
                }
            }
            Err(e) => format!("ERR {e}").chars().take(200).collect(),
        }
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn ft(&self, token: &str, id: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": id })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn near(&self, id: &str) -> u128 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.amount.as_yoctonear()).unwrap_or(0)
    }
    /// Rhea positions this contract owns, and the ones on users' records; Rhea balance vs ledgers.
    async fn audit(&self, g: &str, users: &[String]) -> (Vec<String>, Vec<String>, HashMap<String, u128>, HashMap<String, u128>) {
        let on_rhea: Vec<String> = self.view("dcl.sandbox", "list_liquidities", json!({ "account_id": g })).await.as_array().cloned().unwrap_or_default().iter().map(|p| p["lpt_id"].as_str().unwrap().to_string()).collect();
        let mut recorded = Vec::new();
        let mut ledgers: HashMap<String, u128> = HashMap::new();
        for u in users {
            let a = self.view(g, "get_account", json!({ "user": u })).await;
            if let Some(id) = a["position"]["lpt_id"].as_str() {
                recorded.push(id.to_string());
            }
            for e in a["ledger"].as_array().cloned().unwrap_or_default() {
                *ledgers.entry(e[0].as_str().unwrap().into()).or_default() += e[1].as_str().unwrap().parse::<u128>().unwrap();
            }
        }
        let rhea: HashMap<String, u128> = self.view("dcl.sandbox", "list_user_assets", json!({ "account_id": g })).await.as_object().cloned().unwrap_or_default().into_iter().map(|(k, v)| (k, v.as_str().unwrap().parse().unwrap())).filter(|(_, v)| *v > 0).collect();
        (on_rhea, recorded, rhea, ledgers.into_iter().filter(|(_, v)| *v > 0).collect())
    }
}

#[tokio::test]
async fn gas_sweep_and_recovery() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    let steps: Vec<u64> = vec![300, 280, 260, 240, 220, 200, 180, 160, 140, 120, 100];
    let mut names: Vec<String> = ["owner", "dcl", "wrap", "farm", "tkq", "lp", "g10", "g11"].iter().map(|s| s.to_string()).collect();
    // v1.0 needs only the mainnet case (300 opens, 280 strands); v1.1 gets the whole sweep.
    let steps_a: Vec<u64> = vec![300, 280];
    for p in &steps_a {
        names.push(format!("a{p}"));
    }
    for p in &steps {
        names.push(format!("b{p}"));
    }
    for name in &names {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(60)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let (yocto, zero) = (NearToken::from_yoctonear(1), NearToken::from_near(0));

    // Rhea as on mainnet, with deep liquidity from another LP.
    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy("dcl.sandbox", fixture("dcl-2.4.0.wasm"), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    t.call("owner.sandbox", "dcl.sandbox", "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto, 100).await;
    t.call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 10000, "init_point": 304800 }), NearToken::from_near(1), 100).await;
    for who in ["dcl.sandbox", "lp.sandbox"] {
        t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125), 30).await;
        t.call("owner.sandbox", WRAP, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125), 30).await;
    }
    t.call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500), 30).await;
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "100000000000000000" }), yocto, 30).await;
    t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(40), 30).await;
    for (tok, amt) in [(TKQ, "50000000000000000"), (WRAP, "30000000000000000000000000")] {
        t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), yocto, 300).await;
    }
    assert_eq!(t.call("lp.sandbox", "dcl.sandbox", "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": "20000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0" }), zero, 300).await, "ok");

    // Two Guardians: v1.0 as first deployed on mainnet, and the current code (v1.1 gas floors and on).
    for (g, code) in [("g10.sandbox", fixture("shared-v1.0.wasm")), ("g11.sandbox", built("kinu_guardian_shared"))] {
        deploy(g, code, json!({ "dcl": "dcl.sandbox", "wnear": WRAP })).await;
        assert_eq!(t.call("owner.sandbox", g, "register_dcl", json!({}), NearToken::from_near(2), 50).await, "ok");
        for tok in [TKQ, WRAP] {
            assert_eq!(t.call("owner.sandbox", g, "register_token", json!({ "token": tok }), NearToken::from_millinear(125), 30).await, "ok");
        }
    }
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let point = t.view("dcl.sandbox", "get_pool", json!({ "pool_id": POOL })).await["current_point"].as_i64().unwrap();
    let right = (point - 200).div_euclid(200) * 200;
    let open = json!({ "left_point": right - 1400, "right_point": right }).to_string();

    let mut stranded: Vec<(String, String)> = Vec::new(); // (user, lpt) on v1.0
    let rpc = format!("http://{}", sandbox.rpc_addr.trim_start_matches("http://"));
    for (v, g, sweep) in [("a", "g10.sandbox", &steps_a), ("b", "g11.sandbox", &steps)] {
        let mut users: Vec<String> = Vec::new();
        println!("--- {g}: 1 wNEAR deposit that opens a one-sided range, gas given to ft_transfer_call:");
        for p in sweep.iter() {
            let u = format!("{v}{p}.sandbox");
            users.push(u.clone());
            assert_eq!(t.call(&u, g, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10), 30).await, "ok");
            t.call(&u, WRAP, "near_deposit", json!({}), NearToken::from_near(2), 10).await;
            let before = t.ft(WRAP, &u).await;
            let r = t.call(&u, WRAP, "ft_transfer_call", json!({ "receiver_id": g, "amount": "1000000000000000000000000", "msg": open }), yocto, *p).await;
            let a = t.view(g, "get_account", json!({ "user": u })).await;
            let back = t.ft(WRAP, &u).await == before;
            let (on_rhea, recorded, rhea, ledgers) = t.audit(g, &users).await;
            let orphans: Vec<String> = on_rhea.iter().filter(|l| !recorded.contains(l)).cloned().collect();
            let outcome = if a["position"].is_object() {
                "OPENED".to_string()
            } else if back {
                "refused, token back whole".to_string()
            } else if !orphans.is_empty() {
                format!("STRANDED {}", orphans.join(","))
            } else {
                format!("on ledger, no position ({})", a["ledger"])
            };
            println!("  {p:>3} TGas: {outcome:<44} tx: {}", r.chars().take(70).collect::<String>());
            let exact = rhea == ledgers;
            if v == "b" {
                assert!(orphans.is_empty(), "v1.1 stranded a position at {p} TGas: {orphans:?}");
                assert!(exact, "v1.1 ledger not exact at {p} TGas: rhea {rhea:?} ledgers {ledgers:?}");
            } else if !orphans.is_empty() && stranded.iter().all(|(_, l)| !orphans.contains(l)) {
                stranded.push((u.clone(), orphans[0].clone()));
            }
            // Wait out any lock a half-done chain left (v1.0), so the next row starts clean.
            if t.view(g, "get_config", json!({})).await["busy"] == json!(true) {
                sandbox_wait(&t, g, &rpc).await;
            }
        }
    }
    println!("v1.0 stranded: {stranded:?}");

    // Recovery on v1.0 (if it stranded one, as on mainnet): upgrade in place, record, cash out.
    if let Some((user, lpt)) = stranded.first().cloned() {
        // As on mainnet: v1.0 → v1.1 (same state layout, no migration).
        let ok = Contract::deploy("g10.sandbox".parse()?).use_code(fixture("shared-v1.1.wasm")).without_init_call().with_signer(signer.clone()).send_to(&net).await?.is_success();
        assert!(ok, "upgrade in place");
        assert_eq!(t.view("g10.sandbox", "get_config", json!({})).await["version"], "1.1.0");
        let users_a: Vec<String> = steps_a.iter().map(|p| format!("a{p}.sandbox")).collect();
        let (_, _, rhea, ledgers) = t.audit("g10.sandbox", &users_a).await;
        assert!(t.call(&user, "g10.sandbox", "recover_position", json!({ "user": user, "lpt_id": lpt }), zero, 50).await.contains("Only the contract account itself"));
        assert_eq!(t.call("g10.sandbox", "g10.sandbox", "recover_position", json!({ "user": user, "lpt_id": lpt }), zero, 50).await, "ok");
        assert_eq!(t.view("g10.sandbox", "get_account", json!({ "user": user })).await["position"]["lpt_id"], json!(lpt));
        // The leftover no record holds goes to the same user (on mainnet: the one unrecorded add).
        for (tok, amt) in &rhea {
            let unassigned = amt - ledgers.get(tok).copied().unwrap_or(0);
            if unassigned > 0 {
                assert_eq!(t.call("g10.sandbox", "g10.sandbox", "recover_credit", json!({ "user": user, "token": tok, "amount": unassigned.to_string() }), zero, 30).await, "ok");
            }
        }
        let (on_rhea, recorded, rhea, ledgers) = t.audit("g10.sandbox", &users_a).await;
        assert!(on_rhea.iter().all(|l| recorded.contains(l)) && rhea == ledgers, "recovered state is exact");
        let near_before = t.near(&user).await;
        assert_eq!(t.call(&user, "g10.sandbox", "cash_out", json!({ "leave": true }), zero, 300).await, "ok");
        let got = (t.near(&user).await - near_before) as f64 / 1e24;
        println!("{user} cashed out after recovery: +{got:.4} NEAR (1 NEAR in, 0.01 deposit back, less gas and the range's TKQ)");
        assert!(t.view("g10.sandbox", "get_account", json!({ "user": user })).await.is_null());
        assert!(!t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "g10.sandbox" })).await.as_array().unwrap().iter().any(|p| p["lpt_id"] == json!(lpt)));
    }

    // Every v1.1 user cashes out: nothing left on the contract.
    let users_b: Vec<String> = steps.iter().map(|p| format!("b{p}.sandbox")).collect();
    for u in &users_b {
        let r = t.call(u, "g11.sandbox", "cash_out", json!({ "leave": true }), zero, 300).await;
        assert!(r == "ok" || r.contains("Nothing to cash out"), "{u}: {r}");
    }
    let (on_rhea, _, rhea, _) = t.audit("g11.sandbox", &users_b).await;
    println!("v1.1 after everyone left: positions {on_rhea:?}, Rhea balance {rhea:?}");
    assert!(on_rhea.is_empty() && rhea.is_empty());
    Ok(())
}

/// Lets a stuck lock (BUSY_SECS = 120 s of block time) expire: the sandbox jumps ahead in blocks.
async fn sandbox_wait(t: &T, g: &str, rpc: &str) {
    let client = reqwest::Client::new();
    for _ in 0..20 {
        let _ = client.post(rpc).json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "sandbox_fast_forward", "params": { "delta_height": 100 } })).send().await;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        if t.view(g, "get_config", json!({})).await["busy"] != json!(true) {
            return;
        }
    }
    panic!("lock never expired");
}
