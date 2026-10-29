//! The agent's `restrict_key` on a real chain (sandbox): the agent's own transaction encoding
//! (agent/src/lib.rs), signed with an ed25519 key as the TEE signs it. One transaction turns the
//! agent's full-access key into a function-call key for the Guardian only (poke,
//! request_rebalance, exit); afterwards a NEAR transfer, a call to any other method and a call to
//! any other contract are refused by the protocol, while `poke` still reaches the contract.
//! Run ./scripts/build.sh first.

use base64::Engine;
use ed25519_dalek::{Signer as _, SigningKey};
use kinu_guardian_agent::{FunctionCall, NearAction, PublicKey, Signature, SignedTransaction, Transaction, implicit_account, restrict_key_actions};
use near_api::{Contract, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};

const G: &str = "guardian.sandbox";

fn built(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}

async fn rpc(url: &str, method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    reqwest::Client::new().post(url).json(&body).send().await.unwrap().json().await.unwrap()
}

/// Signs `actions` from the agent's implicit account and sends them; the RPC's whole answer.
async fn send(url: &str, key: &SigningKey, receiver: &str, actions: Vec<NearAction>) -> Value {
    let pk = key.verifying_key().to_bytes();
    let me = implicit_account(&pk);
    let ak = rpc(url, "query", json!({ "request_type": "view_access_key", "finality": "optimistic", "account_id": me, "public_key": kinu_guardian_agent::near_public_key(&pk) })).await;
    let nonce = ak["result"]["nonce"].as_u64().expect("access key") + 1;
    let block = rpc(url, "block", json!({ "finality": "final" })).await;
    let hash: [u8; 32] = bs58::decode(block["result"]["header"]["hash"].as_str().unwrap()).into_vec().unwrap().try_into().unwrap();
    let tx = Transaction { signer_id: me, public_key: PublicKey::Ed25519(pk), nonce, receiver_id: receiver.to_string(), block_hash: hash, actions };
    let sig = key.sign(&tx.hash()).to_bytes();
    let signed = SignedTransaction { transaction: tx, signature: Signature::Ed25519(sig) };
    let b64 = base64::engine::general_purpose::STANDARD.encode(borsh::to_vec(&signed).unwrap());
    rpc(url, "broadcast_tx_commit", json!([b64])).await
}

fn verdict(r: &Value) -> String {
    if let Some(e) = r.get("error") {
        return format!("REJECTED {}", e["data"].to_string().chars().take(160).collect::<String>());
    }
    let status = &r["result"]["status"];
    if status.get("SuccessValue").is_some() { "ok".into() } else { format!("FAILED {}", status.to_string().chars().take(160).collect::<String>()) }
}

#[tokio::test]
async fn restrict_key_leaves_only_the_guardian_calls() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let url = sandbox.rpc_addr.clone();
    let net = NetworkConfig::from_rpc_url("sandbox", url.parse()?);
    let genesis = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["guardian", "dcl", "owner"] {
        sandbox.create_account(format!("{name}.sandbox").parse()?).initial_balance(NearToken::from_near(20)).send().await?;
    }
    Contract::deploy(G.parse()?)
        .use_code(built("kinu_guardian_shared"))
        .with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": "owner.sandbox" }))?
        .with_signer(genesis.clone())
        .send_to(&net)
        .await?
        .assert_success();

    // The agent's account: an implicit account of an ed25519 key, funded with 1 NEAR.
    let key = SigningKey::from_bytes(&[42u8; 32]);
    let pk = key.verifying_key().to_bytes();
    let me = implicit_account(&pk);
    near_api::Tokens::account("sandbox".parse()?).send_to(me.parse()?).near(NearToken::from_near(1)).with_signer(genesis.clone()).send_to(&net).await?.assert_success();

    // Before: a full-access key (a transfer works).
    let r = send(&url, &key, "owner.sandbox", vec![NearAction::Transfer(1)]).await;
    println!("{:<48} {}", "transfer before restrict_key", verdict(&r));
    assert_eq!(verdict(&r), "ok");

    // restrict_key: one transaction, to itself.
    let r = send(&url, &key, &me, restrict_key_actions(pk, G)).await;
    println!("{:<48} {}", "restrict_key", verdict(&r));
    assert_eq!(verdict(&r), "ok");
    let ak = rpc(&url, "query", json!({ "request_type": "view_access_key", "finality": "optimistic", "account_id": me, "public_key": kinu_guardian_agent::near_public_key(&pk) })).await;
    let perm = &ak["result"]["permission"];
    println!("{:<48} {perm}", "the key now");
    assert_eq!(perm["FunctionCall"]["receiver_id"], G);
    assert_eq!(perm["FunctionCall"]["method_names"], json!(["poke", "request_rebalance", "exit"]));
    assert!(perm["FunctionCall"]["allowance"].is_null());

    // After: NEAR cannot leave, other methods and other contracts are refused by the protocol.
    let r = send(&url, &key, "owner.sandbox", vec![NearAction::Transfer(1)]).await;
    println!("{:<48} {}", "transfer after", verdict(&r));
    assert!(verdict(&r).starts_with("REJECTED"));
    let call = |m: &str| NearAction::FunctionCall(FunctionCall { method_name: m.into(), args: json!({ "token": "x.sandbox" }).to_string().into_bytes(), gas: 30_000_000_000_000, deposit: 0 });
    let r = send(&url, &key, G, vec![call("withdraw")]).await;
    println!("{:<48} {}", "Guardian.withdraw after", verdict(&r));
    assert!(verdict(&r).starts_with("REJECTED"));
    let r = send(&url, &key, "dcl.sandbox", vec![call("poke")]).await;
    println!("{:<48} {}", "another contract after", verdict(&r));
    assert!(verdict(&r).starts_with("REJECTED"));
    let r = send(&url, &key, &me, restrict_key_actions(pk, "evil.sandbox")).await;
    println!("{:<48} {}", "re-key to another contract after", verdict(&r));
    assert!(verdict(&r).starts_with("REJECTED"));
    // poke reaches the contract (which then refuses a user who never joined: that is the contract's
    // answer, not the protocol's).
    let poke = NearAction::FunctionCall(FunctionCall { method_name: "poke".into(), args: json!({ "user": "nobody.sandbox" }).to_string().into_bytes(), gas: 30_000_000_000_000, deposit: 0 });
    let r = send(&url, &key, G, vec![poke]).await;
    let v = verdict(&r);
    println!("{:<48} {v}", "Guardian.poke after");
    assert!(v.starts_with("FAILED") && v.contains("has not joined"), "{v}");
    Ok(())
}
