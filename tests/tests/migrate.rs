//! v1.1 (as on mainnet, 3M8vEyfN) → v1.2 in place, on the real Rhea DCL 2.4.0, with users inside:
//! Alice holds a position, Bob only a ledger. The deploy runs `migrate` in the same transaction; the
//! records stay, the new user list starts empty and the contract's own key fills it with
//! `index_users`; a new user is listed on join; everyone cashes out and the contract ends empty.
//! Run ./scripts/build.sh first.

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
}

fn ok(label: &str, r: String) {
    println!("{label:<44} {r}");
    assert_eq!(r, "ok", "{label}");
}

#[tokio::test]
async fn v11_to_v12_in_place() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "guardian", "alice", "bob", "carol"] {
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

    // v1.1 exactly as deployed on mainnet, with Alice (position) and Bob (ledger only).
    deploy(G, fixture("shared-v1.1.wasm"), json!({ "dcl": "dcl.sandbox", "wnear": WRAP })).await;
    ok("register_dcl", t.call("owner.sandbox", G, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in [TKQ, WRAP] {
        ok("register_token", t.call("owner.sandbox", G, "register_token", json!({ "token": tok }), NearToken::from_millinear(125)).await);
    }
    let policy = json!({ "allowed_pools": [POOL], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    for who in ["alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        t.call(who, WRAP, "near_deposit", json!({}), NearToken::from_near(3)).await;
    }
    for who in ["alice.sandbox", "bob.sandbox"] {
        ok(&format!("{who} joins v1.1"), t.call(who, G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await);
    }
    let open = json!({ "left_point": 303200, "right_point": 304600 }).to_string();
    ok("alice deposit + open on v1.1", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "1000000000000000000000000", "msg": open }), yocto).await);
    ok("bob deposit only on v1.1", t.call("bob.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": "500000000000000000000000", "msg": "" }), yocto).await);
    let alice_before = t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await;
    let bob_before = t.view(G, "get_account", json!({ "user": "bob.sandbox" })).await;
    assert!(alice_before["position"].is_object());
    println!("v1.1 config {}", t.view(G, "get_config", json!({})).await);

    // In place: the new code and `migrate` in one transaction, signed by the contract account.
    let r = Contract::deploy(G.parse()?).use_code(built("kinu_guardian_shared")).with_init_call("migrate", json!({}))?.with_signer(signer.clone()).send_to(&net).await?;
    assert!(r.is_success(), "upgrade + migrate: {r:?}");
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("v1.2 config {cfg}");
    assert_eq!(cfg["version"], "1.2.0");
    assert_eq!(cfg["users"], 0);
    assert_eq!(t.view(G, "get_account", json!({ "user": "alice.sandbox" })).await, alice_before, "alice's record kept");
    assert_eq!(t.view(G, "get_account", json!({ "user": "bob.sandbox" })).await, bob_before, "bob's record kept");
    // migrate cannot run again by anyone else.
    assert!(t.call("alice.sandbox", G, "migrate", json!({}), zero).await.starts_with("ERR"));

    // The contract's own key lists the existing users; others cannot.
    assert!(t.call("alice.sandbox", G, "index_users", json!({ "users": ["alice.sandbox"] }), zero).await.contains("Only the contract account itself"));
    ok("index_users by the contract itself", t.call(G, G, "index_users", json!({ "users": ["alice.sandbox", "bob.sandbox", "carol.sandbox"] }), zero).await);
    assert_eq!(t.view(G, "get_users", json!({})).await, json!(["alice.sandbox", "bob.sandbox"]), "carol has no record, not listed");
    ok("carol joins v1.2", t.call("carol.sandbox", G, "join", json!({ "policy": policy, "agent": null }), NearToken::from_millinear(10)).await);
    assert_eq!(t.view(G, "get_user_count", json!({})).await, json!(3));
    assert_eq!(t.view(G, "get_users", json!({ "from_index": 1, "limit": 1 })).await, json!(["bob.sandbox"]));

    // Everyone out; the list and the contract's Rhea balance end empty.
    for who in ["alice.sandbox", "bob.sandbox", "carol.sandbox"] {
        ok(&format!("{who} cash_out(leave) on v1.2"), t.call(who, G, "cash_out", json!({ "leave": true }), zero).await);
    }
    assert_eq!(t.view(G, "get_users", json!({})).await, json!([]));
    let rhea = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": G })).await;
    println!("Rhea balance after everyone left: {rhea}");
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"));
    assert_eq!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": G })).await, json!([]));
    Ok(())
}
