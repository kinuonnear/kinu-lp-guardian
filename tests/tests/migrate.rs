//! v1.2 (as on mainnet, FHzvPc81) -> v1.3 in place, on the real Rhea DCL 2.4.0, with users inside
//! (Alice holds a position, Bob only a ledger); then the contract's own key is deleted and the only
//! way to change the code is the owner (a DAO) plus the 48-hour delay: stage, propose, a user leaves
//! meanwhile, a stranger runs it after the delay; records stay and the contract ends empty.
//! v1.3 here is the short-delay test build (10 minutes, feature short-delay; skipping 48 h of sandbox
//! blocks takes over an hour); the upgrade it runs installs the production build (48 h), which the
//! test checks at the end. Run ./scripts/build.sh first.

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
/// The guardian built with the 10-minute admin delay (scripts/build.sh).
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
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        let out = Contract(to.parse().unwrap()).call_function(method, args).transaction().gas(NearGas::from_tgas(300)).deposit(deposit).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
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
    async fn near(&self, id: &str) -> f64 {
        near_api::Tokens::account(id.parse().unwrap()).near_balance().fetch_from(&self.net).await.map(|b| b.total.as_yoctonear() as f64 / 1e24).unwrap_or(0.0)
    }
    /// The latest final block's time, seconds.
    async fn now(&self, rpc: &str) -> u64 {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "block", "params": { "finality": "final" } });
        let b: Value = reqwest::Client::new().post(rpc).json(&body).send().await.unwrap().json().await.unwrap();
        b["result"]["header"]["timestamp"].as_u64().unwrap() / 1_000_000_000
    }
}

fn ok(label: &str, r: String) {
    println!("{label:<44} {r}");
    assert_eq!(r, "ok", "{label}");
}

