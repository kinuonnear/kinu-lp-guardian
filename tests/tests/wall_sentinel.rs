//! The sentinel's rules against the real wall: it reads the chain the way the TEE build does, decides
//! with the same `plan`, and signs as its own account. Nobody else touches the wall in the stolen scenes.

use kinu_sentinel::{Move, Request, WallConfig, plan};
use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const W: &str = "wall.sandbox";
const OWNER: &str = "alice.sandbox";
const CANCELLER: &str = "carol.sandbox";
const SENTRY: &str = "sentry.sandbox";
const HOME: &str = "dave.sandbox";
const THIEF: &str = "thief.sandbox";

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
    rpc: String,
}
impl T {
    async fn call(&self, from: &str, method: &str, args: Value) -> String {
        let r = Contract(W.parse().unwrap()).call_function(method, args).transaction().gas(NearGas::from_tgas(100)).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
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
    async fn view(&self, method: &str) -> Value {
        Contract(W.parse().unwrap()).call_function(method, json!({})).read_only::<Value>().fetch_from(&self.net).await.unwrap().data
    }
    async fn near(&self, id: &str) -> f64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.unwrap().data.amount.as_yoctonear() as f64 / 1e24
    }
    async fn now(&self) -> u64 {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "block", "params": { "finality": "final" } });
        let v: Value = reqwest::Client::new().post(&self.rpc).json(&body).send().await.unwrap().json().await.unwrap();
        v["result"]["header"]["timestamp"].as_u64().unwrap() / 1_000_000_000
    }
    /// One sentinel run, as the TEE build does it.
    async fn look(&self) -> String {
        let config: WallConfig = serde_json::from_value(self.view("get_config").await).unwrap();
        let pending: Vec<Request> = serde_json::from_value(self.view("get_pending").await).unwrap();
        let p = plan(&config, &pending, self.now().await);
        let flags: Vec<String> = p.flags.iter().map(|f| format!("#{} {}", f.id, f.why)).collect();
        let did = match p.act {
            Move::Nothing => "nothing".to_string(),
            Move::Cancel { id, reason } => format!("cancel {id}: {}", self.call(SENTRY, "cancel", json!({ "id": id, "reason": reason })).await),
            Move::Freeze { reason } => format!("freeze ({reason}): {}", self.call(SENTRY, "freeze", json!({})).await),
        };
        println!("sentinel sees [{}] -> {did}", flags.join("; "));
        did
    }
    async fn wait(&self, sandbox: &near_sandbox::Sandbox, until: u64) {
        while self.now().await < until {
            sandbox.fast_forward(200).await.unwrap();
        }
    }
    async fn unlock(&self) -> u64 {
        self.view("get_pending").await[0]["unlock_secs"].as_u64().unwrap()
    }
}
fn ok(label: &str, r: String) {
    println!("{label:<46} {r}");
    assert_eq!(r, "ok", "{label}");
}

/// Every request the wall can hold, as the wall really returns it, read by the sentinel's own types
/// and given the rule it should get. The wall's fields and the sentinel's must never drift apart.
#[tokio::test]
async fn every_request_the_wall_returns_is_read_and_judged() -> testresult::TestResult<()> {
    use kinu_sentinel::{Call, RULES, judge};
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["alice", "carol", "dave", "gd", "sentry", "thief", "wall"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(20)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone(), rpc: sandbox.rpc_addr.clone() };
    let code = std::fs::read(format!("{}/../target/near/kinu_outer_wall_short/kinu_outer_wall.wasm", env!("CARGO_MANIFEST_DIR")))?;
    let init = json!({ "owner": OWNER, "canceller": CANCELLER, "sentinel": SENTRY, "home": HOME, "guardian": "gd.sandbox", "delay_secs": 3600 });
    Contract::deploy(W.parse()?).use_code(code).with_init_call("new", init)?.with_signer(signer.clone()).send_to(&net).await?.assert_success();

    let key = near_api::signer::generate_secret_key()?.public_key().to_string();
    let token = "gd.sandbox";
    let stop = |i: usize| Call::Stop(RULES[i]);
    let cases: Vec<(Value, Call)> = vec![
        (json!({ "Send": { "token": null, "to": THIEF, "amount": "1" } }), stop(0)),
        (json!({ "Send": { "token": token, "to": THIEF, "amount": "340282366920938463463374607431768211455" } }), stop(0)),
        (json!({ "Send": { "token": null, "to": HOME, "amount": "1" } }), Call::Leave),
        (json!({ "Guardian": { "method": "set_agent", "args": "{\"agent\":\"thief.sandbox\"}", "deposit": "1" } }), stop(8)),
        (json!({ "Guardian": { "method": "open", "args": "{}", "deposit": "0" } }), stop(8)),
        (json!({ "SetHome": { "home": "gd.sandbox" } }), stop(2)),
        (json!({ "SetCanceller": { "canceller": THIEF } }), stop(3)),
        (json!({ "SetSentinel": { "sentinel": null } }), Call::Alarm(RULES[9])),
        (json!({ "SetSentinel": { "sentinel": THIEF } }), Call::Alarm(RULES[9])),
        (json!({ "SetDelay": { "delay_secs": 3600 } }), Call::Leave),
        (json!({ "SetAgent": { "agent": THIEF, "receivers": [THIEF], "limits": [{ "token": null, "per_day": "9" }, { "token": token, "per_day": "9" }] } }), stop(7)),
        (json!({ "RemoveTripwire": { "public_key": key } }), stop(4)),
        (json!("Close"), stop(5)),
    ];
    for (action, _) in &cases {
        ok(&format!("queue {}", action.to_string().chars().take(34).collect::<String>()), t.call(OWNER, "request", json!({ "action": action })).await);
    }
    let config: WallConfig = serde_json::from_value(t.view("get_config").await)?;
    assert_eq!(config.home, HOME);
    assert_eq!(config.delay_secs, 3600);
    assert_eq!(config.sentinel.as_deref(), Some(SENTRY));
    let raw = t.view("get_pending").await;
    let pending: Vec<Request> = serde_json::from_value(raw)?;
    assert_eq!(pending.len(), cases.len());
    for (r, (action, call)) in pending.iter().zip(&cases) {
        println!("#{:<2} {:?}", r.id, judge(&config, r));
        assert_eq!(&judge(&config, r), call, "{action}");
        assert!(!r.by_agent && !r.vouched);
        assert!(r.unlock_secs > 0);
    }

    println!("--- Unfreeze, and what the sentinel does about the rest");
    ok("the sentinel freezes", t.call(SENTRY, "freeze", json!({})).await);
    ok("the owner asks to unfreeze", t.call(OWNER, "request", json!({ "action": "Unfreeze" })).await);
    let config: WallConfig = serde_json::from_value(t.view("get_config").await)?;
    assert!(config.frozen);
    let pending: Vec<Request> = serde_json::from_value(t.view("get_pending").await)?;
    assert_eq!(pending.len(), 1);
    assert_eq!(judge(&config, &pending[0]), Call::Alarm(RULES[9]));
    Ok(())
}

