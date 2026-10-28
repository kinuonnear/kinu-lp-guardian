//! The mainnet upgrade, rehearsed in near-sandbox: the exact v1 factory and vault builds that run on
//! guardian.kinuonnear.near (tests/fixtures/v1, hashes 3kZqsx... and 8hMAmATK...), one vault made
//! with them, then the new factory code deployed over the same account and the vault code replaced.
//! The factory keeps its state, the old vault keeps its code, new vaults get the new code, and the
//! factory pays only the storage difference. Run ./scripts/build.sh first.

use base64::Engine;
use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const V1_VAULT: &str = "8hMAmATK8TC2NBGKgVCV2D7edJdvyJSkMfXMkmoAByTJ";

fn built(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/v1/{name}.wasm", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn b64(code: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(code)
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
    async fn account(&self, id: &str) -> near_api::types::Account {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.unwrap().data
    }
    async fn code_hash(&self, id: &str) -> String {
        match self.account(id).await.contract_state {
            near_api::types::account::ContractState::LocalHash(h) => h.to_string(),
            other => format!("{other:?}"),
        }
    }
    async fn min_deposit(&self) -> NearToken {
        let raw: u128 = self.view("guardian.sandbox", "get_config", json!({})).await["min_deposit"].as_str().unwrap().parse().unwrap();
        NearToken::from_yoctonear(raw + 10u128.pow(22))
    }
}

#[tokio::test]
async fn upgrade_v1_factory_in_place() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["alice", "bob", "dcl", "guardian"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(60)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };

    // v1, as on mainnet: the factory is its own admin, inline mode, the v1 vault code published.
    Contract::deploy("guardian.sandbox".parse()?)
        .use_code(fixture("factory"))
        .with_init_call("new", json!({ "admin": "guardian.sandbox", "dcl": "dcl.sandbox", "mode": "Inline" }))?
        .with_signer(signer.clone())
        .send_to(&net)
        .await?
        .assert_success();
    let v1 = fixture("vault");
    assert!(t.call("guardian.sandbox", "guardian.sandbox", "publish_vault_code", json!({ "code": b64(&v1) }), NearToken::from_near(0)).await);
    let old_policy = json!({ "allowed_pools": ["a|b|2000"], "min_width": 400, "max_width": 1600, "max_distance": 800, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0 });
    let min_v1 = t.min_deposit().await;
    assert!(t.call("alice.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "alice-lp", "policy": old_policy, "agent": null, "exit_zec": null }), min_v1).await);
    assert_eq!(t.code_hash("alice-lp.guardian.sandbox").await, V1_VAULT);
    let before = t.account("guardian.sandbox").await;

    // Step 1: the new factory code over the same account, no init. The old state must still read.
    Contract::deploy("guardian.sandbox".parse()?).use_code(built("kinu_guardian_factory")).without_init_call().with_signer(signer.clone()).send_to(&net).await?.assert_success();
    let cfg = t.view("guardian.sandbox", "get_config", json!({})).await;
    assert_eq!(cfg["vault_count"], 1, "{cfg}");
    assert_eq!(cfg["vault_code_hash"], V1_VAULT, "{cfg}");
    assert_eq!(t.view("guardian.sandbox", "get_vaults", json!({ "owner": "alice.sandbox" })).await, json!(["alice-lp.guardian.sandbox"]));
    let mid = t.account("guardian.sandbox").await;

    // Step 2: replace the vault code. Only the admin, and never with the code already published.
    let v2 = built("kinu_guardian_vault");
    assert!(!t.call("alice.sandbox", "guardian.sandbox", "replace_vault_code", json!({ "code": b64(&v2) }), NearToken::from_near(0)).await, "only the admin");
    assert!(!t.call("guardian.sandbox", "guardian.sandbox", "replace_vault_code", json!({ "code": b64(&v1) }), NearToken::from_near(0)).await, "same code refused");
    assert!(t.call("guardian.sandbox", "guardian.sandbox", "replace_vault_code", json!({ "code": b64(&v2) }), NearToken::from_near(0)).await);
    let cfg = t.view("guardian.sandbox", "get_config", json!({})).await;
    assert_eq!(cfg["vault_code_hash"], b58(&Sha256::digest(&v2)), "{cfg}");
    let after = t.account("guardian.sandbox").await;

    // Bob's new vault runs the new code with the new policy fields; Alice's keeps v1, untouched.
    let new_policy = json!({ "allowed_pools": ["a|b|2000"], "min_width": 400, "max_width": 1600, "max_distance": 800, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 1625, "exit_on_rise": true, "rebalance_buffer_points": 200 });
    let min_v2 = t.min_deposit().await;
    assert!(t.call("bob.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "bob-lp", "policy": new_policy, "agent": null, "exit_zec": null }), min_v2).await);
    assert_eq!(t.code_hash("bob-lp.guardian.sandbox").await, b58(&Sha256::digest(&v2)));
    assert_eq!(t.code_hash("alice-lp.guardian.sandbox").await, V1_VAULT);
    let bob = t.view("bob-lp.guardian.sandbox", "get_vault", json!({})).await;
    assert_eq!(bob["policy"]["exit_on_rise"], json!(true), "{bob}");
    assert_eq!(bob["policy"]["rebalance_buffer_points"], json!(200), "{bob}");
    assert_eq!(t.view("alice-lp.guardian.sandbox", "get_vault", json!({})).await["owner"], "alice.sandbox");

    // What each step cost the factory. Storage NEAR stays locked in its balance; gas is burned.
    let near = |y: u128| y as f64 / 1e24;
    println!(
        "UPGRADE factory code: storage {} -> {} bytes, balance {:.6} -> {:.6} NEAR",
        before.storage_usage,
        mid.storage_usage,
        near(before.amount.as_yoctonear()),
        near(mid.amount.as_yoctonear())
    );
    println!(
        "UPGRADE vault code: storage {} -> {} bytes, balance {:.6} -> {:.6} NEAR; vault deposit {:.5} -> {:.5} NEAR",
        mid.storage_usage,
        after.storage_usage,
        near(mid.amount.as_yoctonear()),
        near(after.amount.as_yoctonear()),
        near(min_v1.as_yoctonear()),
        near(min_v2.as_yoctonear())
    );
    Ok(())
}
