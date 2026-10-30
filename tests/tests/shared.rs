//! The shared Guardian contract on the real Rhea DCL 2.4.0 (mainnet wasm), two users at once:
//! join → one deposit that opens the position → a swap moves the price → the agent rebalances
//! one user, the other is untouched → Alice cashes out in one approval; Carol's wallet refuses a token
//! (owed, then claimed); Bob exits and withdraws step by step → Rhea balance and wallets end at zero.
//! Every call from a wallet carries 300 TGas. Run ./scripts/build.sh first.

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
async fn shared_guardian_on_real_dcl() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "bob", "carol"] {
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

    // Alice and Bob each hold TKQ and wNEAR.
    for who in ["alice.sandbox", "bob.sandbox"] {
        t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": "2000000000000000" }), yocto).await;
        t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(4)).await;
    }
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let join = NearToken::from_millinear(10);
    let base = t.storage(G).await;
    println!("contract storage before any user: {base} B");
    ok("alice join", t.call("alice.sandbox", G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), join).await);
    ok("bob join", t.call("bob.sandbox", G, "join", json!({ "policy": policy, "agent": "agent.sandbox" }), join).await);
    let joined = t.storage(G).await;
    println!("after 2 joins: {} B ({} B per user)", joined, (joined - base) / 2);
    fails("join twice", t.call("bob.sandbox", G, "join", json!({ "policy": policy, "agent": null }), join).await, "Already joined");

    // Alice: TKQ and wNEAR deposits (each measured on Rhea, v1.5), then she opens her range across the
    // price (the page sends all three in one wallet request).
    ok("alice deposit TKQ", t.call("alice.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "100000000000", "msg": "" }), yocto).await);
    ok("alice deposit wNEAR", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "2000000000000000000000000", "msg": "" }), yocto).await);
    let ranged = json!({ "left_point": 303400, "right_point": 306200 }).to_string();
    fails("a deposit that names a range", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "1000000000000000000000", "msg": ranged }), yocto).await, "empty msg");
    ok("alice opens", t.call("alice.sandbox", G, "open", json!({ "left_point": 303400, "right_point": 306200 }), zero).await);
    let (pos, led) = t.ledger("alice.sandbox").await;
    println!("alice: position {} ledger {:?}", pos.as_ref().map(|p| p["lpt_id"].to_string()).unwrap_or("none".into()), led);
    assert!(pos.is_some(), "alice has a position");
    let alice_leftover_wnear = led.iter().find(|(t, _)| t == WRAP).map(|(_, a)| *a).unwrap_or(0);
    assert!(alice_leftover_wnear > 0, "the add's wNEAR leftover is back on her ledger: {led:?}");

    // Bob: wNEAR only, one-sided below the price (deposit, then open).
    ok("bob deposit wNEAR", t.call("bob.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "1000000000000000000000000", "msg": "" }), yocto).await);
    ok("bob opens (one side)", t.call("bob.sandbox", G, "open", json!({ "left_point": 303200, "right_point": 304600 }), zero).await);
    let (bpos, bled) = t.ledger("bob.sandbox").await;
    println!("bob: position {} ledger {:?}", bpos.as_ref().map(|p| p["lpt_id"].to_string()).unwrap_or("none".into()), bled);
    assert!(bpos.is_some());
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": G })).await;
    assert_eq!(positions.as_array().map(|a| a.len()), Some(2), "two positions on Rhea: {positions}");
    let full = t.storage(G).await;
    println!("with 2 positions and ledgers: {} B ({} B per user, {:.4} NEAR at 1e19 yocto/B)", full, (full - base) / 2, ((full - base) / 2) as f64 * 1e19 / 1e24);
    println!("contract's Rhea storage with 2 positions: {}", t.view("dcl.sandbox", "get_user_storage_detail", json!({ "user_id": G })).await);

    // Nobody else can touch them.
    fails("bob cannot exit alice", t.call("bob.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "Only the vault's agent");
    fails("stranger cannot rebalance", t.call("lp.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": 303400, "right_point": 306200 } }), zero).await, "Only the vault's agent");
    fails("agent cannot churn in range", t.call("agent.sandbox", G, "request_rebalance", json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": 303600, "right_point": 306400 } }), zero).await, "not far enough outside");
    fails("agent cannot exit without a rule", t.call("agent.sandbox", G, "exit", json!({ "user": "alice.sandbox" }), zero).await, "has not allowed the agent to exit");

    // A real swap pushes the price below Alice's range (and into Bob's).
    let sell = json!({ "Swap": { "pool_ids": [POOL], "output_token": WRAP, "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    ok("swap TKQ -> wNEAR", t.call("lp.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "2000000000000", "msg": sell }), yocto).await);
    let point = t.view("dcl.sandbox", "get_pool", json!({ "pool_id": POOL })).await["current_point"].as_i64().unwrap();
    println!("price point after swap: {point}");
    assert!(point < 303400 - 200, "{point}");

    // The agent re-enters Alice one-sided just above the price; Bob's ledger and position are untouched.
    let bob_before = t.ledger("bob.sandbox").await;
    let left = (point + 20).div_euclid(200) * 200 + 200;
    let move_alice = json!({ "user": "alice.sandbox", "request": { "pool_id": POOL, "left_point": left, "right_point": left + 1400 } });
    // v1.3: not on one moment's price. No earlier read: refused; read just now: refused; read more
    // than 3 minutes ago with the price already outside then: allowed.
    fails("agent cannot move on a price never read", t.call("agent.sandbox", G, "request_rebalance", move_alice.clone(), zero).await, "3 to 60 minutes");
    ok("agent pokes alice (the contract reads the price)", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await);
    fails("agent cannot move on a price read just now", t.call("agent.sandbox", G, "request_rebalance", move_alice.clone(), zero).await, "3 to 60 minutes");
    wait_secs(&sandbox, &t, 200).await;
    ok("agent rebalances alice", t.call("agent.sandbox", G, "request_rebalance", move_alice, zero).await);
    let (apos, aled) = t.ledger("alice.sandbox").await;
    println!("alice after rebalance: position {:?} ledger {:?}", apos.as_ref().map(|p| (p["left_point"].clone(), p["right_point"].clone())), aled);
    assert_eq!(apos.as_ref().map(|p| p["left_point"].as_i64().unwrap()), Some(left));
    assert_eq!(t.ledger("bob.sandbox").await, bob_before, "bob untouched");

    // The agent pokes Alice's mark (exit rule bookkeeping); a stranger cannot.
    ok("agent pokes alice", t.call("agent.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await);
    fails("stranger cannot poke", t.call("lp.sandbox", G, "poke", json!({ "user": "alice.sandbox" }), zero).await, "Only the vault's agent");

    // Alice: the whole way out in ONE approval, from an open position: close, TKQ and NEAR to her
    // wallet, record removed, join deposit back.
    let (near_before, tkq_before) = (t.near("alice.sandbox").await, t.ft(TKQ, "alice.sandbox").await);
    let alice_wnear = t.ledger("alice.sandbox").await.1.iter().find(|(t, _)| t == WRAP).map(|(_, a)| *a).unwrap_or(0) as f64 / 1e24;
    ok("alice cash_out(leave) - one approval", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    let (got_near, got_tkq) = (t.near("alice.sandbox").await - near_before, t.ft(TKQ, "alice.sandbox").await - tkq_before);
    println!("alice got {got_near:.4} NEAR and {got_tkq} TKQ in one approval");
    assert!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await.is_null(), "alice left");
    // Her range sat above the price (all TKQ); the NEAR is her wNEAR leftover plus the join deposit, less gas.
    assert!((got_near - (alice_wnear + 0.01)).abs() < 0.01 && got_tkq > 0, "both tokens arrived: {got_near} vs {}", alice_wnear + 0.01);

    // Carol: her wallet refuses TKQ (unregistered) during cash_out: NEAR still arrives, TKQ is owed,
    // the record stays; after registering she claims it and leaves.
    let point = t.view("dcl.sandbox", "get_pool", json!({ "pool_id": POOL })).await["current_point"].as_i64().unwrap();
    t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": "carol.sandbox", "registration_only": true }), NearToken::from_millinear(125)).await;
    t.call("owner.sandbox", WRAP, "storage_deposit", json!({ "account_id": "carol.sandbox", "registration_only": true }), NearToken::from_millinear(125)).await;
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "carol.sandbox", "amount": "100000000000" }), yocto).await;
    t.call("carol.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(2)).await;
    ok("carol join", t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), join).await);
    ok("carol deposit TKQ", t.call("carol.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": "100000000000", "msg": "" }), yocto).await);
    let cl = point.div_euclid(200) * 200 - 600;
    ok("carol deposit wNEAR", t.call("carol.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "1000000000000000000000000", "msg": "" }), yocto).await);
    ok("carol opens", t.call("carol.sandbox", G, "open", json!({ "left_point": cl, "right_point": cl + 1400 }), zero).await);
    assert!(t.ledger("carol.sandbox").await.0.is_some(), "carol has a position");
    ok("carol unregisters from TKQ", t.call("carol.sandbox", TKQ, "storage_unregister", json!({ "force": false }), yocto).await);
    let near_before = t.near("carol.sandbox").await;
    println!("{:<42} {}", "carol cash_out(leave), TKQ refused", t.call("carol.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    let c = t.view(G, "get_account", json!({ "user": "carol.sandbox" })).await;
    println!("carol after: position {} ledger {} owed {}", c["position"], c["ledger"], c["owed"]);
    assert!(c["position"].is_null() && c["ledger"].as_array().unwrap().is_empty(), "closed and paid out");
    let owed: u128 = c["owed"][0][1].as_str().unwrap().parse()?;
    assert_eq!(c["owed"][0][0], TKQ);
    assert!(t.near("carol.sandbox").await - near_before > 0.5, "NEAR arrived despite the TKQ refusal");
    assert_eq!(t.ft(TKQ, G).await, owed, "the owed TKQ sits in the contract's own wallet, exactly");
    fails("carol cannot leave while owed", t.call("carol.sandbox", G, "leave", json!({}), zero).await, "withdraw first");
    t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": "carol.sandbox", "registration_only": true }), NearToken::from_millinear(125)).await;
    ok("carol claim TKQ", t.call("carol.sandbox", G, "claim", json!({ "token": TKQ }), zero).await);
    assert_eq!(t.ft(TKQ, "carol.sandbox").await, owed, "claimed in full");
    ok("carol leave", t.call("carol.sandbox", G, "leave", json!({}), zero).await);

    // Bob turns on an exit rule (2000 points against him); the price fell ~13,700 points since his
    // range opened, so the agent may close it. Then the agent may NOT re-open from his idle tokens.
    let mut bob_policy = policy.clone();
    bob_policy["exit_drop_points"] = json!(2000);
    ok("bob sets an exit rule", t.call("bob.sandbox", G, "set_policy", json!({ "policy": bob_policy }), zero).await);
    fails("agent cannot exit bob on a price never read", t.call("agent.sandbox", G, "exit", json!({ "user": "bob.sandbox" }), zero).await, "3 to 60 minutes");
    ok("agent pokes bob", t.call("agent.sandbox", G, "poke", json!({ "user": "bob.sandbox" }), zero).await);
    wait_secs(&sandbox, &t, 200).await;
    ok("agent exits bob by his rule", t.call("agent.sandbox", G, "exit", json!({ "user": "bob.sandbox" }), zero).await);
    assert!(t.ledger("bob.sandbox").await.0.is_none(), "bob's position closed by the agent");
    fails("agent cannot re-open after an exit", t.call("agent.sandbox", G, "request_rebalance", json!({ "user": "bob.sandbox", "request": { "pool_id": POOL, "left_point": 290000, "right_point": 291400 } }), zero).await, "only the user opens one");

    // Bob: the step-by-step way (each withdraw, then leave) still works.

    // Withdrawals: wNEAR arrives as NEAR, TKQ as TKQ; ledgers end at zero; leave returns the deposit.
    for who in ["bob.sandbox"] {
        let (_, led) = t.ledger(who).await;
        let near_before = t.near(who).await;
        let tkq_before = t.ft(TKQ, who).await;
        for (tok, amt) in &led {
            ok(&format!("{who} withdraw {tok}"), t.call(who, G, "withdraw", json!({ "token": tok, "amount": null }), zero).await);
            let _ = amt;
        }
        let (_, after) = t.ledger(who).await;
        assert!(after.is_empty(), "{who} ledger empty: {after:?}");
        let wn = led.iter().find(|(t, _)| t == WRAP).map(|(_, a)| *a).unwrap_or(0) as f64 / 1e24;
        let tq = led.iter().find(|(t, _)| t == TKQ).map(|(_, a)| *a).unwrap_or(0);
        let got_near = t.near(who).await - near_before;
        let got_tkq = t.ft(TKQ, who).await - tkq_before;
        println!("{who}: got {got_near:.4} NEAR (ledger {wn:.4}), {got_tkq} TKQ (ledger {tq})");
        assert!((got_near - wn).abs() < 0.01, "NEAR arrived");
        assert_eq!(got_tkq, tq, "TKQ arrived");
        ok(&format!("{who} leave"), t.call(who, G, "leave", json!({}), zero).await);
        assert!(t.view(G, "get_account", json!({ "user": who })).await.is_null());
    }
    let rhea = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": G })).await;
    println!("contract's Rhea balance after everyone left: {rhea}");
    // The ledger is exact: everything users put in went back out, nothing is left unassigned.
    for (tok, v) in rhea.as_object().cloned().unwrap_or_default() {
        let left: u128 = v.as_str().unwrap().parse().unwrap();
        assert_eq!(left, 0, "{tok} left on the contract: {left}");
    }
    // And nothing stranded in the contract's own token wallets.
    assert_eq!((t.ft(TKQ, G).await, t.ft(WRAP, G).await), (0, 0), "contract wallets empty");
    Ok(())
}
