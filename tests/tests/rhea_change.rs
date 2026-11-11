//! A Rhea upgrade under Guardian, on the production build and a stand-in Rhea that can change: more
//! gas on its liquidity and withdraw calls, an answer in a new shape, a method gone. For each change,
//! which ways out still work for a two-token position, and whether every token comes back once Rhea
//! is right again. Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const POOL: &str = "tka.sandbox|tkb.sandbox|2000";
const TKA: &str = "tka.sandbox";
const TKB: &str = "tkb.sandbox";
const DCL: &str = "dcl.sandbox";
const G: &str = "guardian.sandbox";
const IN: u128 = 1_000_000;

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
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        let out = Contract(to.parse().unwrap()).call_function(method, args).transaction().gas(NearGas::from_tgas(300)).deposit(deposit).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let err = ["Smart contract panicked: ", "ExecutionError(\""].iter().find_map(|m| dbg.find(m).map(|i| dbg[i + m.len()..].chars().take(70).collect::<String>().split('"').next().unwrap_or("").to_string()));
                match (r.is_success(), err) {
                    (true, None) => "ok".into(),
                    (_, Some(e)) => format!("FAIL {e}"),
                    (false, None) => "FAIL".into(),
                }
            }
            Err(e) => format!("ERR {e}").chars().take(120).collect(),
        }
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn ft(&self, token: &str, id: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": id })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn acct(&self, who: &str) -> Value {
        self.view(G, "get_account", json!({ "user": who })).await
    }
    async fn back(&self, who: &str) -> u128 {
        self.ft(TKA, who).await + self.ft(TKB, who).await
    }
    async fn state(&self, who: &str) -> String {
        let a = self.acct(who).await;
        let cfg = self.view(G, "get_config", json!({})).await;
        format!("wallet {} of {}, position {}, ledger {}, owed {}, busy {}, unsettled {}", self.back(who).await, 2 * IN, a["position"]["lpt_id"].is_string(), a["ledger"], a["owed"], cfg["busy"], cfg["unsettled"])
    }
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
async fn a_rhea_upgrade_under_guardian() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    let burns: [u64; 8] = [0, 35, 45, 55, 75, 95, 115, 140];
    let shapes = ["liquidity", "pool", "assets", "removed", "gone"];
    let mut names: Vec<String> = ["owner", "kinu", "dcl", "wrap", "tka", "tkb", "guardian"].iter().map(|s| s.to_string()).collect();
    names.extend(burns.iter().map(|b| format!("gas{b}")));
    names.extend(shapes.iter().map(|s| format!("sh{s}")));
    for name in &names {
        sandbox.create_account(format!("{name}.sandbox").parse::<AccountId>()?).initial_balance(NearToken::from_near(40)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move { Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success() }
    };
    let (yocto, zero, reg, join) = (NearToken::from_yoctonear(1), NearToken::from_near(0), NearToken::from_millinear(125), NearToken::from_millinear(10));
    deploy("wrap.sandbox", fixture("wrap-near.wasm"), json!({})).await;
    deploy(DCL, built("kinu_guardian_mockdcl"), json!({})).await;
    for tok in [TKA, TKB] {
        deploy(tok, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000", "symbol": "TK", "decimals": 6 })).await;
    }
    deploy(G, built("kinu_guardian_shared"), json!({ "dcl": DCL, "wnear": "wrap.sandbox", "owner": "kinu.sandbox" })).await;
    assert_eq!(t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": TKA, "token_b": TKB, "fee": 2000, "init_point": 0 }), zero).await, "ok");
    assert_eq!(t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await, "ok");
    assert_eq!(t.call("kinu.sandbox", G, "list_factories", json!({ "parents": ["sandbox"], "trusted": true }), zero).await, "ok");
    let users: Vec<String> = names[7..].iter().map(|n| format!("{n}.sandbox")).collect();
    for tok in [TKA, TKB] {
        for who in [DCL, G] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
        for who in &users {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
            assert_eq!(t.call("owner.sandbox", tok, "ft_transfer", json!({ "receiver_id": who, "amount": IN.to_string() }), yocto).await, "ok");
        }
        assert_eq!(t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), reg).await, "ok");
    }
    let policy = json!({ "allowed_pools": [POOL], "min_width": 400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 2877, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let open = |who: String| {
        let (t, policy) = (&t, policy.clone());
        async move {
            assert_eq!(t.call(&who, G, "join", json!({ "policy": policy, "agent": null }), join).await, "ok", "join");
            // Like the page: wait out the window another user's step left (v1.8).
            while !t.view(G, "get_config", json!({})).await["exit_window"].is_null() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            for tok in [TKA, TKB] {
                assert_eq!(t.call(&who, tok, "ft_transfer_call", json!({ "receiver_id": G, "amount": IN.to_string(), "msg": "" }), yocto).await, "ok", "deposit");
            }
            let r = t.call(&who, G, "open", json!({ "left_point": -800, "right_point": 800 }), zero).await;
            assert_eq!(r, "ok", "open");
            assert!(t.acct(&who).await["position"]["lpt_id"].is_string(), "opened");
        }
    };
    let change = |burn: u64, shape: &'static str| {
        let t = &t;
        async move { assert_eq!(t.call("owner.sandbox", DCL, "set_change", json!({ "burn": burn, "shape": shape }), zero).await, "ok") }
    };

    println!("\n--- Rhea needs more gas ---");
    for b in burns {
        let who = format!("gas{b}.sandbox");
        open(who.clone()).await;
        wait_secs(&sandbox, 11).await;
        change(b, "").await;
        let cash = t.call(&who, G, "cash_out", json!({ "leave": false }), zero).await;
        let by_cash = t.back(&who).await == 2 * IN;
        println!("{b:>4}T cash_out: {cash}; {}", t.state(&who).await);
        let mut route = if by_cash { "cash_out alone" } else { "" }.to_string();
        if !by_cash {
            if t.acct(&who).await["position"]["lpt_id"].is_string() {
                let r = t.call(&who, G, "exit", json!({ "user": who }), zero).await;
                println!("      exit: {r}; {}", t.state(&who).await);
            }
            for tok in [TKA, TKB] {
                if t.acct(&who).await["ledger"].as_array().is_some_and(|l| l.iter().any(|e| e[0] == tok)) {
                    let r = t.call(&who, G, "withdraw", json!({ "token": tok }), zero).await;
                    println!("      withdraw {tok}: {r}");
                }
            }
            if t.back(&who).await == 2 * IN {
                route = "exit, then withdraw each token".into();
            }
        }
        change(0, "").await;
        if route.is_empty() {
            println!("      {}", t.state(&who).await);
            route = "NOT OUT while the change holds".into();
            println!("      {}", t.call(&who, G, "cash_out", json!({ "leave": false }), zero).await);
        }
        let held = t.state(&who).await;
        println!("{b:>4}T => {route}; after Rhea is normal: {held}");
        assert_eq!(t.back(&who).await, 2 * IN, "{b}T: every token back");
    }

    println!("\n--- Rhea changes an answer or a method ---");
    for sh in shapes {
        let who = format!("sh{sh}.sandbox");
        open(who.clone()).await;
        wait_secs(&sandbox, 11).await;
        change(0, sh).await;
        println!("{sh}: cash_out: {}", t.call(&who, G, "cash_out", json!({ "leave": false }), zero).await);
        println!("    {}", t.state(&who).await);
        wait_secs(&sandbox, 125).await;
        if t.acct(&who).await["position"]["lpt_id"].is_string() {
            println!("    exit: {}", t.call(&who, G, "exit", json!({ "user": who }), zero).await);
            println!("    {}", t.state(&who).await);
            wait_secs(&sandbox, 125).await;
        }
        for tok in [TKA, TKB] {
            println!("    withdraw {tok}: {}", t.call(&who, G, "withdraw", json!({ "token": tok }), zero).await);
        }
        println!("    {}", t.state(&who).await);
        change(0, "").await;
        wait_secs(&sandbox, 125).await;
        println!("    Rhea normal: settle: {}", t.call(&who, G, "settle", json!({ "user": who }), zero).await);
        println!("    cash_out: {}", t.call(&who, G, "cash_out", json!({ "leave": false }), zero).await);
        println!("    end: {}", t.state(&who).await);
    }
    Ok(())
}
