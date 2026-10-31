//! The mainnet upgrade with people inside: v1.5 exactly as live (fixture read from
//! lp.kinuonnear.near, code 2X4Kt3q...) -> the production build + migrate in ONE transaction, on the
//! live Rhea wasm. Every record, read, target, debt and total stays; old agent calls keep working;
//! a read made on v1.5 still counts; a user adds a second position; going back to v1.5 before anyone
//! uses a slot also works. Everything ends at zero. Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const POOL: &str = "tkq.sandbox|wrap.sandbox|2000";
const TKQ: &str = "tkq.sandbox";
const WRAP: &str = "wrap.sandbox";
const DCL: &str = "dcl.sandbox";
const G: &str = "guardian.sandbox";
const E24: u128 = 1_000_000_000_000_000_000_000_000;

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
    async fn call_gas(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken, tgas: u64) -> String {
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
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        self.call_gas(from, to, method, args, deposit, 300).await
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn near(&self, id: &str) -> f64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.amount.as_yoctonear() as f64 / 1e24).unwrap_or(0.0)
    }
    async fn ft(&self, token: &str, id: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": id })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn acct(&self, who: &str) -> Value {
        self.view(G, "get_account", json!({ "user": who })).await
    }
    async fn held(&self, who: &str, token: &str) -> u128 {
        self.acct(who).await["ledger"].as_array().cloned().unwrap_or_default().iter().find(|e| e[0] == token).map(|e| e[1].as_str().unwrap().parse().unwrap()).unwrap_or(0)
    }
    async fn rhea(&self, token: &str) -> u128 {
        self.view(DCL, "list_user_assets", json!({ "account_id": G })).await[token].as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn total(&self, token: &str) -> u128 {
        self.view(G, "get_ledger_total", json!({ "token": token })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn point(&self, pool: &str) -> i64 {
        self.view(DCL, "get_pool", json!({ "pool_id": pool })).await["current_point"].as_i64().unwrap()
    }
    /// The books must equal Rhea, token by token, and the lock must be free.
    async fn books(&self, stage: &str) {
        for token in [TKQ, WRAP] {
            assert_eq!(self.total(token).await, self.rhea(token).await, "{stage}: ledger_total of {token} differs from Rhea");
        }
        let cfg = self.view(G, "get_config", json!({})).await;
        assert_eq!(cfg["busy"], false, "{stage}: lock held");
        assert_eq!(cfg["unsettled"], json!([]), "{stage}: unsettled");
        println!("{:<58} books match Rhea", stage);
    }
}

fn ok(label: &str, r: String) {
    println!("{label:<58} {r}");
    assert!(r.starts_with("ok ("), "{label}: {r}");
}
fn no(label: &str, r: String, with: &str) {
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
}

async fn snapshot(t: &T, who: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for w in who {
        out.insert(format!("{w}.account"), t.acct(w).await);
        out.insert(format!("{w}.seen"), t.view(G, "get_seen", json!({ "user": w })).await);
        out.insert(format!("{w}.tp"), t.view(G, "get_take_profit", json!({ "user": w })).await);
    }
    for token in [TKQ, WRAP] {
        out.insert(format!("{token}.ledger_total"), t.view(G, "get_ledger_total", json!({ "token": token })).await);
        out.insert(format!("{token}.owed_total"), t.view(G, "get_owed_total", json!({ "token": token })).await);
        out.insert(format!("{token}.rhea"), json!(t.rhea(token).await.to_string()));
        out.insert(format!("{token}.wallet"), json!(t.ft(token, G).await.to_string()));
    }
    out.insert("users".into(), t.view(G, "get_users", json!({})).await);
    let mut cfg = t.view(G, "get_config", json!({})).await;
    cfg.as_object_mut().unwrap().remove("version");
    out.insert("config".into(), cfg);
    Value::Object(out)
}

#[tokio::test]
async fn v15_with_people_inside_to_v16() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "kinu", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "bob", "carol"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(200)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let (yocto, zero, reg, join) = (NearToken::from_yoctonear(1), NearToken::from_near(0), NearToken::from_millinear(125), NearToken::from_millinear(10));
    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy(DCL, kinu_guardian_tests::dcl_wasm(), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    ok("whitelist", t.call("owner.sandbox", DCL, "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await);
    ok("create_pool", t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 2000, "init_point": 304800 }), NearToken::from_near(1)).await);
    for who in [DCL, G, "lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        for tok in [TKQ, WRAP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
    }
    for who in ["lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        ok("fund", t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": "100000000000000000" }), yocto).await);
        ok("wrap", t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(60)).await);
    }
    ok("lp on Rhea", t.call("lp.sandbox", DCL, "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await);
    for (tok, amt) in [(TKQ, "50000000000000000".to_string()), (WRAP, (45 * E24).to_string())] {
        ok("lp deposit", t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": amt, "msg": "\"Deposit\"" }), yocto).await);
    }
    ok("lp liquidity", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": (20 * E24).to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);

    println!("\n--- v1.5 as live, with people inside ---");
    deploy(G, fixture("shared-v1.5.wasm"), json!({ "dcl": DCL, "wnear": WRAP, "owner": "kinu.sandbox" })).await;
    assert_eq!(t.view(G, "get_config", json!({})).await["version"], "1.5.0", "the fixture is the live v1.5");
    ok("register_dcl", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in [TKQ, WRAP] {
        ok("register_token", t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), reg).await);
    }
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 2877, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let tkq_in: u128 = 100_000_000_000;
    ok("alice joins, names the agent", t.call("alice.sandbox", G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), join).await);
    ok("alice deposits TKQ", t.call("alice.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": tkq_in.to_string(), "msg": "" }), yocto).await);
    ok("alice deposits wNEAR", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": E24.to_string(), "msg": "" }), yocto).await);
    let c = (t.point(POOL).await / 200) * 200;
    ok("alice opens", t.call("alice.sandbox", G, "open", json!({ "left_point": c - 600, "right_point": c + 800 }), zero).await);
    ok("alice sets a target", t.call("alice.sandbox", G, "set_take_profit", json!({ "target": { "point": 310000, "above": true } }), zero).await);
    ok("bob joins", t.call("bob.sandbox", G, "join", json!({ "policy": policy, "agent": null }), join).await);
    ok("bob deposits, ledger only", t.call("bob.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": (E24 / 2).to_string(), "msg": "" }), yocto).await);
    ok("carol joins", t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), join).await);
    ok("carol deposits", t.call("carol.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": tkq_in.to_string(), "msg": "" }), yocto).await);
    let held = t.ft(TKQ, "carol.sandbox").await;
    ok("carol moves her TKQ away", t.call("carol.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": held.to_string() }), yocto).await);
    ok("carol unregisters", t.call("carol.sandbox", TKQ, "storage_unregister", json!({ "force": false }), yocto).await);
    no("carol's wallet refuses: owed", t.call("carol.sandbox", G, "withdraw", json!({ "token": TKQ }), zero).await, "not registered");
    let sell = json!({ "Swap": { "pool_ids": [POOL], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    ok("a whale pushes the price out of alice's range", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "3000000000000", "msg": sell }), yocto).await);
    let p = t.point(POOL).await;
    assert!(p < c - 800, "out of range: {p}");
    ok("the agent reads the price on v1.5", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await);
    t.books("v1.5 before the upgrade").await;
    let who = ["alice.sandbox", "bob.sandbox", "carol.sandbox"];
    let before = snapshot(&t, &who).await;
    println!("{:<58} {}", "alice's record", before["alice.sandbox.account"]["position"]);
    println!("{:<58} {}", "alice's read", before["alice.sandbox.seen"]);
    println!("{:<58} {}", "carol's debt", before["carol.sandbox.account"]["owed"]);
    assert!(before["alice.sandbox.seen"].is_object() && before["alice.sandbox.tp"].is_object());

    println!("\n--- the upgrade ---");
    let upgrade = |code: Vec<u8>| {
        let (net, signer) = (net.clone(), signer.clone());
        async move { Contract::deploy(G.parse().unwrap()).use_code(code).with_init_call("migrate", json!({ "owner": "kinu.sandbox" })).unwrap().with_signer(signer).send_to(&net).await.unwrap() }
    };
    let code = built("kinu_guardian_shared");
    println!("{:<58} {} bytes", "production build", code.len());
    let r = upgrade(code.clone()).await;
    assert!(r.is_success(), "upgrade: {r:?}");
    let cfg = t.view(G, "get_config", json!({})).await;
    assert_eq!(cfg["version"], "1.6.0");
    assert_eq!(cfg["admin_delay_secs"], 172_800, "the production build");
    assert_eq!(snapshot(&t, &who).await, before, "every record, read, target, debt and total is the same");
    println!("{:<58} same", "all records, reads, targets, debts, totals");

    println!("\n--- going back, while no slot is in use ---");
    let r = upgrade(fixture("shared-v1.5.wasm")).await;
    assert!(r.is_success(), "back to v1.5: {r:?}");
    assert_eq!(t.view(G, "get_config", json!({})).await["version"], "1.5.0");
    assert_eq!(snapshot(&t, &who).await, before, "v1.5 reads the same state");
    let r = upgrade(code).await;
    assert!(r.is_success(), "forward again: {r:?}");
    assert_eq!(snapshot(&t, &who).await, before);
    println!("{:<58} same both ways", "v1.6 -> v1.5 -> v1.6");

    println!("\n--- the old agent calls still work ---");
    wait_secs(&sandbox, 200).await;
    let left = (p + 20).div_euclid(200) * 200 + 200;
    let fair = json!({ "pool_id": POOL, "left_point": left, "right_point": left + 1400 });
    ok("old agent call, on the read made on v1.5", t.call("agent.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox", "request": fair }), zero).await);
    assert_eq!(t.acct("alice.sandbox").await["position"]["left_point"], json!(left));
    assert_eq!(t.acct("alice.sandbox").await["rate"]["used"], json!(1));
    t.books("after the agent's move").await;

    println!("\n--- users on v1.6 ---");
    // Bob holds wNEAR only: his range sits below the price.
    let under = (t.point(POOL).await - 40).div_euclid(200) * 200;
    ok("the old page: bob opens with no slot", t.call("bob.sandbox", G, "open", json!({ "left_point": under - 1400, "right_point": under }), zero).await);
    ok("carol registers again", t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": "carol.sandbox", "registration_only": true }), reg).await);
    ok("carol claims what v1.5 owed her", t.call("carol.sandbox", G, "claim", json!({ "token": TKQ }), zero).await);
    assert_eq!(t.ft(TKQ, "carol.sandbox").await, tkq_in);
    ok("carol leaves", t.call("carol.sandbox", G, "leave", json!({}), zero).await);
    ok("alice adds a second position", t.call("alice.sandbox", G, "join", json!({ "policy": policy, "agent": "agent.sandbox", "slot": 1 }), join).await);
    ok("deposit to it", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": E24.to_string(), "msg": "1" }), yocto).await);
    let now = t.point(POOL).await;
    let below = (now - 40).div_euclid(200) * 200;
    ok("open it, one-sided", t.call("alice.sandbox", G, "open", json!({ "left_point": below - 1400, "right_point": below, "slot": 1 }), zero).await);
    assert_eq!(t.view(G, "get_slots", json!({ "user": "alice.sandbox" })).await, json!([0, 1]));
    t.books("two positions").await;
    ok("claim fees on slot 0", t.call("alice.sandbox", G, "claim_fees", json!({ "send": true }), zero).await);
    let n = t.near("alice.sandbox").await;
    ok("alice cashes out slot 0", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    println!("{:<58} +{:.4} NEAR", "alice received", t.near("alice.sandbox").await - n);
    assert!(t.acct("alice.sandbox").await.is_null());
    assert!(t.acct("alice.sandbox#1").await["position"].is_object(), "slot 1 stays");
    assert!(t.view(G, "get_users", json!({})).await.as_array().unwrap().iter().any(|u| u == "alice.sandbox"), "still listed");
    t.books("slot 0 gone").await;
    ok("alice cashes out slot 1", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true, "slot": 1 }), zero).await);
    ok("bob cashes out", t.call("bob.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);

    assert_eq!(t.view(G, "get_users", json!({})).await, json!([]));
    t.books("the end").await;
    let rhea = t.view(DCL, "list_user_assets", json!({ "account_id": G })).await;
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"), "nothing left on Rhea: {rhea}");
    assert_eq!(t.view(DCL, "list_liquidities", json!({ "account_id": G })).await, json!([]));
    for token in [TKQ, WRAP] {
        assert_eq!(t.ft(token, G).await, 0, "contract wallet {token}");
        assert_eq!(t.view(G, "get_owed_total", json!({ "token": token })).await, json!("0"));
    }
    println!("\nupgraded with people inside, nothing lost, nothing stranded");
    Ok(())
}
