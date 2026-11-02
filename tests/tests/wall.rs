//! The outer wall in front of Guardian, on the real Rhea wasm: stolen owner key, undo, the way home,
//! the agent's limits and the bait key. Run ./scripts/build.sh first.

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
async fn the_outer_wall_holds() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "agent", "guardian", "alice", "bob", "carol", "dave", "thief", "shop", "wall"] {
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

    const W: &str = "wall.sandbox";
    const HOME: &str = "dave.sandbox";
    const THIEF: &str = "thief.sandbox";
    const SHOP: &str = "shop.sandbox";
    const CANCELLER: &str = "carol.sandbox";
    deploy(W, std::fs::read(format!("{}/../target/near/kinu_outer_wall_short/kinu_outer_wall.wasm", env!("CARGO_MANIFEST_DIR")))?, json!({ "owner": "alice.sandbox", "canceller": CANCELLER, "home": HOME, "guardian": G, "delay_secs": 3600 })).await;
    for tok in [TKQ, WRAP] {
        for who in [W, HOME, THIEF, SHOP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
        }
    }
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": W, "amount": "2000000000000000" }), yocto).await;
    t.call("lp.sandbox", WRAP, "ft_transfer", json!({ "receiver_id": W, "amount": "3000000000000000000000000" }), yocto).await;
    let a = "alice.sandbox"; // the owner key: from scene 2 on, the thief holds it
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let guardian = |method: &str, args: Value| json!({ "method": method, "args": args.to_string() });
    let at_once = |method: &str, args: Value| json!({ "method": method, "args": args.to_string() });
    let req = |action: Value| json!({ "action": action });
    let send_to = |to: &str, amount: &str| req(json!({ "Send": { "token": TKQ, "to": to, "amount": amount } }));
    let _ = guardian;

    println!("--- 1. Setup: a rule change waits like a payment");
    fails("join at once", t.call(a, W, "guardian_call", at_once("join", json!({ "policy": policy, "agent": "agent.sandbox" })), zero).await, "must wait");
    let join_args = json!({ "policy": policy, "agent": "agent.sandbox" }).to_string();
    ok("request join (0)", t.call(a, W, "request", req(json!({ "Guardian": { "method": "join", "args": join_args, "deposit": NearToken::from_millinear(10).as_yoctonear().to_string() } })), zero).await);
    ok("request the wall's agent (1)", t.call(a, W, "request", req(json!({ "SetAgent": { "agent": "agent.sandbox", "receivers": [SHOP], "limits": [{ "token": TKQ, "per_day": "1000" }] } })), zero).await);
    fails("too early", t.call("lp.sandbox", W, "execute", json!({ "id": 0 }), zero).await, "Still waiting");
    wait_secs(&sandbox, &t, 3610).await;
    ok("execute join (anyone)", t.call("lp.sandbox", W, "execute", json!({ "id": 0 }), zero).await);
    ok("execute agent", t.call("lp.sandbox", W, "execute", json!({ "id": 1 }), zero).await);
    assert!(t.view(G, "get_account", json!({ "user": W })).await.is_object(), "the wall joined");
    ok("deposit TKQ", t.call(a, W, "guardian_deposit", json!({ "token": TKQ, "amount": "100000000000", "msg": "" }), zero).await);
    ok("deposit wNEAR", t.call(a, W, "guardian_deposit", json!({ "token": WRAP, "amount": "2000000000000000000000000", "msg": "" }), zero).await);
    fails("open at once", t.call(a, W, "guardian_call", at_once("open", json!({ "left_point": 303400, "right_point": 306200 })), zero).await, "must wait");
    let open_args = json!({ "left_point": 303400, "right_point": 306200 }).to_string();
    ok("request open (2)", t.call(a, W, "request", req(json!({ "Guardian": { "method": "open", "args": open_args, "deposit": "0" } })), zero).await);
    wait_secs(&sandbox, &t, 3610).await;
    ok("execute open", t.call("lp.sandbox", W, "execute", json!({ "id": 2 }), zero).await);
    assert!(t.ledger(W).await.0.is_some(), "a position behind the wall");

    println!("--- 2. The owner key is stolen");
    let keys = near_api::Account(W.parse()?).list_keys().fetch_from(&net).await?.data;
    println!("{:<42} {}", "keys on the wall", keys.len());
    assert!(keys.is_empty(), "nobody holds the wall's own account");
    ok("thief asks to pay himself (3)", t.call(a, W, "request", send_to(THIEF, "500000"), zero).await);
    fails("thief cannot run it early", t.call(a, W, "execute", json!({ "id": 3 }), zero).await, "Still waiting");
    fails("a stranger cannot cancel", t.call("bob.sandbox", W, "cancel", json!({ "id": 3 }), zero).await, "Only the owner, the canceller or the sentinel");
    ok("UNDO: the canceller cancels", t.call(CANCELLER, W, "cancel", json!({ "id": 3 }), zero).await);
    fails("nothing left to run", t.call(a, W, "execute", json!({ "id": 3 }), zero).await, "No such request");
    assert_eq!(t.ft(TKQ, THIEF).await, 0, "the thief got nothing");
    fails("thief cannot widen the rules at once", t.call(a, W, "guardian_call", at_once("set_policy", json!({ "policy": policy })), zero).await, "must wait");
    fails("thief cannot change the agent at once", t.call(a, W, "guardian_call", at_once("set_agent", json!({ "agent": THIEF })), zero).await, "must wait");
    fails("home cannot be the owner", t.call(a, W, "request", req(json!({ "SetHome": { "home": a } })), zero).await, "third account");
    fails("home cannot be the canceller", t.call(a, W, "request", req(json!({ "SetHome": { "home": CANCELLER } })), zero).await, "third account");
    fails("the canceller cannot be home", t.call(a, W, "request", req(json!({ "SetCanceller": { "canceller": HOME } })), zero).await, "third account");
    ok("thief asks for a new home (4)", t.call(a, W, "request", req(json!({ "SetHome": { "home": THIEF } })), zero).await);
    ok("thief asks for a shorter delay (5)", t.call(a, W, "request", req(json!({ "SetDelay": { "delay_secs": 3600 } })), zero).await);
    ok("thief asks for a new canceller (6)", t.call(a, W, "request", req(json!({ "SetCanceller": { "canceller": THIEF } })), zero).await);
    for id in [4, 5, 6] {
        ok("the canceller cancels", t.call(CANCELLER, W, "cancel", json!({ "id": id }), zero).await);
    }
    assert_eq!(t.view(W, "get_config", json!({})).await["home"], HOME);
    let (tkq, thief_near) = (t.ft(TKQ, W).await, t.near(THIEF).await);
    ok("thief cashes out of Guardian", t.call(a, W, "guardian_call", at_once("cash_out", json!({ "leave": false })), zero).await);
    assert!(t.ledger(W).await.0.is_none(), "position closed");
    assert!(t.ft(TKQ, W).await > tkq, "the money landed on the wall");
    assert_eq!((t.ft(TKQ, THIEF).await, t.near(THIEF).await), (0, thief_near), "not on the thief");

    println!("--- 3. Nobody watching: the wait is a window, not a vault");
    ok("thief asks again (7)", t.call(a, W, "request", send_to(THIEF, "7"), zero).await);
    wait_secs(&sandbox, &t, 3610).await;
    ok("thief runs it after the wait", t.call(a, W, "execute", json!({ "id": 7 }), zero).await);
    assert_eq!(t.ft(TKQ, THIEF).await, 7, "an uncancelled request goes through");

    ok("close while still in Guardian (8)", t.call(a, W, "request", req(json!("Close")), zero).await);
    wait_secs(&sandbox, &t, 3610).await;
    ok("it runs", t.call(a, W, "execute", json!({ "id": 8 }), zero).await);
    assert!(t.view(W, "get_config", json!({})).await.is_object(), "the wall stands: Guardian still holds a record");

    println!("--- 4. The way home");
    ok("owner sends home", t.call(a, W, "send_home", json!({ "token": TKQ, "amount": "1000" }), zero).await);
    ok("the canceller sends home", t.call(CANCELLER, W, "send_home", json!({ "token": TKQ, "amount": "1000" }), zero).await);
    assert_eq!(t.ft(TKQ, HOME).await, 2000, "both arrived at home");
    fails("the canceller has no other road", t.call(CANCELLER, W, "request", send_to(CANCELLER, "1"), zero).await, "Only the owner");
    fails("nor Guardian", t.call(CANCELLER, W, "guardian_call", at_once("withdraw", json!({ "token": TKQ })), zero).await, "Only the owner");

    println!("--- 5. The agent holds money, not the keys");
    let pay = |to: &str, amount: &str| json!({ "token": TKQ, "to": to, "amount": amount });
    ok("agent pays the shop", t.call("agent.sandbox", W, "agent_send", pay(SHOP, "600"), zero).await);
    assert_eq!(t.ft(TKQ, SHOP).await, 600);
    fails("injected agent pays the thief", t.call("agent.sandbox", W, "agent_send", pay(THIEF, "1"), zero).await, "not on the owner's list");
    fails("another token has no limit", t.call("agent.sandbox", W, "agent_send", json!({ "token": WRAP, "to": SHOP, "amount": "1" }), zero).await, "No limit");
    ok("over the day's limit: it waits (9)", t.call("agent.sandbox", W, "agent_send", pay(SHOP, "401"), zero).await);
    assert_eq!(t.ft(TKQ, SHOP).await, 600, "not paid yet");
    assert_eq!(t.view(W, "get_pending", json!({})).await[0]["by_agent"], true);
    ok("the canceller stops the agent's payment", t.call(CANCELLER, W, "cancel", json!({ "id": 9 }), zero).await);
    fails("a stranger is no agent", t.call("bob.sandbox", W, "agent_send", pay(SHOP, "1"), zero).await, "Only the agent");

    println!("--- 6. The bait key");
    let bait_key = near_api::signer::generate_secret_key()?;
    let bait_pk = bait_key.public_key();
    ok("owner plants the bait", t.call(a, W, "add_tripwire", json!({ "public_key": bait_pk.to_string() }), zero).await);
    ok("request before the trip (10)", t.call(a, W, "request", send_to(THIEF, "1"), zero).await);
    assert!(t.view(G, "get_account", json!({ "user": W })).await["frozen"] == false);
    let stolen = T { net: net.clone(), signer: Signer::from_secret_key(bait_key)? };
    // The freeze also tries slots 1 and 2, which this wall never opened: those two receipts fail, by design.
    println!("{:<42} {}", "thief signs with the bait", stolen.call(W, W, "agent_send", pay(SHOP, "1"), zero).await);
    assert_eq!(t.view(W, "get_config", json!({})).await["frozen"], true, "the wall froze itself");
    assert_eq!(t.view(G, "get_account", json!({ "user": W })).await["frozen"], true, "and the agent on Guardian");
    assert_eq!(t.ft(TKQ, SHOP).await, 600, "the bait moved nothing");
    println!("{:<42} {}", "another bait call", stolen.call(W, W, "agent_send", pay(SHOP, "1"), zero).await);
    assert_eq!(t.view(W, "get_pending", json!({})).await.as_array().map(|p| p.len()), Some(0), "the freeze emptied the queue");
    fails("frozen: no new request", t.call(a, W, "request", send_to(THIEF, "1"), zero).await, "frozen");
    fails("frozen: no new position", t.call(a, W, "guardian_call", at_once("open", json!({ "left_point": 303400, "right_point": 306200 })), zero).await, "must wait");
    fails("frozen: the agent stops", t.call("agent.sandbox", W, "agent_send", pay(SHOP, "1"), zero).await, "frozen");
    wait_secs(&sandbox, &t, 3610).await;
    fails("frozen: the waiting request is gone", t.call(a, W, "execute", json!({ "id": 10 }), zero).await, "No such request");
    ok("frozen: the way home still works", t.call(a, W, "send_home", json!({ "token": TKQ, "amount": "5" }), zero).await);
    assert_eq!(t.ft(TKQ, HOME).await, 2005);
    ok("frozen: leaving Guardian still works", t.call(a, W, "guardian_call", at_once("leave", json!({})), zero).await);
    fails("the owner alone cannot unfreeze at once", t.call(a, W, "unfreeze", json!({}), zero).await, "Only the canceller");
    ok("the owner asks, the long wait (11)", t.call(a, W, "request", req(json!("Unfreeze")), zero).await);
    fails("not yet", t.call(a, W, "execute", json!({ "id": 11 }), zero).await, "Still waiting");
    wait_secs(&sandbox, &t, 7210).await; // 7 days in the deployed build
    ok("a lost canceller strands nothing", t.call(a, W, "execute", json!({ "id": 11 }), zero).await);
    assert_eq!(t.view(W, "get_config", json!({})).await["frozen"], false);

    println!("--- 7. The canceller's own freeze, and closing the wall");
    fails("a stranger cannot freeze", t.call("bob.sandbox", W, "freeze", json!({}), zero).await, "Only the owner, the canceller or the sentinel");
    println!("{:<42} {}", "the canceller freezes", t.call(CANCELLER, W, "freeze", json!({}), zero).await);
    assert_eq!(t.view(W, "get_config", json!({})).await["frozen"], true);
    ok("the canceller lifts it at once", t.call(CANCELLER, W, "unfreeze", json!({}), zero).await);
    ok("owner sends the tokens home", t.call(a, W, "send_home", json!({ "token": TKQ, "amount": t.ft(TKQ, W).await.to_string() }), zero).await);
    ok("owner sends the wNEAR home", t.call(a, W, "send_home", json!({ "token": WRAP, "amount": t.ft(WRAP, W).await.to_string() }), zero).await);
    assert_eq!((t.ft(TKQ, W).await, t.ft(WRAP, W).await), (0, 0), "nothing left in tokens");
    let (near_in_wall, home_near) = (t.near(W).await, t.near(HOME).await);
    ok("owner asks to close (12)", t.call(a, W, "request", req(json!("Close")), zero).await);
    wait_secs(&sandbox, &t, 3610).await;
    ok("the wall closes", t.call(a, W, "execute", json!({ "id": 12 }), zero).await);
    assert!(t.near(HOME).await - home_near > near_in_wall - 0.1, "all its NEAR, storage too, went home");
    Ok(())
}
