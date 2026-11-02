//! v1.7 with people inside to v1.8: records, books and every way out stay as they were.

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
fn built_short() -> Vec<u8> {
    let path = format!("{}/../target/near/kinu_guardian_shared_short/kinu_guardian_shared.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
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



#[tokio::test]
async fn v17_with_people_inside_to_v18() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "kinu", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "bob"] {
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
    let (yocto, zero, reg) = (NearToken::from_yoctonear(1), NearToken::from_near(0), NearToken::from_millinear(125));
    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy(DCL, kinu_guardian_tests::dcl_wasm(), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    ok("whitelist", t.call("owner.sandbox", DCL, "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await);
    ok("create_pool", t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 2000, "init_point": 304800 }), NearToken::from_near(1)).await);
    for who in [DCL, G, "lp.sandbox", "alice.sandbox", "bob.sandbox"] {
        for tok in [TKQ, WRAP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
    }
    for who in ["lp.sandbox", "alice.sandbox", "bob.sandbox"] {
        ok("fund", t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": "100000000000000000" }), yocto).await);
        ok("wrap", t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(60)).await);
    }
    ok("lp on Rhea", t.call("lp.sandbox", DCL, "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await);
    for (tok, amt) in [(TKQ, "50000000000000000".to_string()), (WRAP, (45 * E24).to_string())] {
        ok("lp deposit", t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": amt, "msg": "\"Deposit\"" }), yocto).await);
    }
    ok("lp liquidity", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": (20 * E24).to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);
    deploy(G, fixture("shared-v1.7.wasm"), json!({ "dcl": DCL, "wnear": WRAP, "owner": "kinu.sandbox" })).await;
    assert_eq!(t.view(G, "get_config", json!({})).await["version"], "1.7.0", "the fixture is the staged v1.7");
    ok("register_dcl", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in [TKQ, WRAP] {
        ok("register_token", t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), reg).await);
    }
    ok("list tokens", t.call("kinu.sandbox", G, "list_tokens", json!({ "tokens": [TKQ, WRAP], "listed": true }), NearToken::from_millinear(100)).await);
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 2877, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let (a, b) = ("alice.sandbox", "bob.sandbox");
    ok("alice joins", t.call(a, G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), NearToken::from_millinear(10)).await);
    ok("alice deposits", t.call(a, TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "100000000000", "msg": "" }), yocto).await);
    ok("alice deposits wNEAR", t.call(a, WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": E24.to_string(), "msg": "" }), yocto).await);
    let c = (t.point(POOL).await / 200) * 200;
    ok("alice opens", t.call(a, G, "open", json!({ "left_point": c - 600, "right_point": c + 800 }), zero).await);
    ok("the agent reads", t.call("agent.sandbox", G, "poke", json!({ "user": a }), zero).await);
    wait_secs(&sandbox, 12).await;
    ok("bob joins", t.call(b, G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await);
    ok("bob deposits, ledger only", t.call(b, WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": (E24 / 2).to_string(), "msg": "" }), yocto).await);
    t.books("v1.7 before the upgrade").await;
    let snap = (t.acct(a).await, t.acct(b).await, t.view(G, "get_seen", json!({ "user": a })).await);
    Contract::deploy(G.parse().unwrap()).use_code(built("kinu_guardian_shared")).with_init_call("migrate", json!({ "owner": "kinu.sandbox" })).unwrap().with_signer(signer.clone()).send_to(&net).await.unwrap().assert_success();
    assert_eq!(t.view(G, "get_config", json!({})).await["version"], "1.8.0");
    assert_eq!(snap, (t.acct(a).await, t.acct(b).await, t.view(G, "get_seen", json!({ "user": a })).await), "records unchanged");
    assert!(t.view(G, "get_sale", json!({ "user": a })).await.is_null());
    t.books("v1.8 after the upgrade").await;
    ok("alice closes herself: no sale follows", t.call(a, G, "exit", json!({ "user": a }), zero).await);
    assert!(t.view(G, "get_sale", json!({ "user": a })).await.is_null(), "only a cut loss leaves a sale");
    wait_secs(&sandbox, 12).await;
    ok("alice cashes out", t.call(a, G, "cash_out", json!({ "leave": true }), zero).await);
    ok("bob cashes out", t.call(b, G, "cash_out", json!({ "leave": true }), zero).await);
    t.books("everyone out").await;
    assert_eq!(t.total(WRAP).await, 0);
    Ok(())
}
