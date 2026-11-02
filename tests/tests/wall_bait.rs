//! The wall alone, fast: one code published as a global contract and shared by two walls, the bait at
//! every gas level, and what the sentinel can and cannot do.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use sha2::Digest;
use std::sync::Arc;

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    async fn call(&self, from: &str, to: &str, method: &str, args: Value) -> String {
        let r = Contract(to.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(100))
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await;
        match r {
            Ok(o) if o.is_success() => "ok".into(),
            Ok(o) => {
                let d = format!("{o:?}");
                let m = d.split("Smart contract panicked: ").nth(1).unwrap_or("failed");
                format!("FAIL {}", m.split('"').next().unwrap_or(m))
            }
            Err(e) => format!("ERR {e}"),
        }
    }
    async fn config(&self, id: &str) -> Value {
        Contract(id.parse().unwrap()).call_function("get_config", json!({})).read_only::<Value>().fetch_from(&self.net).await.unwrap().data
    }
    async fn pending(&self, id: &str) -> usize {
        Contract(id.parse().unwrap()).call_function("get_pending", json!({})).read_only::<Value>().fetch_from(&self.net).await.unwrap().data.as_array().unwrap().len()
    }
    async fn account(&self, id: &str) -> (f64, u64) {
        let a = near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.unwrap().data;
        (a.amount.as_yoctonear() as f64 / 1e24, a.storage_usage)
    }
}
fn ok(label: &str, r: String) {
    println!("{label:<46} {r}");
    assert_eq!(r, "ok", "{label}");
}
fn fails(label: &str, r: String, with: &str) {
    println!("{label:<46} {r}");
    assert!(r.contains(with), "{label}: expected '{with}', got {r}");
}

