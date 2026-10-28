//! End-to-end in near-sandbox: owner funds a vault, a compromised agent is refused on-chain, the
//! honest rebalance opens a position held by the vault, the agent's exit waits for the owner's rule,
//! then the exit returns the funds to the vault and the owner sweeps them. Rhea DCL is the mock in
//! contracts/mockdcl (same methods and argument shapes). Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const POOL: &str = "tka.sandbox|tkb.sandbox|2000";
const ONE_K: &str = "1000000000"; // 1,000 tokens with 6 decimals

fn wasm(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}

impl T {
    async fn call(&self, from: &str, contract: &str, method: &str, args: Value, deposit: NearToken) -> bool {
        let r = Contract(contract.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(300))
            .deposit(deposit)
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await
            .unwrap();
        r.is_success()
    }
    async fn view(&self, contract: &str, method: &str, args: Value) -> Value {
        Contract(contract.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.unwrap().data
    }
}

#[tokio::test]
async fn guardian_flow() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "agent", "stranger", "dcl", "vault", "tka", "tkb"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(30)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };

    // Rhea DCL stand-in, two tokens, one pool at point 0.
    deploy("dcl.sandbox", wasm("kinu_guardian_mockdcl"), json!({})).await;
    for (id, sym) in [("tka.sandbox", "TKA"), ("tkb.sandbox", "TKB")] {
        deploy(id, wasm("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000", "symbol": sym, "decimals": 6 })).await;
        assert!(t.call("owner.sandbox", id, "storage_deposit", json!({ "account_id": "dcl.sandbox", "registration_only": true }), NearToken::from_millinear(125)).await);
    }
    assert!(t.call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": "tka.sandbox", "token_b": "tkb.sandbox", "fee": 2000, "init_point": 0 }), NearToken::from_near(0)).await);

    // The vault and the owner's policy.
    let policy = json!({ "allowed_pools": [POOL], "min_width": 400, "max_width": 8000, "max_distance": 400, "max_actions": 10, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 2877 });
    deploy("vault.sandbox", wasm("kinu_guardian_vault"), json!({ "owner": "owner.sandbox", "agent": "agent.sandbox", "dcl": "dcl.sandbox", "policy": policy })).await;
    assert!(t.call("owner.sandbox", "vault.sandbox", "register_dcl", json!({}), NearToken::from_millinear(500)).await);
    for tok in ["tka.sandbox", "tkb.sandbox"] {
        assert!(t.call("owner.sandbox", "vault.sandbox", "register_token", json!({ "token": tok }), NearToken::from_millinear(125)).await);
        // Owner funds the vault; the vault forwards it into its DCL balance.
        assert!(t.call("owner.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "vault.sandbox", "amount": ONE_K, "msg": "" }), NearToken::from_yoctonear(1)).await);
    }
    let assets = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "vault.sandbox" })).await;
    assert_eq!(assets["tka.sandbox"], ONE_K, "vault DCL balance: {assets}");
    assert_eq!(assets["tkb.sandbox"], ONE_K);


    // A compromised agent: every one of these is refused on-chain.
    let bad = [
        json!({ "pool_id": "evil.sandbox|tkb.sandbox|2000", "left_point": -400, "right_point": 400 }), // other pool
        json!({ "pool_id": POOL, "left_point": -400000, "right_point": 400000 }),                       // absurd width
        json!({ "pool_id": POOL, "left_point": 50000, "right_point": 51000 }),                          // far from price
    ];
    for r in bad {
        assert!(!t.call("agent.sandbox", "vault.sandbox", "request_rebalance", json!({ "request": r }), NearToken::from_near(0)).await, "should be refused: {r}");
    }
    assert!(!t.call("stranger.sandbox", "vault.sandbox", "request_rebalance", json!({ "request": { "pool_id": POOL, "left_point": -400, "right_point": 400 } }), NearToken::from_near(0)).await);
    assert!(!t.call("agent.sandbox", "vault.sandbox", "set_exit_zec", json!({ "address": "u1fasx684eaqjej3rvn7kjnjsymdm8mdv66nuym9uh9xznmf3skz6zz0qt3q3cflw8cgy8cfndran77a4e797q7t32u779q9halyx7djty" }), NearToken::from_near(0)).await);
    assert!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await.as_array().unwrap().is_empty());

    // The honest rebalance: the vault opens the position itself.
    assert!(t.call("agent.sandbox", "vault.sandbox", "request_rebalance", json!({ "request": { "pool_id": POOL, "left_point": -400, "right_point": 400 } }), NearToken::from_near(0)).await);
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await;
    assert_eq!(positions.as_array().unwrap().len(), 1, "{positions}");
    assert_eq!(positions[0]["owner_id"], "vault.sandbox");
    let v = t.view("vault.sandbox", "get_vault", json!({})).await;
    assert_eq!(v["position"]["lpt_id"], positions[0]["lpt_id"]);

    // A second rebalance closes the old position and opens a new one-sided range above the price.
    assert!(t.call("agent.sandbox", "vault.sandbox", "request_rebalance", json!({ "request": { "pool_id": POOL, "left_point": 200, "right_point": 1200 } }), NearToken::from_near(0)).await);
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await;
    assert_eq!(positions.as_array().unwrap().len(), 1, "{positions}");
    assert_eq!(positions[0]["left_point"], 200);

    // The agent may not exit before the owner's rule is met...
    assert!(!t.call("agent.sandbox", "vault.sandbox", "exit", json!({}), NearToken::from_near(0)).await);
    // ...the price falls 3,000 points (> 25%) from the high the vault saw itself...
    assert!(t.call("owner.sandbox", "dcl.sandbox", "set_point", json!({ "pool_id": POOL, "point": -3000 }), NearToken::from_near(0)).await);
    // ...now the agent's exit goes through, and the funds land in the vault's own wallet.
    assert!(t.call("agent.sandbox", "vault.sandbox", "exit", json!({}), NearToken::from_near(0)).await);
    assert!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await.as_array().unwrap().is_empty());
    let bal_a = t.view("tka.sandbox", "ft_balance_of", json!({ "account_id": "vault.sandbox" })).await;
    assert_eq!(bal_a, ONE_K);

    // Only the owner moves them out.
    assert!(!t.call("agent.sandbox", "vault.sandbox", "sweep_to_owner", json!({ "token": "tka.sandbox", "amount": ONE_K }), NearToken::from_near(0)).await);
    assert!(t.call("owner.sandbox", "vault.sandbox", "sweep_to_owner", json!({ "token": "tka.sandbox", "amount": ONE_K }), NearToken::from_near(0)).await);

    // Revoked agent is refused.
    assert!(t.call("owner.sandbox", "vault.sandbox", "revoke_agent", json!({}), NearToken::from_near(0)).await);
    assert!(!t.call("agent.sandbox", "vault.sandbox", "request_rebalance", json!({ "request": { "pool_id": POOL, "left_point": -400, "right_point": 400 } }), NearToken::from_near(0)).await);
    Ok(())
}
