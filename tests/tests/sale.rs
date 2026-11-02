//! A cut loss on the real Rhea DCL: the falling token is sold for the other one, into the wallet.

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


const BAND: i64 = 300;

async fn whale(t: &T, to: i64) {
    let cur = t.point(POOL).await;
    let (tok, out) = if to < cur { (TKQ, WRAP) } else { (WRAP, TKQ) };
    let m = json!({ "SwapByStopPoint": { "pool_id": POOL, "output_token": out, "stop_point": to, "skip_unwrap_near": true } }).to_string();
    ok("a whale moves the price", t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": if to < cur { "40000000000000000".to_string() } else { (20 * E24).to_string() }, "msg": m }), NearToken::from_yoctonear(1)).await);
    assert_eq!(t.point(POOL).await, to, "the whale reached the price");
}

/// Joins, deposits, opens, lets the agent confirm the price, moves it against the user and exits by the rule.
async fn to_sale(t: &T, s: &near_sandbox::Sandbox, who: &str, tok: &str, amount: u128, flip: bool) {
    let (zero, yocto) = (NearToken::from_near(0), NearToken::from_yoctonear(1));
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 6000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 500, "exit_on_rise": flip, "rebalance_buffer_points": 100 });
    ok("joins", t.call(who, G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), NearToken::from_millinear(10)).await);
    wait_secs(s, 12).await;
    ok("deposits", t.call(who, tok, "ft_transfer_call", json!({ "receiver_id": G, "amount": amount.to_string(), "msg": "" }), yocto).await);
    let c = (t.point(POOL).await / 200) * 200;
    ok("opens", t.call(who, G, "open", json!({ "left_point": if tok == WRAP { c - 1200 } else { c + 200 }, "right_point": if tok == WRAP { c - 200 } else { c + 1200 } }), zero).await);
    ok("agent reads", t.call("agent.sandbox", G, "poke", json!({ "user": who }), zero).await);
    wait_secs(s, 190).await;
    ok("agent reads again", t.call("agent.sandbox", G, "poke", json!({ "user": who }), zero).await);
    let to = t.point(POOL).await + if flip { 700 } else { -700 };
    whale(t, to).await;
    wait_secs(s, 190).await;
    ok("agent reads the fall", t.call("agent.sandbox", G, "poke", json!({ "user": who }), zero).await);
    wait_secs(s, 190).await;
    ok("agent reads it again", t.call("agent.sandbox", G, "poke", json!({ "user": who }), zero).await);
    wait_secs(s, 190).await;
    ok("agent exits by the rule", t.call("agent.sandbox", G, "exit", json!({ "user": who }), zero).await);
    assert!(t.acct(who).await["position"].is_null(), "closed");
    let sale = t.view(G, "get_sale", json!({ "user": who })).await;
    println!("{:<58} {sale}", "the sale left");
    assert_eq!(sale["token_in"], if flip { WRAP } else { TKQ });
    t.books("after the exit").await;
}

