//! Retiring Guardian, rehearsed in near-sandbox with the exact mainnet builds: the factory (code
//! CgCKE7…, inline vault code in its state) and the v1 test vault (BEjFkn…-era code at 74890a…).
//! Each account is emptied by its own full-access key and deleted with kinu's account as beneficiary.

use base64::Engine;
use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";

fn built(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn b64(v: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(v)
}
/// Borsh of an AccountId (a string): u32 little-endian length, then the bytes.
fn borsh_str(s: &str) -> Vec<u8> {
    let mut v = (s.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v
}

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> bool {
        Contract(to.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(300))
            .deposit(deposit)
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await
            .map(|r| r.is_success())
            .unwrap_or(false)
    }
    async fn account(&self, id: &str) -> Option<near_api::types::Account> {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.ok().map(|r| r.data)
    }
    async fn deploy_bare(&self, id: &str, code: Vec<u8>) -> bool {
        Contract::deploy(id.parse().unwrap()).use_code(code).without_init_call().with_signer(self.signer.clone()).send_to(&self.net).await.map(|r| r.is_success()).unwrap_or(false)
    }
    async fn delete(&self, id: &str, beneficiary: &str) -> String {
        let r = near_api::Account(id.parse().unwrap())
            .delete_account_with_beneficiary(beneficiary.parse().unwrap())
            .with_signer(self.signer.clone())
            .send_to(&self.net)
            .await;
        match r {
            Ok(r) if r.is_success() => "ok".into(),
            Ok(r) => format!("{:?}", r.into_result().err()).chars().take(200).collect(),
            Err(e) => format!("ERR {e}").chars().take(200).collect(),
        }
    }
}

#[tokio::test]
async fn retire_factory_and_test_vault() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["guardian", "kinu", "testvault", "dcl"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(10)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };

    // The factory as on mainnet: its own admin, inline mode, vault code published, one vault (kinu-1).
    Contract::deploy("guardian.sandbox".parse()?)
        .use_code(built("kinu_guardian_factory"))
        .with_init_call("new", json!({ "admin": "guardian.sandbox", "dcl": "dcl.sandbox", "mode": "Inline" }))?
        .with_signer(signer.clone())
        .send_to(&net)
        .await?
        .assert_success();
    assert!(t.call("guardian.sandbox", "guardian.sandbox", "publish_vault_code", json!({ "code": b64(&fixture("v2/vault.wasm")) }), NearToken::from_near(0)).await);
    let policy = json!({ "allowed_pools": ["a|b|10000"], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0 });
    let min: u128 = Contract("guardian.sandbox".parse()?).call_function("get_config", json!({})).read_only::<Value>().fetch_from(&net).await?.data["min_deposit"].as_str().unwrap().parse()?;
    assert!(t.call("kinu.sandbox", "guardian.sandbox", "create_vault", json!({ "name": "kinu-1", "policy": policy, "agent": null, "exit_zec": null }), NearToken::from_yoctonear(min + 10u128.pow(22))).await);
    let f = t.account("guardian.sandbox").await.unwrap();
    println!("factory before: {} NEAR, storage {} B", f.amount.as_yoctonear() as f64 / 1e24, f.storage_usage);

    // 1. Deleting it as it is: refused (too much state).
    let r = t.delete("guardian.sandbox", "kinu.sandbox").await;
    println!("delete as is -> {r}");
    assert_ne!(r, "ok");

    // 2. Its own key deploys the cleanup code and removes the state: STATE, the vault code (Key::Code
    //    = [1]) and the vault list entry (Key::Vaults = [0] + the owner's account id).
    assert!(t.deploy_bare("guardian.sandbox", built("kinu_guardian_cleanup")).await);
    let mut vaults_key = vec![0u8];
    vaults_key.extend(borsh_str("kinu.sandbox"));
    let keys = json!({ "keys": [b64(b"STATE"), b64(&[1u8]), b64(&vaults_key)] });
    assert!(!t.call("kinu.sandbox", "guardian.sandbox", "clean", keys.clone(), NearToken::from_near(0)).await, "only the account itself");
    assert!(t.call("guardian.sandbox", "guardian.sandbox", "clean", keys, NearToken::from_near(0)).await);
    let after_clean = t.account("guardian.sandbox").await.unwrap();
    println!("after clean: storage {} B", after_clean.storage_usage);

    // 3. The empty module, then delete with kinu as beneficiary.
    assert!(t.deploy_bare("guardian.sandbox", EMPTY_WASM.to_vec()).await);
    println!("after empty code: storage {} B", t.account("guardian.sandbox").await.unwrap().storage_usage);
    let kinu_before = t.account("kinu.sandbox").await.unwrap().amount;
    let held = t.account("guardian.sandbox").await.unwrap().amount;
    let r = t.delete("guardian.sandbox", "kinu.sandbox").await;
    println!("delete factory -> {r}");
    assert_eq!(r, "ok");
    let got = t.account("kinu.sandbox").await.unwrap().amount.saturating_sub(kinu_before);
    println!("factory held {held}, kinu received {got}");
    assert!(t.account("guardian.sandbox").await.is_none());

    // The test vault as on mainnet: v1 vault code at an account its owner holds the key of.
    Contract::deploy("testvault.sandbox".parse()?)
        .use_code(fixture("v1/vault.wasm"))
        .with_init_call("new", json!({ "owner": "testvault.sandbox", "agent": null, "dcl": "dcl.sandbox", "policy": policy }))?
        .with_signer(signer.clone())
        .send_to(&net)
        .await?
        .assert_success();
    // Its state is small (the code does not count toward NEAR's deletion limit): one delete is enough.
    println!("test vault storage {} B", t.account("testvault.sandbox").await.unwrap().storage_usage);
    let kinu_before = t.account("kinu.sandbox").await.unwrap().amount;
    let held = t.account("testvault.sandbox").await.unwrap().amount;
    let r = t.delete("testvault.sandbox", "kinu.sandbox").await;
    println!("delete test vault as is -> {r}");
    assert_eq!(r, "ok");
    assert!(t.account("testvault.sandbox").await.is_none());
    let got = t.account("kinu.sandbox").await.unwrap().amount.saturating_sub(kinu_before);
    println!("test vault held {held}, kinu received {got}");
    Ok(())
}