#[tokio::test]
async fn wall_alone() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["kinu", "alice", "carol", "dave", "gd", "sentry", "thief", "wall", "wall2"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(20)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let code = std::fs::read(format!("{}/../target/near/kinu_outer_wall_short/kinu_outer_wall.wasm", env!("CARGO_MANIFEST_DIR")))?;

    println!("--- One code, many walls");
    let (before, _) = t.account("kinu.sandbox").await;
    Contract::deploy_global_contract_code(code.clone()).as_hash().with_signer("kinu.sandbox".parse()?, signer.clone()).send_to(&net).await?.assert_success();
    let (after, _) = t.account("kinu.sandbox").await;
    println!("publishing {} KB cost {:.4} NEAR (gone for good)", code.len() / 1024, before - after);
    let hash: [u8; 32] = sha2::Sha256::digest(&code).into();
    for (id, owner) in [("wall.sandbox", "alice.sandbox"), ("wall2.sandbox", "thief.sandbox")] {
        let init = json!({ "owner": owner, "canceller": "carol.sandbox", "sentinel": "sentry.sandbox", "home": "dave.sandbox", "guardian": "gd.sandbox", "delay_secs": 3600 });
        Contract::deploy(id.parse()?).use_global_hash(near_api::types::CryptoHash(hash)).with_init_call("new", init)?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
        let (near, storage) = t.account(id).await;
        let keys = near_api::Account(id.parse()?).list_keys().fetch_from(&net).await?.data;
        println!("{id}: storage {storage} B ({:.4} NEAR locked), keys {}, balance {near:.4}", storage as f64 * 1e19 / 1e24, keys.len());
        assert!(keys.is_empty(), "no key left on {id}");
        assert!(storage < 2_000, "the code is not in this account: {storage} B");
    }
    assert_eq!(t.config("wall2.sandbox").await["owner"], "thief.sandbox", "each wall has its own state");
    assert_eq!(t.config("wall.sandbox").await["owner"], "alice.sandbox");
    let a = "alice.sandbox";
    let w = "wall.sandbox";
    let send = json!({ "action": { "Send": { "token": null, "to": "thief.sandbox", "amount": "1" } } });

    println!("--- The sentinel can only stop");
    ok("thief asks to pay himself (0)", t.call(a, w, "request", send.clone()).await);
    fails("a stranger cannot cancel", t.call("thief.sandbox", w, "cancel", json!({ "id": 0 })).await, "Only the owner, the canceller or the sentinel");
    ok("the sentinel cancels, with its reason", t.call("sentry.sandbox", w, "cancel", json!({ "id": 0, "reason": "new receiver, large share of the balance" })).await);
    fails("no road for the sentinel: request", t.call("sentry.sandbox", w, "request", send.clone()).await, "Only the owner");
    fails("no road: send_home", t.call("sentry.sandbox", w, "send_home", json!({ "token": null, "amount": "1" })).await, "Only the owner or the canceller");
    fails("no road: guardian_call", t.call("sentry.sandbox", w, "guardian_call", json!({ "method": "withdraw", "args": "{}" })).await, "Only the owner");
    fails("no road: unfreeze", t.call("sentry.sandbox", w, "unfreeze", json!({})).await, "Only the canceller");
    fails("no road: dismiss itself or another", t.call("sentry.sandbox", w, "dismiss_sentinel", json!({})).await, "Only the canceller");
    ok("a rule change waits (1)", t.call(a, w, "request", json!({ "action": { "SetSentinel": { "sentinel": null } } })).await);
    fails("the sentinel cannot block its own removal", t.call("sentry.sandbox", w, "cancel", json!({ "id": 1 })).await, "Not for the sentinel");
    assert_eq!(t.pending(w).await, 1);
    println!("{:<46} {}", "the sentinel freezes", t.call("sentry.sandbox", w, "freeze", json!({})).await);
    assert_eq!(t.config(w).await["frozen"], true);
    assert_eq!(t.pending(w).await, 0, "the queue is empty");
    ok("the owner asks to unfreeze (2)", t.call(a, w, "request", json!({ "action": "Unfreeze" })).await);
    fails("the sentinel cannot block that either", t.call("sentry.sandbox", w, "cancel", json!({ "id": 2 })).await, "Not for the sentinel");
    ok("the canceller dismisses a bad sentinel", t.call("carol.sandbox", w, "dismiss_sentinel", json!({})).await);
    ok("the canceller unfreezes", t.call("carol.sandbox", w, "unfreeze", json!({})).await);
    fails("the sentinel is gone", t.call("sentry.sandbox", w, "freeze", json!({})).await, "Only the owner, the canceller or the sentinel");

    ok("a request (3)", t.call(a, w, "request", send.clone()).await);
    fails("a reason cannot be endless", t.call("carol.sandbox", w, "cancel", json!({ "id": 3, "reason": "x".repeat(300) })).await, "Reason too long");
    ok("a short one is fine", t.call("carol.sandbox", w, "cancel", json!({ "id": 3, "reason": "not mine" })).await);

    println!("--- The canceller vouches, the sentinel stands down");
    let w2 = "wall2.sandbox";
    ok("a request (0 on the second wall)", t.call("thief.sandbox", w2, "request", send.clone()).await);
    fails("the owner cannot vouch", t.call("thief.sandbox", w2, "vouch", json!({ "id": 0 })).await, "Only the canceller");
    fails("the sentinel cannot vouch", t.call("sentry.sandbox", w2, "vouch", json!({ "id": 0 })).await, "Only the canceller");
    fails("nothing to vouch for", t.call("carol.sandbox", w2, "vouch", json!({ "id": 9 })).await, "No such request");
    ok("the canceller vouches", t.call("carol.sandbox", w2, "vouch", json!({ "id": 0 })).await);
    fails("the sentinel stands down", t.call("sentry.sandbox", w2, "cancel", json!({ "id": 0 })).await, "The canceller vouched");
    ok("the canceller can still cancel", t.call("carol.sandbox", w2, "cancel", json!({ "id": 0 })).await);

    println!("--- The bait at every gas level");
    let bait = near_api::signer::generate_secret_key()?;
    let pk = bait.public_key();
    ok("owner plants the bait", t.call(a, w, "add_tripwire", json!({ "public_key": pk.to_string() })).await);
    let stolen = Signer::from_secret_key(bait)?;
    for tgas in [300, 200, 100, 60, 30] {
        let r = Contract(w.parse()?)
            .call_function("agent_send", json!({ "token": null, "to": "dave.sandbox", "amount": "1" }))
            .transaction()
            .gas(NearGas::from_tgas(tgas))
            .with_signer(w.parse()?, stolen.clone())
            .send_to(&net)
            .await;
        println!("{tgas:>3} TGas: {} | frozen {}", match r { Ok(_) => "reached the wall".to_string(), Err(e) => format!("{e:?}").chars().take(120).collect() }, t.config(w).await["frozen"]);
        if tgas >= 60 {
            assert_eq!(t.config(w).await["frozen"], true, "{tgas} TGas must spring the bait");
        }
        ok("the canceller unfreezes", t.call("carol.sandbox", w, "unfreeze", json!({})).await);
    }
    Ok(())
}
