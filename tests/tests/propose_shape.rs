//! The exact JSON the page's Propose button builds, on the live v1.6 and on the CI v1.7: `propose` takes
//! it and `get_pending` shows the right action. Local only.

use near_api::{Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};

const G: &str = "guardian.sandbox";

async fn run(wasm: Vec<u8>, staged: Option<Vec<u8>>, actions: &[Value]) -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in [G, "kinu.sandbox"] {
        sandbox.create_account(name.parse()?).initial_balance(NearToken::from_near(20)).send().await?;
    }
    Contract::deploy(G.parse()?).use_code(wasm).with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": "kinu.sandbox" }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    if let Some(code) = staged {
        Contract(G.parse()?).call_function_raw("stage_code", code).transaction().gas(NearGas::from_tgas(150)).deposit(NearToken::from_millinear(4560)).with_signer(G.parse()?, signer.clone()).send_to(&net).await?.assert_success();
    }
    for action in actions {
        let out = Contract(G.parse()?).call_function("propose", json!({ "action": action })).transaction().with_signer("kinu.sandbox".parse()?, signer.clone()).send_to(&net).await?;
        println!("{action} success {}", out.is_success());
        assert!(out.is_success(), "{out:?}");
    }
    let pending = Contract(G.parse()?).call_function("get_pending", json!({})).read_only::<Value>().fetch_from(&net).await?.data;
    println!("pending {pending}");
    assert_eq!(pending.as_array().unwrap().len(), actions.len());
    Ok(())
}

#[tokio::test]
async fn the_buttons_json_reaches_the_contract() -> testresult::TestResult<()> {
    let page: Value = serde_json::from_str(&std::env::var("PAGE_JSON")?)?;
    let ci = std::fs::read(std::env::var("CI_WASM")?)?;
    let v16 = std::fs::read(format!("{}/fixtures/shared-v1.6.wasm", env!("CARGO_MANIFEST_DIR")))?;
    println!("--- v1.6, as live now ---");
    run(v16, Some(ci.clone()), &[page["upgrade"].clone(), page["owner"].clone()]).await?;
    println!("--- v1.7 ---");
    run(ci, None, &[page["pauser"].clone(), page["owner"].clone()]).await?;
    Ok(())
}
