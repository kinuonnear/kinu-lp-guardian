//! The mainnet Sputnik DAO code (v2.3.1) as Guardian's owner, on the CI v1.7: one vote does nothing, two
//! votes act, a stranger's vote is refused, and the owner's work runs through it. Local only.

use base64::Engine;
use near_api::{Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const G: &str = "guardian.sandbox";
const D: &str = "dao.sandbox";

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    async fn call(&self, from: &str, to: &str, m: &str, args: Value, dep: NearToken) -> String {
        let out = Contract(to.parse().unwrap()).call_function(m, args).transaction().gas(NearGas::from_tgas(300)).deposit(dep).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let err = dbg.find("Smart contract panicked: ").map(|i| dbg[i + 25..].chars().take(90).collect::<String>().split('"').next().unwrap_or("").to_string());
                match (r.is_success(), err) {
                    (true, None) => "ok".into(),
                    (_, Some(e)) => format!("FAIL {e}"),
                    (false, None) => format!("FAIL {}", dbg.chars().take(200).collect::<String>()),
                }
            }
            Err(e) => format!("ERR {e}").chars().take(200).collect(),
        }
    }
    async fn view(&self, to: &str, m: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(m, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    /// A DAO proposal that calls Guardian. Returns its id.
    async fn ask(&self, method: &str, args: Value, deposit: &str, gas: &str) -> (u64, Value) {
        let kind = json!({ "FunctionCall": { "receiver_id": G, "actions": [{ "method_name": method, "args": base64::engine::general_purpose::STANDARD.encode(args.to_string()), "deposit": deposit, "gas": gas }] } });
        let id = self.view(D, "get_last_proposal_id", json!({})).await.as_u64().unwrap();
        let r = self.call("kinuonnear.sandbox", D, "add_proposal", json!({ "proposal": { "description": method, "kind": kind } }), NearToken::from_near(0)).await;
        assert_eq!(r, "ok", "add_proposal {method}");
        (id, kind)
    }
    async fn status(&self, id: u64) -> Value {
        self.view(D, "get_proposal", json!({ "id": id })).await["status"].clone()
    }
}

#[tokio::test]
async fn a_two_of_two_dao_owns_guardian() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in [G, D, "kinuonnear.sandbox", "jhonveska.sandbox", "third.sandbox", "new.sandbox", "stranger.sandbox", "worker.sandbox", "dcl.sandbox", "wrap.sandbox"] {
        sandbox.create_account(name.parse()?).initial_balance(NearToken::from_near(30)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let policy = json!({
        "roles": [{ "name": "council", "kind": { "Group": ["kinuonnear.sandbox", "jhonveska.sandbox", "third.sandbox"] }, "permissions": ["*:*"], "vote_policy": {} }],
        "default_vote_policy": { "weight_kind": "RoleWeight", "quorum": "0", "threshold": [1, 2] },
        "proposal_bond": "0", "proposal_period": "604800000000000", "bounty_bond": "0", "bounty_forgiveness_period": "604800000000000"
    });
    Contract::deploy(D.parse()?).use_code(std::fs::read(std::env::var("DAO_WASM")?)?).with_init_call("new", json!({ "config": { "name": "kinu-guardian", "purpose": "Owner of KINU LP Guardian", "metadata": "" }, "policy": policy }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    Contract::deploy(G.parse()?).use_code(std::fs::read(std::env::var("CI_WASM")?)?).with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": D }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    println!("version {}", t.view(D, "version", json!({})).await);
    assert_eq!(t.call(D, G, "set_paused", json!({ "paused": true }), NearToken::from_near(0)).await, "ok");
    let paused = |cfg: Value| cfg["paused"] == true;
    assert!(paused(t.view(G, "get_config", json!({})).await));

    println!("--- resume through the DAO ---");
    let (id, kind) = t.ask("set_paused", json!({ "paused": false }), "0", "30000000000000").await;
    let vote = |who: &'static str, id: u64, kind: Value| {
        let t = &t;
        async move { t.call(who, D, "act_proposal", json!({ "id": id, "action": "VoteApprove", "proposal": kind }), NearToken::from_near(0)).await }
    };
    println!("stranger votes: {}", vote("stranger.sandbox", id, kind.clone()).await);
    assert_ne!(t.status(id).await, "Approved");
    println!("first vote: {}, status {}", vote("kinuonnear.sandbox", id, kind.clone()).await, t.status(id).await);
    assert!(paused(t.view(G, "get_config", json!({})).await), "one vote does nothing");
    println!("same member again: {}", vote("kinuonnear.sandbox", id, kind.clone()).await);
    assert!(paused(t.view(G, "get_config", json!({})).await), "one member twice does nothing");
    println!("second member: {}, status {}", vote("jhonveska.sandbox", id, kind.clone()).await, t.status(id).await);
    assert!(!paused(t.view(G, "get_config", json!({})).await), "two votes resume");

    println!("--- the owner's other work through the DAO ---");
    let (id, kind) = t.ask("propose", json!({ "action": { "SetPauser": { "pauser": "worker.sandbox" } } }), "0", "30000000000000").await;
    vote("kinuonnear.sandbox", id, kind.clone()).await;
    println!("propose SetPauser: {}, status {}", vote("jhonveska.sandbox", id, kind).await, t.status(id).await);
    let pending = t.view(G, "get_pending", json!({})).await;
    println!("pending {pending}");
    assert_eq!(pending.as_array().unwrap().len(), 1);
    let (id, kind) = t.ask("list_factories", json!({ "parents": ["sandbox"], "trusted": true }), "0", "30000000000000").await;
    vote("kinuonnear.sandbox", id, kind.clone()).await;
    println!("list_factories: {}, status {}", vote("jhonveska.sandbox", id, kind).await, t.status(id).await);
    assert_eq!(t.view(G, "get_listed", json!({ "token": "tk.sandbox" })).await, true);
    println!("{}", t.call("kinuonnear.sandbox", G, "set_paused", json!({ "paused": true }), NearToken::from_near(0)).await);
    assert!(!paused(t.view(G, "get_config", json!({})).await), "a member alone is no owner");

    println!("--- a member's wallet is lost: the other two replace it ---");
    for (kind, label) in [(json!({ "RemoveMemberFromRole": { "member_id": "third.sandbox", "role": "council" } }), "remove"), (json!({ "AddMemberToRole": { "member_id": "new.sandbox", "role": "council" } }), "add")] {
        let id = t.view(D, "get_last_proposal_id", json!({})).await.as_u64().unwrap();
        assert_eq!(t.call("kinuonnear.sandbox", D, "add_proposal", json!({ "proposal": { "description": label, "kind": kind } }), NearToken::from_near(0)).await, "ok");
        vote("kinuonnear.sandbox", id, kind.clone()).await;
        println!("{label}: {}, status {}", vote("jhonveska.sandbox", id, kind).await, t.status(id).await);
    }
    let roles = t.view(D, "get_policy", json!({})).await["roles"][0]["kind"].clone();
    println!("members {roles}");
    assert_eq!(roles["Group"].as_array().unwrap().len(), 3);
    let (id, kind) = t.ask("set_paused", json!({ "paused": true }), "0", "30000000000000").await;
    vote("kinuonnear.sandbox", id, kind.clone()).await;
    println!("the lost wallet cannot vote: {}", vote("third.sandbox", id, kind.clone()).await);
    println!("the new member completes it: {}, status {}", vote("new.sandbox", id, kind).await, t.status(id).await);
    assert!(paused(t.view(G, "get_config", json!({})).await), "two of the new three act");
    Ok(())
}

/// Steps 8 to 15 on the 10-minute build: the owner hands over, in the worse order, and nothing is lost.
#[tokio::test]
async fn handover_from_the_owner_to_the_dao() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in [G, D, "kinuonnear.sandbox", "jhonveska.sandbox", "third.sandbox", "worker.sandbox", "stranger.sandbox", "dcl.sandbox", "wrap.sandbox"] {
        sandbox.create_account(name.parse()?).initial_balance(NearToken::from_near(30)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let zero = NearToken::from_near(0);
    let policy = json!({ "roles": [{ "name": "council", "kind": { "Group": ["kinuonnear.sandbox", "jhonveska.sandbox", "third.sandbox"] }, "permissions": ["*:*"], "vote_policy": {} }], "default_vote_policy": { "weight_kind": "RoleWeight", "quorum": "0", "threshold": [1, 2] }, "proposal_bond": "0", "proposal_period": "604800000000000", "bounty_bond": "0", "bounty_forgiveness_period": "604800000000000" });
    Contract::deploy(D.parse()?).use_code(std::fs::read(std::env::var("DAO_WASM")?)?).with_init_call("new", json!({ "config": { "name": "kinu-guardian", "purpose": "owner", "metadata": "" }, "policy": policy }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    Contract::deploy(G.parse()?).use_code(std::fs::read(std::env::var("V17_SHORT")?)?).with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": "kinuonnear.sandbox" }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    assert_eq!(t.call("kinuonnear.sandbox", G, "set_paused", json!({ "paused": true }), zero).await, "ok");
    assert_eq!(t.call("kinuonnear.sandbox", G, "propose", json!({ "action": { "SetPauser": { "pauser": "worker.sandbox" } } }), zero).await, "ok");
    assert_eq!(t.call("kinuonnear.sandbox", G, "propose", json!({ "action": { "SetOwner": { "owner": D } } }), zero).await, "ok");
    println!("too early: {}", t.call("stranger.sandbox", G, "execute", json!({ "id": 1 }), zero).await);
    let start = t.view(G, "get_config", json!({})).await;
    println!("pending {}", start["pending"]);
    let mut wait = 0;
    while !t.call("stranger.sandbox", G, "execute", json!({ "id": 1 }), zero).await.eq("ok") {
        sandbox.fast_forward(600).await?;
        wait += 1;
        assert!(wait < 200, "never ready");
    }
    println!("SetOwner ran first, by a stranger: owner {}", t.view(G, "get_config", json!({})).await["owner"]);
    println!("the old owner: {}", t.call("kinuonnear.sandbox", G, "set_paused", json!({ "paused": false }), zero).await);
    assert_eq!(t.call("stranger.sandbox", G, "execute", json!({ "id": 0 }), zero).await, "ok", "SetPauser still runs under the new owner");
    assert_eq!(t.view(G, "get_config", json!({})).await["pauser"], "worker.sandbox");
    println!("the pauser cannot resume: {}", t.call("worker.sandbox", G, "set_paused", json!({ "paused": false }), zero).await);
    let (id, kind) = t.ask("set_paused", json!({ "paused": false }), "0", "30000000000000").await;
    for who in ["kinuonnear.sandbox", "jhonveska.sandbox"] {
        assert_eq!(t.call(who, D, "act_proposal", json!({ "id": id, "action": "VoteApprove", "proposal": kind }), zero).await, "ok");
    }
    assert_eq!(t.view(G, "get_config", json!({})).await["paused"], false, "resumed by two signatures");
    println!("PROPOSAL {}", t.view(D, "get_proposals", json!({ "from_index": 0, "limit": 5 })).await);
    assert_eq!(t.call("worker.sandbox", G, "pause", json!({}), zero).await, "ok", "the pause drill");
    assert_eq!(t.view(G, "get_config", json!({})).await["paused"], true);
    println!("DAO balance after a proposal: {:?}", near_api::Account(D.parse()?).view().fetch_from(&net).await?.data.amount);
    Ok(())
}

/// Step 14 and after: the contract key is gone and the DAO owns Guardian. A wallet that is no member stages
/// the code, the DAO proposes it, a stranger runs it after the wait. The DAO can still reach every repair.
#[tokio::test]
async fn the_dao_can_still_repair_after_the_key_is_gone() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in [G, D, "kinuonnear.sandbox", "jhonveska.sandbox", "third.sandbox", "worker.sandbox", "stranger.sandbox", "stager.sandbox", "alice.sandbox", "dcl.sandbox", "wrap.sandbox"] {
        sandbox.create_account(name.parse()?).initial_balance(NearToken::from_near(30)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let zero = NearToken::from_near(0);
    let policy = json!({ "roles": [{ "name": "council", "kind": { "Group": ["kinuonnear.sandbox", "jhonveska.sandbox", "third.sandbox"] }, "permissions": ["*:*"], "vote_policy": {} }], "default_vote_policy": { "weight_kind": "RoleWeight", "quorum": "0", "threshold": [1, 2] }, "proposal_bond": "0", "proposal_period": "604800000000000", "bounty_bond": "0", "bounty_forgiveness_period": "604800000000000" });
    Contract::deploy(D.parse()?).use_code(std::fs::read(std::env::var("DAO_WASM")?)?).with_init_call("new", json!({ "config": { "name": "kinu-guardian", "purpose": "owner", "metadata": "" }, "policy": policy }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    Contract::deploy(G.parse()?).use_code(std::fs::read(std::env::var("V17_SHORT")?)?).with_init_call("new", json!({ "dcl": "dcl.sandbox", "wnear": "wrap.sandbox", "owner": "kinuonnear.sandbox" }))?.with_signer(signer.clone()).send_to(&net).await?.assert_success();

    println!("--- the handover ---");
    assert_eq!(t.call("kinuonnear.sandbox", G, "propose", json!({ "action": { "SetPauser": { "pauser": "worker.sandbox" } } }), zero).await, "ok");
    assert_eq!(t.call("kinuonnear.sandbox", G, "propose", json!({ "action": { "SetOwner": { "owner": D } } }), zero).await, "ok");
    let mut wait = 0;
    while !t.call("stranger.sandbox", G, "execute", json!({ "id": 0 }), zero).await.eq("ok") {
        sandbox.fast_forward(600).await?;
        wait += 1;
        assert!(wait < 200, "never ready");
    }
    assert_eq!(t.call("stranger.sandbox", G, "execute", json!({ "id": 1 }), zero).await, "ok");
    assert_eq!(t.view(G, "get_config", json!({})).await["owner"], D);

    println!("--- the contract key is deleted ---");
    let pk = signer.get_public_key().await?;
    near_api::Account(G.parse()?).delete_key(pk).with_signer(signer.clone()).send_to(&net).await?.assert_success();
    let keys = near_api::Account(G.parse()?).list_keys().fetch_from(&net).await?;
    println!("keys on the contract: {}", keys.data.len());
    assert_eq!(keys.data.len(), 0);

    let vote = |id: u64, kind: Value| {
        let t = &t;
        async move {
            assert_eq!(t.call("kinuonnear.sandbox", D, "act_proposal", json!({ "id": id, "action": "VoteApprove", "proposal": kind }), NearToken::from_near(0)).await, "ok");
            assert_eq!(t.call("jhonveska.sandbox", D, "act_proposal", json!({ "id": id, "action": "VoteApprove", "proposal": kind }), NearToken::from_near(0)).await, "ok");
            let s = t.status(id).await;
            println!("proposal {id}: {s}");
            assert_eq!(s, "Approved");
        }
    };

    println!("--- a wallet with no role stages, the DAO proposes, a stranger runs ---");
    let hash = "Cs89ihQAYiDCbaV3c9ZxY7rsthKMWmDXR3ijQBb72E9p";
    let code = std::fs::read(std::env::var("CI_WASM")?)?;
    let before = near_api::Account("stager.sandbox".parse()?).view().fetch_from(&net).await?.data.amount;
    let stage = Contract(G.parse()?).call_function_raw("stage_code", code).transaction().gas(NearGas::from_tgas(150)).deposit(NearToken::from_millinear(4560)).with_signer("stager.sandbox".parse()?, signer.clone()).send_to(&net).await?;
    assert!(stage.is_success(), "stage: {stage:?}");
    assert_eq!(t.view(G, "get_config", json!({})).await["staged"]["code_hash"], hash);
    let (id, kind) = t.ask("propose", json!({ "action": { "Upgrade": { "code_hash": hash } } }), "0", "30000000000000").await;
    vote(id, kind).await;
    let pending = t.view(G, "get_pending", json!({})).await;
    println!("pending {pending}");
    let upgrade = pending[0][0].as_u64().unwrap();
    println!("too early: {}", t.call("stranger.sandbox", G, "execute", json!({ "id": upgrade }), zero).await);
    let mut wait = 0;
    while !t.call("stranger.sandbox", G, "execute", json!({ "id": upgrade }), zero).await.eq("ok") {
        sandbox.fast_forward(600).await?;
        wait += 1;
        assert!(wait < 200, "never ready");
    }
    let cfg = t.view(G, "get_config", json!({})).await;
    println!("after: version {}, delay {}, owner {}, pauser {}, staged {}", cfg["version"], cfg["admin_delay_secs"], cfg["owner"], cfg["pauser"], cfg["staged"]);
    assert_eq!(cfg["admin_delay_secs"], 172_800, "the new code runs");
    assert_eq!(cfg["owner"], D);
    assert_eq!(cfg["pauser"], "worker.sandbox");
    assert_eq!(cfg["staged"], Value::Null);
    let after = near_api::Account("stager.sandbox".parse()?).view().fetch_from(&net).await?.data.amount;
    println!("stager paid {} NEAR net", (before.as_yoctonear() as f64 - after.as_yoctonear() as f64) / 1e24);
    assert!(before.as_yoctonear() - after.as_yoctonear() < 100_000_000_000_000_000_000_000, "the deposit came back");

    println!("--- every repair is still reachable through the DAO ---");
    let (id, kind) = t.ask("propose", json!({ "action": { "RecoverCredit": { "user": "alice.sandbox", "token": "wrap.sandbox", "amount": "1" } } }), "0", "30000000000000").await;
    vote(id, kind).await;
    let (id, kind) = t.ask("propose", json!({ "action": { "RecoverPosition": { "user": "alice.sandbox", "lpt_id": "a|b|100#1" } } }), "0", "30000000000000").await;
    vote(id, kind).await;
    let pending = t.view(G, "get_pending", json!({})).await;
    println!("pending {pending}");
    assert_eq!(pending.as_array().unwrap().len(), 2);
    let first = pending[0][0].as_u64().unwrap();
    let second = pending[1][0].as_u64().unwrap();
    let (id, kind) = t.ask("cancel", json!({ "id": first }), "0", "30000000000000").await;
    vote(id, kind).await;
    let (id, kind) = t.ask("cancel", json!({ "id": second }), "0", "30000000000000").await;
    vote(id, kind).await;
    assert_eq!(t.view(G, "get_pending", json!({})).await.as_array().unwrap().len(), 0, "the DAO cancels");
    let (id, kind) = t.ask("list_tokens", json!({ "tokens": ["bad.sandbox"], "listed": false }), "0", "60000000000000").await;
    vote(id, kind).await;
    assert_eq!(t.view(G, "get_listed", json!({ "token": "bad.sandbox" })).await, false);
    let (id, kind) = t.ask("remove_pauser", json!({}), "0", "30000000000000").await;
    vote(id, kind).await;
    assert_eq!(t.view(G, "get_config", json!({})).await["pauser"], Value::Null, "a stolen pauser can be removed");
    println!("\nthe DAO can still repair");
    Ok(())
}
