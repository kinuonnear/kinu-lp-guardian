//! The mainnet upgrade path exactly: v1.2 (as on mainnet, FHzvPc81) with users inside -> the
//! production v1.4 build (target/near/kinu_guardian_shared, the CI artifact when checking a release)
//! and migrate(owner) in ONE transaction signed by the contract's own key, on the real Rhea DCL 2.4.0.
//! Records survive, take profit works on the migrated state and goes with the position, the owner
//! pauses at once, and every user gets everything back. Run ./scripts/build.sh (or copy the CI wasm
//! into target/near/kinu_guardian_shared/) first.

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
}

fn ok(label: &str, r: String) {
    println!("{label:<44} {r}");
    assert_eq!(r, "ok", "{label}");
}

#[tokio::test]
async fn v12_to_production_v14_in_one_transaction() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "guardian", "alice", "bob", "carol", "kinu"] {
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
    deploy("dcl.sandbox", fixture("dcl-2.4.0.wasm"), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
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

    // v1.2 exactly as deployed on mainnet, with Alice (position) and Bob (ledger only).
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

    // The mainnet step: the production build + migrate(owner) in one transaction, signed by the
    // contract's own key. The owner is an account first (kinuonnear.near on mainnet), a DAO later.
    let code = built("kinu_guardian_shared");
    println!("{:<44} {} bytes", "production build", code.len());
    let r = Contract::deploy(G.parse()?).use_code(code).with_init_call("migrate", json!({ "owner": "kinu.sandbox" }))?.with_signer(signer.clone()).send_to(&net).await?;
    assert!(r.is_success(), "upgrade + migrate: {r:?}");
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("after deploy + migrate {cfg}");
    assert_eq!(cfg["version"], "1.4.0");
    assert_eq!(cfg["owner"], "kinu.sandbox");
    assert_eq!(cfg["admin_delay_secs"], 172_800, "the production build (48 h)");
    assert_eq!(cfg["users"], 2, "the v1.2 user list is kept");
    assert_eq!(cfg["paused"], false);
    assert_eq!(cfg["pending"], 0);
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await, alice_before, "alice's record kept");
    assert_eq!(t.view(G, "get_account", json!({ "user": "bob.sandbox" })).await, bob_before, "bob's record kept");
    assert!(t.call("alice.sandbox", G, "migrate", json!({ "owner": "alice.sandbox" }), zero).await.starts_with("ERR"), "migrate is private");

    // Take profit on the migrated state: only with an open position, read back, cleared on close.
    let bob_tp = t.call("bob.sandbox", G, "set_take_profit", json!({ "target": { "point": 310000, "above": true } }), zero).await;
    println!("{:<44} {bob_tp}", "bob sets take profit without a position");
    assert!(bob_tp.contains("Open a position first"));
    ok("alice sets take profit", t.call("alice.sandbox", G, "set_take_profit", json!({ "target": { "point": 310000, "above": true } }), zero).await);
    assert_eq!(t.view(G, "get_take_profit", json!({ "user": "alice.sandbox" })).await, json!({ "point": 310000, "above": true }));
    assert!(t.call("bob.sandbox", G, "set_paused", json!({ "paused": true }), zero).await.contains("Only the owner"));

    // The owner pauses at once; every way out still works.
    ok("owner pauses", t.call("kinu.sandbox", G, "set_paused", json!({ "paused": true }), zero).await);
    assert!(t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await.contains("paused"));
    let alice_near = t.near("alice.sandbox").await;
    ok("alice cash_out(leave) while paused", t.call("alice.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    let back = t.near("alice.sandbox").await - alice_near;
    println!("{:<44} +{back:.4} NEAR (plus TKQ)", "alice got back");
    assert!(back > 0.4, "alice's wNEAR came back as NEAR");
    assert_eq!(t.view(G, "get_take_profit", json!({ "user": "alice.sandbox" })).await, Value::Null, "the target went with the position");
    ok("bob cash_out(leave) while paused", t.call("bob.sandbox", G, "cash_out", json!({ "leave": true }), zero).await);
    ok("owner resumes", t.call("kinu.sandbox", G, "set_paused", json!({ "paused": false }), zero).await);
    ok("carol joins on v1.4", t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await);
    ok("carol leaves", t.call("carol.sandbox", G, "leave", json!({}), zero).await);

    // The contract ends empty.
    assert_eq!(t.view(G, "get_users", json!({})).await, json!([]));
    let rhea = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": G })).await;
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"));
    assert_eq!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": G })).await, json!([]));
    Ok(())
}
