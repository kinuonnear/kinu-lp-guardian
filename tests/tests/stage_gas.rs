//! The mainnet stage, measured: the CI wasm staged on the live v1.6 by its own account, with the gas and
//! deposit the owner will sign. Local only.

use near_api::{Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::json;

const G: &str = "guardian.sandbox";

#[tokio::test]
async fn stage_the_ci_wasm_at_the_signed_numbers() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    sandbox.create_account(G.parse()?).initial_balance(NearToken::from_near(10)).send().await?;
    let v16 = std::fs::read(format!("{}/fixtures/shared-v1.6.wasm", env!("CARGO_MANIFEST_DIR")))?;
    Contract::deploy(G.parse()?).use_code(v16).with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": "kinu.sandbox" }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    let wasm = std::fs::read(std::env::var("CI_WASM")?)?;
    println!("wasm {} bytes", wasm.len());
    let out = Contract(G.parse()?).call_function_raw("stage_code", wasm).transaction().gas(NearGas::from_tgas(150)).deposit(NearToken::from_millinear(4560)).with_signer(G.parse()?, signer.clone()).send_to(&net).await?;
    let dbg = format!("{out:?}");
    let burnt = dbg.split("total_gas_burnt: NearGas { inner: ").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1e12;
    println!("success {} burnt {burnt:.1} TGas", out.is_success());
    let cfg = Contract(G.parse()?).call_function("get_config", json!({})).read_only::<serde_json::Value>().fetch_from(&net).await?.data;
    println!("staged {}", cfg["staged"]);
    assert!(out.is_success(), "{dbg}");
    assert_eq!(cfg["staged"]["code_hash"], "Cs89ihQAYiDCbaV3c9ZxY7rsthKMWmDXR3ijQBb72E9p");
    Ok(())
}
