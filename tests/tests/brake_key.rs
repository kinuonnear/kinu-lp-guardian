//! The automatic brake on a real chain: api/_guardianBrake.js signs and sends `pause` with a key limited
//! to it. A full key and a key that can do more are refused before anything is sent. Local only: it runs
//! the site's code through Node.

use ed25519_dalek::SigningKey;
use near_api::types::transaction::actions::{AccessKeyPermission, FunctionCallPermission};
use near_api::{Account, Contract, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};

const G: &str = "guardian.sandbox";

fn built(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}
fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn node(rpc: &str, secret: &str) -> Value {
    let script = std::env::var("BRAKE_E2E").unwrap();
    let out = std::process::Command::new("/mnt/c/Program Files/nodejs/node.exe").args([script.as_str(), rpc, G, "kinu.sandbox", secret]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    println!("{text}{}", String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>());
    serde_json::from_str(text.lines().last().unwrap_or("null")).unwrap_or(Value::Null)
}

#[tokio::test]
async fn the_brake_pauses_with_a_limited_key_only() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let genesis = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["kinu", "guardian", "dcl", "wrap"] {
        sandbox.create_account(format!("{name}.sandbox").parse()?).initial_balance(NearToken::from_near(40)).send().await?;
    }
    Contract::deploy("wrap.sandbox".parse()?).use_code(fixture("wrap-near.wasm")).with_init_call("new", json!({}))?.with_signer(genesis.clone()).send_to(&net).await?.assert_success();
    Contract::deploy(G.parse()?).use_code(built("kinu_guardian_shared")).with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": "kinu.sandbox" }))?.with_signer(genesis.clone()).send_to(&net).await?.assert_success();
    let paused = || {
        let net = net.clone();
        async move { tokio::time::sleep(std::time::Duration::from_secs(4)).await; Contract(G.parse().unwrap()).call_function("get_config", json!({})).read_only::<Value>().fetch_from(&net).await.unwrap().data["paused"] == true }
    };
    let add = |seed: u8, methods: Vec<&str>| {
        let (net, genesis) = (net.clone(), genesis.clone());
        let methods: Vec<String> = methods.into_iter().map(String::from).collect();
        async move {
            let sk = SigningKey::from_bytes(&[seed; 32]);
            let public = format!("ed25519:{}", bs58::encode(sk.verifying_key().to_bytes()).into_string());
            let perm = AccessKeyPermission::FunctionCall(FunctionCallPermission { allowance: Some(NearToken::from_millinear(100)), receiver_id: G.to_string(), method_names: methods });
            Account("kinu.sandbox".parse().unwrap()).add_key(perm, public.parse::<near_api::types::PublicKey>().unwrap()).with_signer(genesis).send_to(&net).await.unwrap().assert_success();
            format!("ed25519:{}", bs58::encode([seed; 32]).into_string())
        }
    };
    let rpc = sandbox.rpc_addr.clone();

    println!("\n--- a full key ---");
    let r = node(&rpc, near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY);
    assert!(r["msgs"][0].as_str().unwrap().contains("key refused"), "{r}");
    assert!(!paused().await);

    println!("\n--- a key with one method too many ---");
    let wide = add(7, vec!["pause", "set_paused"]).await;
    let r = node(&rpc, &wide);
    assert!(r["msgs"][0].as_str().unwrap().contains("key refused"), "{r}");
    assert!(!paused().await);

    println!("\n--- a key limited to pause ---");
    let limited = add(9, vec!["pause"]).await;
    let r = node(&rpc, &limited);
    assert_eq!(r["paused"], true, "{r}");
    assert!(r["msgs"][0].as_str().unwrap().contains("is paused"), "{r}");
    assert!(paused().await, "paused on the chain");
    println!("{:<40} {}", "paused on the chain", paused().await);
    Ok(())
}
