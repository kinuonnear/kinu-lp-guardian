//! v1.6 on the real Rhea DCL: one wallet with three positions, a second wallet that shares the
//! tokens, an agent named on one slot, a wallet that refuses a token, a tax token, and a gas sweep.
//! After every stage the books must match Rhea: ledger_total equals Rhea's balance for each token.
//! Everything ends at zero: no record, no ledger, nothing owed, nothing on Rhea, nothing in the
//! contract's wallet. Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const POOL: &str = "tkq.sandbox|wrap.sandbox|10000";
const POOL2: &str = "tkq.sandbox|wrap.sandbox|2000";
const POOL_TAX: &str = "tax.sandbox|wrap.sandbox|10000";
const TKQ: &str = "tkq.sandbox";
const TAX: &str = "tax.sandbox";
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
    async fn storage(&self, id: &str) -> u64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.storage_usage).unwrap_or(0)
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
        for token in [TKQ, WRAP, TAX] {
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
async fn three_positions_on_real_dcl() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "tax", "lp", "agent", "guardian", "alice", "bob", "carol", "thief"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(200)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, method: &str, init: Value| {
        let (net, signer, id, method) = (net.clone(), signer.clone(), id.to_string(), method.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call(&method, init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let (yocto, zero, reg, join) = (NearToken::from_yoctonear(1), NearToken::from_near(0), NearToken::from_millinear(125), NearToken::from_millinear(10));
    deploy(WRAP, fixture("wrap-near.wasm"), "new", json!({})).await;
    deploy(DCL, kinu_guardian_tests::dcl_wasm(), "new", json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), "new", json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    deploy(TAX, built("kinu_guardian_testtoken"), "new_taxed", json!({ "owner_id": "owner.sandbox", "total_supply": (1_000_000 * E24).to_string(), "symbol": "TAX", "decimals": 24, "tax": { "pair": DCL, "buy_bps": 100, "sell_bps": 100 } })).await;
    ok("whitelist", t.call("owner.sandbox", DCL, "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, TAX, WRAP] }), yocto).await);
    for (a, fee, p) in [(TKQ, 10000, 304800), (TKQ, 2000, 304800), (TAX, 10000, 0)] {
        ok("create_pool", t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": a, "token_b": WRAP, "fee": fee, "init_point": p }), NearToken::from_near(1)).await);
    }
    for who in [DCL, G, "lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        for tok in [TKQ, TAX, WRAP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
    }
    for who in ["lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        ok("fund TKQ", t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": "100000000000000000" }), yocto).await);
        ok("fund TAX", t.call("owner.sandbox", TAX, "ft_transfer", json!({ "receiver_id": who, "amount": (10_000 * E24).to_string() }), yocto).await);
        ok("wrap", t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(60)).await);
    }
    ok("lp on Rhea", t.call("lp.sandbox", DCL, "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await);
    for (tok, amt) in [(TKQ, "50000000000000000".to_string()), (TAX, (5_000 * E24).to_string()), (WRAP, (45 * E24).to_string())] {
        ok("lp deposit", t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": amt, "msg": "\"Deposit\"" }), yocto).await);
    }
    for (pool, l, r, x, y) in [(POOL, 284800, 324800, "2000000000000".to_string(), 20 * E24), (POOL2, 284800, 324800, "2000000000000".to_string(), 20 * E24), (POOL_TAX, -20000, 20000, (4 * E24).to_string(), 4 * E24)] {
        ok("lp liquidity", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": pool, "left_point": l, "right_point": r, "amount_x": x, "amount_y": y.to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);
    }

    deploy(G, built("kinu_guardian_shared"), "new", json!({ "dcl": DCL, "wnear": WRAP, "owner": "owner.sandbox" })).await;
    assert_eq!(t.view(G, "get_config", json!({})).await["version"], "1.6.0");
    ok("register_dcl", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in [TKQ, TAX, WRAP] {
        ok("register_token", t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), reg).await);
    }

    let policy = |pool: &str| json!({ "allowed_pools": [pool], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });

    println!("\n--- three positions, one wallet ---");
    let huge = format!("{TKQ}|{WRAP}|{}", "9".repeat(50_000));
    no("a huge pool id is refused", t.call("carol.sandbox", G, "join", json!({ "policy": policy(&huge), "agent": null }), join).await, "exactly one pool");
    let storage0 = t.storage(G).await;
    let tkq_in: u128 = 100_000_000_000;
    let wnear_in: u128 = E24;
    ok("alice joins slot 0 (1% pool, agent)", t.call("alice.sandbox", G, "join", json!({ "policy": policy(POOL), "agent": "agent.sandbox" }), join).await);
    ok("alice joins slot 1 (0.2% pool, thief as agent)", t.call("alice.sandbox", G, "join", json!({ "policy": policy(POOL2), "agent": "thief.sandbox", "slot": 1 }), join).await);
    ok("alice joins slot 2 (same pool as slot 0)", t.call("alice.sandbox", G, "join", json!({ "policy": policy(POOL), "agent": null, "slot": 2 }), join).await);
    no("slot 3 does not exist", t.call("alice.sandbox", G, "join", json!({ "policy": policy(POOL), "agent": null, "slot": 3 }), join).await, "positions 0, 1 and 2");
    ok("bob joins slot 1 only", t.call("bob.sandbox", G, "join", json!({ "policy": policy(POOL), "agent": null, "slot": 1 }), join).await);
    assert_eq!(t.view(G, "get_slots", json!({ "user": "alice.sandbox" })).await, json!([0, 1, 2]));
    assert_eq!(t.view(G, "get_slots", json!({ "user": "bob.sandbox" })).await, json!([1]));
    assert_eq!(t.view(G, "get_user_count", json!({})).await, json!(2));
    for (who, msg) in [("alice.sandbox", ""), ("alice.sandbox", "1"), ("alice.sandbox", "2"), ("bob.sandbox", "1")] {
        ok("deposit TKQ", t.call(who, TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": tkq_in.to_string(), "msg": msg }), yocto).await);
        ok("deposit wNEAR", t.call(who, WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": wnear_in.to_string(), "msg": msg }), yocto).await);
    }
    let wallet_tkq = t.ft(TKQ, "bob.sandbox").await;
    let r = t.call("bob.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "5", "msg": "2" }), yocto).await;
    println!("{:<58} {r}", "bob deposits to a slot he has not joined");
    assert_eq!(t.ft(TKQ, "bob.sandbox").await, wallet_tkq, "the token came back");
    for who in ["alice.sandbox", "alice.sandbox#1", "alice.sandbox#2", "bob.sandbox#1"] {
        assert_eq!(t.held(who, TKQ).await, tkq_in, "{who}");
        assert_eq!(t.held(who, WRAP).await, wnear_in, "{who}");
    }
    assert_eq!(t.total(TKQ).await, 4 * tkq_in);
    t.books("after four deposits").await;

    let centre = |p: i64| (p / 200) * 200; // both pools accept multiples of 200
    let c1 = centre(t.point(POOL).await);
    let c2 = centre(t.point(POOL2).await);
    ok("open slot 0", t.call("alice.sandbox", G, "open", json!({ "left_point": c1 - 600, "right_point": c1 + 800 }), zero).await);
    ok("open slot 1", t.call("alice.sandbox", G, "open", json!({ "left_point": c2 - 600, "right_point": c2 + 800, "slot": 1 }), zero).await);
    ok("open slot 2 in the same pool", t.call("alice.sandbox", G, "open", json!({ "left_point": c1 - 1000, "right_point": c1 + 400, "slot": 2 }), zero).await);
    ok("bob opens", t.call("bob.sandbox", G, "open", json!({ "left_point": c1 - 600, "right_point": c1 + 800, "slot": 1 }), zero).await);
    t.books("after four opens").await;
    let used = t.storage(G).await - storage0;
    println!("{:<58} {used} bytes for 4 records", "storage");
    assert!(used / 4 < 1_000, "a record stays inside its join deposit (1,000 bytes): {used}");
    let lpt: Vec<String> = vec![t.acct("alice.sandbox").await["position"]["lpt_id"].as_str().unwrap().into(), t.acct("alice.sandbox#2").await["position"]["lpt_id"].as_str().unwrap().into()];
    assert_ne!(lpt[0], lpt[1], "two positions in one pool are two Rhea positions");

    println!("\n--- attacks across slots ---");
    let before: Vec<Value> = futures_all(&t, &["alice.sandbox", "alice.sandbox#1", "alice.sandbox#2", "bob.sandbox#1"]).await;
    let far = json!({ "pool_id": POOL, "left_point": c1 - 600, "right_point": c1 + 800 });
    no("agent of slot 1 moves slot 0", t.call("thief.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox", "request": far }), zero).await, "Only the vault's agent");
    no("agent of slot 1 moves slot 2", t.call("thief.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox#2", "request": far }), zero).await, "Only the vault's agent");
    no("agent of slot 1 closes slot 0", t.call("thief.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "Only the vault's agent");
    no("agent of slot 1 closes bob", t.call("thief.sandbox", G, "exit", json!({ "user": "bob.sandbox#1" }), zero).await, "Only the vault's agent");
    no("agent of slot 1 pokes slot 0", t.call("thief.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "Only the vault's agent");
    no("agent of slot 1 closes its own slot (no exit rule)", t.call("thief.sandbox", G, "exit", json!({ "user": "alice.sandbox#1" }), zero).await, "has not allowed the agent to exit");
    no("agent of slot 1 moves slot 1 into slot 0's pool", t.call("thief.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox#1", "request": far }), zero).await, "not in the owner's allowed list");
    no("agent of slot 0 touches slot 1", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox#1" }), zero).await, "Only the vault's agent");
    for bad in ["alice.sandbox#0", "alice.sandbox#3", "alice.sandbox#01", "alice.sandbox#1#1", "bob.sandbox"] {
        no("a spelling that is no record", t.call("thief.sandbox", G, "exit", json!({ "user": bad }), zero).await, "has not joined");
    }
    no("bob withdraws from slot 2", t.call("bob.sandbox", G, "withdraw", json!({ "token": TKQ, "slot": 2 }), zero).await, "has not joined");
    no("thief cashes out alice", t.call("thief.sandbox", G, "cash_out", json!({ "leave": false, "slot": 1 }), zero).await, "has not joined");
    no("thief sets its own agent on slot 1", t.call("thief.sandbox", G, "set_agent", json!({ "agent": "thief.sandbox", "slot": 1 }), zero).await, "has not joined");
    assert_eq!(futures_all(&t, &["alice.sandbox", "alice.sandbox#1", "alice.sandbox#2", "bob.sandbox#1"]).await, before, "no record changed");

    println!("\n--- the agent works one slot, the others stay ---");
    let tkq_before = t.ft(TKQ, "lp.sandbox").await;
    let sell = json!({ "Swap": { "pool_ids": [POOL2], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    ok("a whale sells in the 0.2% pool only", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "3000000000000", "msg": sell }), yocto).await);
    assert!(t.ft(TKQ, "lp.sandbox").await < tkq_before);
    let p = t.point(POOL2).await;
    assert!(p < c2 - 800, "pushed out of slot 1's range: {p}");
    let left = (p + 20).div_euclid(200) * 200 + 200;
    let fair = json!({ "pool_id": POOL2, "left_point": left, "right_point": left + 1400 });
    ok("agent pokes slot 1", t.call("thief.sandbox", G, "poke", json!({ "user": "alice.sandbox#1" }), zero).await);
    wait_secs(&sandbox, 200).await;
    ok("agent pokes slot 1 again", t.call("thief.sandbox", G, "poke", json!({ "user": "alice.sandbox#1" }), zero).await);
    no("slot 0 has no read of its own", t.call("agent.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox", "request": far }), zero).await, "REFUSED");
    wait_secs(&sandbox, 200).await;
    ok("agent re-centres slot 1 by its rules", t.call("thief.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox#1", "request": fair }), zero).await);
    assert_eq!(t.acct("alice.sandbox#1").await["position"]["left_point"], json!(left));
    assert_eq!(t.acct("alice.sandbox#1").await["rate"]["used"], json!(1));
    assert_eq!(t.acct("alice.sandbox").await["position"], before[0]["position"], "slot 0 did not move");
    assert_eq!(t.acct("alice.sandbox").await["rate"]["used"], json!(0), "slot 0's action count is its own");
    t.books("after the agent's move").await;

    println!("\n--- fees, then ways out, one slot at a time ---");
    let sell1 = json!({ "Swap": { "pool_ids": [POOL], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    let buy1 = json!({ "Swap": { "pool_ids": [POOL], "output_token": TKQ, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    ok("volume in the 1% pool", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "20000000000", "msg": sell1 }), yocto).await);
    ok("volume back", t.call("lp.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": (E24 / 5).to_string(), "msg": buy1 }), yocto).await);
    let slot2_ledger = t.held("alice.sandbox#2", TKQ).await;
    let w = t.ft(TKQ, "alice.sandbox").await;
    ok("alice takes slot 0's fees to her wallet", t.call("alice.sandbox", G, "claim_fees", json!({ "send": true }), zero).await);
    assert!(t.ft(TKQ, "alice.sandbox").await >= w, "fees arrived");
    assert_eq!(t.held("alice.sandbox#2", TKQ).await, slot2_ledger, "slot 2's ledger untouched");
    assert!(t.acct("alice.sandbox").await["position"].is_object(), "slot 0 stays open");
    t.books("after claim_fees").await;

    ok("owner pauses", t.call("owner.sandbox", G, "set_paused", json!({ "paused": true }), zero).await);
    ok("alice freezes slot 2's agent", t.call("alice.sandbox", G, "set_frozen", json!({ "frozen": true, "slot": 2 }), zero).await);
    let (nw, tw) = (t.near("alice.sandbox").await, t.ft(TKQ, "alice.sandbox").await);
    ok("paused and frozen: cash out slot 2", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true, "slot": 2 }), zero).await);
    println!("{:<58} +{:.4} NEAR, +{} TKQ", "alice received", t.near("alice.sandbox").await - nw, t.ft(TKQ, "alice.sandbox").await - tw);
    assert!(t.near("alice.sandbox").await - nw > 0.8, "NEAR arrived");
    assert!(t.ft(TKQ, "alice.sandbox").await > tw, "TKQ arrived");
    assert!(t.acct("alice.sandbox#2").await.is_null(), "slot 2 is gone");
    assert_eq!(t.view(G, "get_slots", json!({ "user": "alice.sandbox" })).await, json!([0, 1]));
    assert_eq!(t.acct("alice.sandbox").await["position"], before[0]["position"], "slot 0 in the same pool stays");
    assert_eq!(t.acct("bob.sandbox#1").await["position"], before[3]["position"], "bob in the same pool stays");
    t.books("after slot 2 left, paused").await;
    ok("owner resumes", t.call("owner.sandbox", G, "set_paused", json!({ "paused": false }), zero).await);

    println!("\n--- a wallet that refuses a token, on a slot ---");
    ok("carol joins slot 2", t.call("carol.sandbox", G, "join", json!({ "policy": policy(POOL), "agent": null, "slot": 2 }), join).await);
    ok("carol deposits TKQ", t.call("carol.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": tkq_in.to_string(), "msg": "2" }), yocto).await);
    let held = t.ft(TKQ, "carol.sandbox").await;
    ok("carol moves her TKQ away", t.call("carol.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": held.to_string() }), yocto).await);
    ok("carol unregisters from TKQ", t.call("carol.sandbox", TKQ, "storage_unregister", json!({ "force": false }), yocto).await);
    no("carol withdraws: her wallet refuses", t.call("carol.sandbox", G, "withdraw", json!({ "token": TKQ, "slot": 2 }), zero).await, "not registered");
    let owed = t.acct("carol.sandbox#2").await["owed"].clone();
    println!("{:<58} {owed}", "owed on slot 2");
    assert_eq!(owed[0][1], json!(tkq_in.to_string()));
    assert_eq!(t.view(G, "get_owed_total", json!({ "token": TKQ })).await, json!(tkq_in.to_string()));
    ok("carol registers again", t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": "carol.sandbox", "registration_only": true }), reg).await);
    ok("carol claims on slot 2", t.call("carol.sandbox", G, "claim", json!({ "token": TKQ, "slot": 2 }), zero).await);
    assert_eq!(t.ft(TKQ, "carol.sandbox").await, tkq_in, "carol has it all");
    assert_eq!(t.view(G, "get_owed_total", json!({ "token": TKQ })).await, json!("0"));
    ok("carol leaves slot 2", t.call("carol.sandbox", G, "leave", json!({ "slot": 2 }), zero).await);
    t.books("after carol").await;

    println!("\n--- a tax token on a slot ---");
    ok("alice joins slot 2 again (TAX pool)", t.call("alice.sandbox", G, "join", json!({ "policy": policy(POOL_TAX), "agent": null, "slot": 2 }), join).await);
    let tax0 = t.ft(TAX, "alice.sandbox").await;
    ok("deposit 100 TAX to slot 2", t.call("alice.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": G, "amount": (100 * E24).to_string(), "msg": "2" }), yocto).await);
    ok("deposit wNEAR to slot 2", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": (E24 * 100 / 100).to_string(), "msg": "2" }), yocto).await);
    let cp = centre(t.point(POOL_TAX).await);
    ok("open on the tax pool", t.call("alice.sandbox", G, "open", json!({ "left_point": cp - 600, "right_point": cp + 800, "slot": 2 }), zero).await);
    t.books("tax token open").await;
    ok("cash out the tax slot", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true, "slot": 2 }), zero).await);
    let back = t.ft(TAX, "alice.sandbox").await + (100 * E24) - tax0;
    println!("{:<58} {:.4} of 100 TAX", "back in the wallet", back as f64 / E24 as f64);
    assert!(back > 97 * E24, "tax took about 1% each way, the rest came back");
    assert_eq!(t.ft(TAX, G).await, 0, "nothing left in the contract's wallet");
    assert_eq!(t.view(G, "get_owed_total", json!({ "token": TAX })).await, json!("0"));
    t.books("after the tax slot").await;

    println!("\n--- gas: slot 1, cash out from 300 down ---");
    for tgas in [100u64, 150, 200, 250, 300] {
        let r = t.call_gas("alice.sandbox", G, "cash_out", json!({ "leave": false, "slot": 1 }), zero, tgas).await;
        println!("{:<58} {r}", format!("cash_out at {tgas} TGas"));
        wait_secs(&sandbox, 125).await; // a dead chain's lock expires
        t.books(&format!("after cash_out at {tgas}")).await;
        let a = t.acct("alice.sandbox#1").await;
        let done = a["position"].is_null() && a["ledger"].as_array().map(|l| l.is_empty()).unwrap_or(true);
        if done {
            assert_eq!(tgas, 300, "floors: only a full wallet request gets through, nothing half done before it");
            break;
        }
    }
    ok("slot 1 leaves", t.call("alice.sandbox", G, "leave", json!({ "slot": 1 }), zero).await);

    println!("\n--- the way out, step by step, and the end ---");
    ok("alice exits slot 0", t.call("alice.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await);
    t.books("after exit").await;
    ok("withdraw TKQ", t.call("alice.sandbox", G, "withdraw", json!({ "token": TKQ }), zero).await);
    ok("withdraw wNEAR", t.call("alice.sandbox", G, "withdraw", json!({ "token": WRAP }), zero).await);
    ok("leave slot 0", t.call("alice.sandbox", G, "leave", json!({}), zero).await);
    assert_eq!(t.view(G, "get_user_count", json!({})).await, json!(1), "alice is gone");
    ok("bob cashes out and leaves", t.call("bob.sandbox", G, "cash_out", json!({ "leave": true, "slot": 1 }), zero).await);
    assert_eq!(t.view(G, "get_users", json!({})).await, json!([]));
    t.books("the end").await;
    let rhea = t.view(DCL, "list_user_assets", json!({ "account_id": G })).await;
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"), "nothing left on Rhea: {rhea}");
    assert_eq!(t.view(DCL, "list_liquidities", json!({ "account_id": G })).await, json!([]));
    for token in [TKQ, TAX, WRAP] {
        assert_eq!(t.ft(token, G).await, 0, "contract wallet {token}");
        assert_eq!(t.view(G, "get_owed_total", json!({ "token": token })).await, json!("0"));
    }
    println!("\nall slots empty, nothing stranded");
    Ok(())
}

async fn futures_all(t: &T, who: &[&str]) -> Vec<Value> {
    let mut out = Vec::new();
    for w in who {
        out.push(t.acct(w).await);
    }
    out
}