#[tokio::test]
async fn sentinel_on_the_real_wall() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["alice", "carol", "dave", "gd", "sentry", "thief", "wall"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(20)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone(), rpc: sandbox.rpc_addr.clone() };
    let code = std::fs::read(format!("{}/../target/near/kinu_outer_wall_short/kinu_outer_wall.wasm", env!("CARGO_MANIFEST_DIR")))?;
    let init = json!({ "owner": OWNER, "canceller": CANCELLER, "sentinel": SENTRY, "home": HOME, "guardian": "gd.sandbox", "delay_secs": 3600 });
    Contract::deploy(W.parse()?).use_code(code).with_init_call("new", init)?.with_signer(signer.clone()).send_to(&net).await?.assert_success();
    let pay_thief = json!({ "action": { "Send": { "token": null, "to": THIEF, "amount": "1000000000000000000000000" } } });
    let new_home = json!({ "action": { "SetHome": { "home": THIEF } } });

    println!("--- Stolen owner key, nobody awake");
    ok("thief asks to pay himself", t.call(OWNER, "request", pay_thief.clone()).await);
    assert_eq!(t.look().await, "nothing", "the first half of the wait is a person's");
    let half = t.unlock().await - 1800;
    t.wait(&sandbox, half).await;
    assert!(t.look().await.ends_with(": ok"));
    assert_eq!(t.view("get_pending").await.as_array().unwrap().len(), 0);

    println!("--- An honest payment, vouched");
    ok("the owner pays a new shop", t.call(OWNER, "request", pay_thief.clone()).await);
    let id = t.view("get_pending").await[0]["id"].as_u64().unwrap();
    ok("the canceller vouches", t.call(CANCELLER, "vouch", json!({ "id": id })).await);
    assert_eq!(t.look().await, "nothing");
    let unlock = t.unlock().await;
    t.wait(&sandbox, unlock).await;
    assert_eq!(t.look().await, "nothing", "still left alone at the end of the wait");
    let before = t.near(THIEF).await;
    ok("the wait is over, it runs", t.call(OWNER, "execute", json!({ "id": id })).await);
    assert!(t.near(THIEF).await - before > 0.99, "the shop was paid");

    println!("--- Two stolen requests at once");
    ok("thief asks to pay himself", t.call(OWNER, "request", pay_thief.clone()).await);
    ok("thief asks to change home", t.call(OWNER, "request", new_home).await);
    let half = t.unlock().await; // the later of the two is due last
    t.wait(&sandbox, half - 1790).await;
    assert!(t.look().await.contains("freeze"));
    assert_eq!(t.view("get_config").await["frozen"], true);
    assert_eq!(t.view("get_pending").await.as_array().unwrap().len(), 0);
    assert_eq!(t.look().await, "nothing", "a frozen wall needs nothing more");
    let home = t.near(HOME).await;
    ok("the way home still works", t.call(OWNER, "send_home", json!({ "token": null, "amount": "1000000000000000000000000" })).await);
    assert!(t.near(HOME).await - home > 0.99);
    Ok(())
}
