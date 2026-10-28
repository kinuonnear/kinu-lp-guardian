//! Factory end-to-end in near-sandbox: the vault code is published once as a global contract,
//! anyone creates their own vault, a taken name or a non-private exit address fails and the
//! deposit comes back, and a factory vault works like any vault. Run ./scripts/build.sh first.

use base64::Engine;
use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const UA: &str = "u1fasx684eaqjej3rvn7kjnjsymdm8mdv66nuym9uh9xznmf3skz6zz0qt3q3cflw8cgy8cfndran77a4e797q7t32u779q9halyx7djty";

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
        Contract(contract.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(300))
            .deposit(deposit)
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await
            .unwrap()
            .is_success()
    }
    async fn view(&self, contract: &str, method: &str, args: Value) -> Value {
        Contract(contract.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.unwrap().data
    }
    async fn balance(&self, who: &str) -> NearToken {
        near_api::Tokens::account(who.parse().unwrap()).near_balance().fetch_from(&self.net).await.unwrap().total
    }
}

fn b58(bytes: &[u8]) -> String {
    const A: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut n: Vec<u8> = Vec::new();
    for &b in bytes {
        let mut c = u32::from(b);
        for d in n.iter_mut() {
            c += u32::from(*d) << 8;
            *d = (c % 58) as u8;
            c /= 58;
        }
        while c > 0 {
            n.push((c % 58) as u8);
            c /= 58;
        }
    }
    n.iter().rev().map(|d| A[*d as usize] as char).collect()
}

#[tokio::test]
async fn factory_flow_global() -> testresult::TestResult<()> {
    factory_flow("global").await
}

#[tokio::test]
async fn factory_flow_inline() -> testresult::TestResult<()> {
    factory_flow("inline").await
}

async fn factory_flow(mode: &str) -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["admin", "alice", "bob", "dcl", "guardian"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(60)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    Contract::deploy("guardian.sandbox".parse()?)
        .use_code(wasm("kinu_guardian_factory"))
        .with_init_call("new", json!({ "admin": "admin.sandbox", "dcl": "dcl.sandbox", "mode": if mode == "global" { "Global" } else { "Inline" } }))?
        .with_signer(signer.clone())
        .send_to(&net)
        .await?
        .assert_success();

    // Publish the vault code once; the factory's hash is the sha256 of exactly that code.
    let code = wasm("kinu_guardian_vault");
    let b64 = base64::engine::general_purpose::STANDARD.encode(&code);
    assert!(!t.call("alice.sandbox", "guardian.sandbox", "publish_vault_code", json!({ "code": b64 }), NearToken::from_near(10)).await, "only the admin publishes");
    assert!(t.call("admin.sandbox", "guardian.sandbox", "publish_vault_code", json!({ "code": b64 }), NearToken::from_near(10)).await);
    let cfg = t.view("guardian.sandbox", "get_config", json!({})).await;
    assert_eq!(cfg["vault_code_hash"], b58(&Sha256::digest(&code)), "{cfg}");
    assert!(!t.call("admin.sandbox", "guardian.sandbox", "publish_vault_code", json!({ "code": b64 }), NearToken::from_near(10)).await, "published once");

    // Inline: each vault carries its own copy of the code, so it needs its storage too.
    let cfg_min: u128 = t.view("guardian.sandbox", "get_config", json!({})).await["min_deposit"].as_str().unwrap().parse()?;
    let deposit = NearToken::from_yoctonear(cfg_min + 10u128.pow(22));
    assert!(deposit.as_yoctonear() >= cfg_min, "deposit {deposit} below min {cfg_min}");
    let policy = json!({ "allowed_pools": ["a|b|2000"], "min_width": 400, "max_width": 8000, "max_distance": 400, "max_actions": 10, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 2877 });

    // Alice creates her vault with her Orchard exit address at birth.
    assert!(t.call("alice.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "alice-lp", "policy": policy, "agent": null, "exit_zec": UA }), deposit).await);
    let v = t.view("alice-lp.guardian.sandbox", "get_vault", json!({})).await;
    assert_eq!(v["owner"], "alice.sandbox");
    assert_eq!(v["exit_zec"], UA);
    assert_eq!(v["dcl"], "dcl.sandbox");
    assert_eq!(t.view("guardian.sandbox", "get_vaults", json!({ "owner": "alice.sandbox" })).await, json!(["alice-lp.guardian.sandbox"]));

    // The vault really is Alice's: she freezes it, Bob cannot.
    assert!(!t.call("bob.sandbox", "alice-lp.guardian.sandbox", "freeze", json!({}), NearToken::from_near(0)).await);
    assert!(t.call("alice.sandbox", "alice-lp.guardian.sandbox", "freeze", json!({}), NearToken::from_near(0)).await);

    // A taken name and a transparent exit address both fail; Bob's deposit comes back.
    let before = t.balance("bob.sandbox").await;
    t.call("bob.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "alice-lp", "policy": policy, "agent": null, "exit_zec": null }), deposit).await;
    t.call("bob.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "bob-lp", "policy": policy, "agent": null, "exit_zec": "t1Q879cLgqaCd7zKRi79wQYuGBenmNX6cKn" }), deposit).await;
    let after = t.balance("bob.sandbox").await;
    assert!(before.saturating_sub(after) < NearToken::from_millinear(100), "deposits refunded: {before} -> {after}");
    assert_eq!(t.view("guardian.sandbox", "get_vaults", json!({ "owner": "bob.sandbox" })).await, json!([]));
    assert_eq!(t.view("guardian.sandbox", "get_config", json!({})).await["vault_count"], 1);

    // Too small a deposit is refused up front.
    assert!(!t.call("bob.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "bob-lp", "policy": policy, "agent": null, "exit_zec": null }), NearToken::from_millinear(100)).await);
    Ok(())
}