#[tokio::test]
async fn v12_to_v13_then_only_dao_and_delay() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "guardian", "alice", "bob", "carol", "dao"] {
        sandbox.create_account(format!("{name}.sandbox").parse::<AccountId>()?).initial_balance(NearToken::from_near(60)).send().await?;
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
    deploy("dcl.sandbox", kinu_guardian_tests::dcl_wasm(), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    t.call("owner.sandbox", "dcl.sandbox", "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await;
    t.call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 10000, "init_point": 304800 }), NearToken::from_near(1)).await;
    for who in ["dcl.sandbox", "lp.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        t.call("owner.sandbox", TKQ, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
        t.call("owner.sandbox", WRAP, "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125)).await;
    }
    t.call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await;
    t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "100000000000000000" }), yocto).await;
    t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(40)).await;
    for (tok, amt) in [(TKQ, "50000000000000000"), (WRAP, "30000000000000000000000000")] {
        t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), yocto).await;
    }
    ok("lp liquidity", t.call("lp.sandbox", "dcl.sandbox", "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": "20000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0" }), zero).await);

    // v1.2 exactly as deployed on mainnet (FHzvPc81), with Alice (position) and Bob (ledger only).
    deploy(G, fixture("shared-v1.2.wasm"), json!({ "dcl": "dcl.sandbox", "wnear": WRAP })).await;
    ok("register_dcl", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in [TKQ, WRAP] {
        ok("register_token", t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), NearToken::from_millinear(125)).await);
    }
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    for who in ["alice.sandbox", "bob.sandbox"] {
        t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(3)).await;
        ok(&format!("{who} joins v1.2"), t.call(who, G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await);
    }
    let open = json!({ "left_point": 303200, "right_point": 304600 }).to_string();
    ok("alice deposit + open on v1.2", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "1000000000000000000000000", "msg": open }), yocto).await);
    ok("bob deposit only on v1.2", t.call("bob.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "500000000000000000000000", "msg": "" }), yocto).await);
    let alice_before = t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await;
    let bob_before = t.view(G, "get_account", json!({ "user": "bob.sandbox" })).await;
    assert!(alice_before["position"].is_object());

    // 1. v1.2 -> v1.3 in place: the new code and migrate(owner) in one transaction, signed by the
    //    contract's own key (the last time it is used).
    let r = Contract::deploy(G.parse()?).use_code(built_short()).with_init_call("migrate", json!({ "owner": "dao.sandbox" }))?.with_signer(signer.clone()).send_to(&net).await?;
    assert!(r.is_success(), "upgrade + migrate: {r:?}");
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("v1.3 config {cfg}");
    assert_eq!(cfg["version"], "1.6.0");
    assert_eq!(cfg["owner"], "dao.sandbox");
    assert_eq!(cfg["users"], 2, "the v1.2 user list is kept");
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await, alice_before, "alice's record kept");
    assert_eq!(t.view(G, "get_account", json!({ "user": "bob.sandbox" })).await, bob_before, "bob's record kept");
    assert!(t.call("alice.sandbox", G, "migrate", json!({ "owner": "alice.sandbox" }), zero).await.starts_with("ERR"), "migrate is private");

    // 2. The contract's own key is deleted: no key can deploy code or move funds any more.
    let pk = signer.get_public_key().await?;
    let r = near_api::Account(G.parse()?).delete_key(pk).with_signer(signer.clone()).send_to(&net).await?;
    assert!(r.is_success(), "key deleted: {r:?}");
    let r = Contract::deploy(G.parse()?).use_code(built("kinu_guardian_shared")).without_init_call().with_signer(signer.clone()).send_to(&net).await;
    assert!(r.map(|r| !r.is_success()).unwrap_or(true), "a direct deploy is impossible now");
    println!("{:<44} refused (no key)", "direct deploy after the key is gone");

    // 3. An upgrade only through the owner and the delay: stage, propose, wait 48 h, anyone runs it.
    let stage = Contract(G.parse()?).call_function_raw("stage_code", built("kinu_guardian_shared")).transaction().gas(NearGas::from_tgas(300)).deposit(NearToken::from_near(5)).with_signer("lp.sandbox".parse()?, signer.clone()).send_to(&net).await?;
    assert!(stage.is_success(), "stage_code: {stage:?}");
    let staged = t.view(G, "get_config", json!({})).await["staged"].clone();
    println!("staged {staged}");
    let hash = staged["code_hash"].as_str().unwrap().to_string();
    let upgrade = json!({ "action": { "Upgrade": { "code_hash": hash } } });
    assert!(t.call("alice.sandbox", G, "propose", upgrade.clone(), zero).await.contains("Only the owner"));
    ok("dao proposes the upgrade", t.call("dao.sandbox", G, "propose", upgrade, zero).await);
    println!("pending {}", t.view(G, "get_pending", json!({})).await);
    let early = t.call("carol.sandbox", G, "execute", json!({ "id": 0 }), zero).await;
    println!("{:<44} {early}", "execute right away");
    assert!(early.contains("Too early"));
    // While it waits, every user can still leave: Bob cashes out.
    let bob_near = t.near("bob.sandbox").await;
    ok("bob cash_out(leave) during the delay", t.call("bob.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    assert!(t.near("bob.sandbox").await - bob_near > 0.45, "bob's wNEAR came back as NEAR");
    // After the delay (sandbox fast-forward), a stranger runs it.
    let delay = t.view(G, "get_config", json!({})).await["admin_delay_secs"].as_u64().unwrap();
    let start = t.now(&sandbox.rpc_addr).await;
    let mut waited = 0;
    while waited < delay + 30 {
        sandbox.fast_forward(500).await?;
        waited = t.now(&sandbox.rpc_addr).await - start;
    }
    println!("{:<44} +{waited} s (delay {delay} s)", "sandbox clock");
    let lp_near = t.near("lp.sandbox").await;
    ok("a stranger runs the upgrade after the delay", t.call("carol.sandbox", G, "execute", json!({ "id": 0 }), zero).await);
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("after the timelocked upgrade {cfg}");
    assert!(cfg["staged"].is_null() && cfg["pending"] == 0 && cfg["owner"] == "dao.sandbox" && cfg["version"] == "1.6.0");
    assert_eq!(cfg["admin_delay_secs"], 172_800, "the production build (48 h) now runs");
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await, alice_before, "alice's record kept through it");
    assert!(t.near("lp.sandbox").await - lp_near > 4.9, "the stager got the storage deposit back");

    // 4. Emergency pause, at once and by the owner only: nothing new starts, but Alice still gets
    //    everything out (from an open position) while it is paused.
    assert!(t.call("alice.sandbox", G, "set_paused", json!({ "paused": true }), zero).await.contains("Only the owner"));
    ok("dao pauses at once", t.call("dao.sandbox", G, "set_paused", json!({ "paused": true }), zero).await);
    let refused = t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await;
    println!("{:<44} {refused}", "carol joins while paused");
    assert!(refused.contains("paused"));
    ok("alice cash_out(leave) while paused", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    ok("dao resumes", t.call("dao.sandbox", G, "set_paused", json!({ "paused": false }), zero).await);
    ok("carol joins after the resume", t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await);
    ok("carol leaves", t.call("carol.sandbox", G, "leave", json!({}), zero).await);

    // The contract ends empty.
    assert_eq!(t.view(G, "get_users", json!({})).await, json!([]));
    let rhea = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": G })).await;
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"));
    assert_eq!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": G })).await, json!([]));
    Ok(())
}