#[tokio::test]
async fn a_cut_loss_sells_on_the_real_rhea() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let s = &sandbox;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "kinu", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "bob", "carol", "dan", "erin", "mallory"] {
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
    for who in [DCL, G, "lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox", "dan.sandbox", "erin.sandbox"] {
        for tok in [TKQ, WRAP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
    }
    for who in ["lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox", "dan.sandbox", "erin.sandbox"] {
        ok("fund", t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": "100000000000000000" }), yocto).await);
        ok("wrap", t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(60)).await);
    }
    ok("the whale wraps more", t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(120)).await);
    ok("lp on Rhea", t.call("lp.sandbox", DCL, "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await);
    for (tok, amt) in [(TKQ, "50000000000000000".to_string()), (WRAP, (45 * E24).to_string())] {
        ok("lp deposit", t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": amt, "msg": "\"Deposit\"" }), yocto).await);
    }
    ok("lp liquidity", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": (20 * E24).to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);
    deploy(G, built("kinu_guardian_shared"), json!({ "dcl": DCL, "wnear": WRAP, "owner": "kinu.sandbox" })).await;
    assert_eq!(t.view(G, "get_config", json!({})).await["version"], "1.8.0");
    ok("register_dcl", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in [TKQ, WRAP] {
        ok("register_token", t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), reg).await);
    }
    ok("list tokens", t.call("kinu.sandbox", G, "list_tokens", json!({ "tokens": [TKQ, WRAP], "listed": true }), NearToken::from_millinear(100)).await);

    println!("\n--- alice: X falls, sold for NEAR, a small amount so all of it sells ---");
    let a = "alice.sandbox";
    to_sale(&t, s, a, TKQ, 10_000_000_000, false).await;
    let held = t.held(a, TKQ).await;
    assert!(held > 0, "the falling token is on her ledger");
    no("too soon: no read since the close", t.call("agent.sandbox", G, "exit", json!({ "user": a }), zero).await, "REFUSED");
    assert_eq!(t.held(a, TKQ).await, held);
    t.books("refused: nothing moved").await;
    ok("the agent reads with no position", t.call("agent.sandbox", G, "poke", json!({ "user": a }), zero).await);
    let marks = (t.acct(a).await["high_point"].clone(), t.acct(a).await["low_point"].clone());
    assert!(marks.0.is_null() && marks.1.is_null(), "a read for a sale writes no marks");
    no("the read is too young", t.call("agent.sandbox", G, "exit", json!({ "user": a }), zero).await, "REFUSED");
    wait_secs(s, 190).await;
    no("a stranger cannot sell it", t.call("mallory.sandbox", G, "exit", json!({ "user": a }), zero).await, "agent");
    ok("alice freezes the agent", t.call(a, G, "set_frozen", json!({ "frozen": true }), zero).await);
    no("frozen: the agent cannot sell", t.call("agent.sandbox", G, "exit", json!({ "user": a }), zero).await, "rozen");
    ok("alice unfreezes", t.call(a, G, "set_frozen", json!({ "frozen": false }), yocto).await);
    ok("Guardian paused", t.call("kinu.sandbox", G, "set_paused", json!({ "paused": true }), zero).await);
    no("paused: no sale", t.call("agent.sandbox", G, "exit", json!({ "user": a }), zero).await, "aused");
    ok("resumed", t.call("kinu.sandbox", G, "set_paused", json!({ "paused": false }), zero).await);
    let p = t.point(POOL).await;
    whale(&t, p - (BAND + 100)).await;
    no("price too far from the read: no sale", t.call("agent.sandbox", G, "exit", json!({ "user": a }), zero).await, "REFUSED");
    assert_eq!(t.held(a, TKQ).await, held);
    t.books("price too far: nothing moved").await;
    ok("the agent reads the new price", t.call("agent.sandbox", G, "poke", json!({ "user": a }), zero).await);
    wait_secs(s, 190).await;
    let near0 = t.near(a).await;
    let tkq0 = t.ft(TKQ, a).await;
    let p = t.point(POOL).await;
    // Too little gas fails whole: nothing moves, the lock is free, the sale is still due.
    for tgas in [40, 80, 120, 160, 200, 230, 245] {
        let r = t.call_gas("agent.sandbox", G, "exit", json!({ "user": a }), zero, tgas).await;
        println!("{:<58} {r}", format!("the sale with {tgas} TGas"));
        if t.held(a, TKQ).await == 0 {
            panic!("sold at {tgas} TGas: the floor is lower than thought, {r}");
        }
        assert_eq!(t.held(a, TKQ).await, held, "{tgas}: the ledger moved");
        assert!(!t.view(G, "get_sale", json!({ "user": a })).await.is_null(), "{tgas}: the sale is lost");
        t.books("short of gas: nothing moved").await;
        wait_secs(s, 3).await;
    }
    // The chain's floor: 255 TGas is enough to the end.
    let r = t.call_gas("agent.sandbox", G, "exit", json!({ "user": a }), zero, 255).await;
    ok("the sale", r);
    let got = t.near(a).await - near0;
    println!("{:<58} {got:.6} NEAR, {} TKQ back, point {p} to {}", "alice receives", t.ft(TKQ, a).await - tkq0, t.point(POOL).await);
    assert!(got > 0.0, "NEAR reached her wallet");
    assert_eq!(t.held(a, TKQ).await, 0, "nothing left on the ledger");
    assert!(t.view(G, "get_sale", json!({ "user": a })).await.is_null());
    assert_eq!(t.ft(TKQ, G).await, 0, "nothing stays in the contract's wallet");
    t.books("after the sale").await;
    no("no second sale", t.call("agent.sandbox", G, "exit", json!({ "user": a }), zero).await, "No open position");

    println!("\n--- bob: Y falls, so wNEAR is sold for TKQ, a partial fill comes back to his wallet ---");
    let b = "bob.sandbox";
    to_sale(&t, s, b, WRAP, 30 * E24, true).await;
    ok("the agent reads", t.call("agent.sandbox", G, "poke", json!({ "user": b }), zero).await);
    wait_secs(s, 190).await;
    let (tkq0, wrap0, ledger) = (t.ft(TKQ, b).await, t.ft(WRAP, b).await, t.held(b, WRAP).await);
    let p = t.point(POOL).await;
    ok("the sale", t.call("agent.sandbox", G, "exit", json!({ "user": b }), zero).await);
    let (tkq1, wrap1) = (t.ft(TKQ, b).await, t.ft(WRAP, b).await);
    println!("{:<58} ledger {ledger}, TKQ +{}, wNEAR back +{}, point {p} to {}", "bob", tkq1 - tkq0, wrap1 - wrap0, t.point(POOL).await);
    assert!(tkq1 > tkq0 && wrap1 > wrap0, "part sold, the rest in his wallet");
    assert_eq!(t.point(POOL).await, p + BAND, "stopped exactly at the band");
    assert_eq!(t.held(b, WRAP).await, 0);
    assert_eq!(t.ft(WRAP, G).await, 0);
    t.books("after a partial sale").await;

    println!("\n--- carol: her wallet cannot receive the token, and she sells herself ---");
    let c = "carol.sandbox";
    to_sale(&t, s, c, WRAP, E24, true).await;
    let bal = t.ft(TKQ, c).await;
    ok("carol moves her TKQ away", t.call(c, TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": bal.to_string() }), yocto).await);
    ok("carol unregisters", t.call(c, TKQ, "storage_unregister", json!({ "force": false }), yocto).await);
    let ledger = t.held(c, WRAP).await;
    no("not registered: no sale", t.call(c, G, "exit", json!({ "user": c }), zero).await, "not registered");
    assert_eq!(t.held(c, WRAP).await, ledger);
    assert!(!t.view(G, "get_sale", json!({ "user": c })).await.is_null(), "still due");
    t.books("refused: nothing moved").await;
    t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": c, "registration_only": true }), reg).await;
    ok("carol sells it herself, no recorded price needed", t.call(c, G, "exit", json!({ "user": c }), zero).await);
    assert!(t.ft(TKQ, c).await > 0);
    t.books("after her sale").await;

    println!("\n--- dan: the user takes over ---");
    let d = "dan.sandbox";
    to_sale(&t, s, d, TKQ, 10_000_000_000, false).await;
    no("cancel needs a yocto", t.call(d, G, "cancel_sale", json!({}), zero).await, "yocto");
    ok("dan keeps his tokens", t.call(d, G, "cancel_sale", json!({}), yocto).await);
    assert!(t.view(G, "get_sale", json!({ "user": d })).await.is_null());
    no("the agent has nothing to do", t.call("agent.sandbox", G, "exit", json!({ "user": d }), zero).await, "No open position");
    assert!(t.held(d, TKQ).await > 0);
    let h = t.held(d, TKQ).await;
    ok("dan withdraws everything, as always", t.call(d, G, "withdraw", json!({ "token": TKQ }), zero).await);
    assert_eq!(t.held(d, TKQ).await, 0);
    assert!(h > 0);
    t.books("after dan").await;

    println!("\n--- erin: leaves the funds behind a sale ---");
    let e = "erin.sandbox";
    to_sale(&t, s, e, TKQ, 10_000_000_000, false).await;
    ok("erin takes it all before the sale", t.call(e, G, "cash_out", json!({ "leave": false }), zero).await);
    ok("the agent reads", t.call("agent.sandbox", G, "poke", json!({ "user": e }), zero).await);
    wait_secs(s, 190).await;
    no("nothing left to sell", t.call("agent.sandbox", G, "exit", json!({ "user": e }), zero).await, "Nothing left");
    assert!(t.view(G, "get_sale", json!({ "user": e })).await.is_null());
    t.books("after erin").await;
    Ok(())
}
