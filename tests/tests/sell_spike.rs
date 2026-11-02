//! What Rhea really does with a swap that names a stop point and a recipient, before anything is built on it.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const POOL: &str = "tkq.sandbox|wrap.sandbox|2000";
const TKQ: &str = "tkq.sandbox";
const WRAP: &str = "wrap.sandbox";
const DCL: &str = "dcl.sandbox";
const E24: u128 = 1_000_000_000_000_000_000_000_000;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn built(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        let out = Contract(to.parse().unwrap()).call_function(method, args).transaction().gas(NearGas::from_tgas(300)).deposit(deposit).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let burnt = dbg.split("total_gas_burnt: NearGas { inner: ").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1e12;
                let logs: Vec<&str> = dbg.split("EVENT_JSON:").skip(1).map(|s| &s[..s.len().min(230)]).collect();
                format!("success={} {burnt:.1}T\n   {}", r.is_success(), logs.join("\n   "))
            }
            Err(e) => format!("ERR {e}").chars().take(400).collect(),
        }
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn ft(&self, token: &str, id: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": id })).await.as_str().unwrap_or("0").parse().unwrap_or(0)
    }
    async fn near(&self, id: &str) -> f64 {
        near_api::Account(id.parse().unwrap()).view().fetch_from(&self.net).await.map(|r| r.data.amount.as_yoctonear() as f64 / 1e24).unwrap_or(0.0)
    }
    async fn point(&self) -> i64 {
        self.view(DCL, "get_pool", json!({ "pool_id": POOL })).await["current_point"].as_i64().unwrap()
    }
    async fn state(&self, label: &str) {
        println!("{label}: point {} bob TKQ {} carol wNEAR {} carol NEAR {:.3} bob on Rhea {} lostfound {}", self.point().await, self.ft(TKQ, "bob.sandbox").await, self.ft(WRAP, "carol.sandbox").await, self.near("carol.sandbox").await, self.view(DCL, "list_user_assets", json!({ "account_id": "bob.sandbox" })).await, self.view(DCL, "get_lostfound", json!({ "account_id": "dave.sandbox" })).await);
    }
}

#[tokio::test]
async fn rhea_stop_point_and_recipient() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "bob", "carol", "dave"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(200)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let (yocto, zero, reg) = (NearToken::from_yoctonear(1), NearToken::from_near(0), NearToken::from_millinear(125));
    deploy(WRAP, fixture("wrap-near.wasm"), json!({})).await;
    deploy(DCL, kinu_guardian_tests::dcl_wasm(), json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    println!("{}", t.call("owner.sandbox", DCL, "extend_whitelisted_tokens", json!({ "token_ids": [TKQ, WRAP] }), yocto).await);
    println!("{}", t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": TKQ, "token_b": WRAP, "fee": 2000, "init_point": 304800 }), NearToken::from_near(1)).await);
    for who in [DCL, "lp.sandbox", "bob.sandbox", "carol.sandbox"] {
        for tok in [TKQ, WRAP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
    }
    t.call("owner.sandbox", WRAP, "storage_deposit", json!({ "account_id": "dave.sandbox", "registration_only": true }), reg).await;
    for who in ["lp.sandbox", "bob.sandbox"] {
        t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": "100000000000000000" }), yocto).await;
    }
    t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(60)).await;
    t.call("lp.sandbox", DCL, "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await;
    for (tok, amt) in [(TKQ, "50000000000000000".to_string()), (WRAP, (45 * E24).to_string())] {
        t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": amt, "msg": "\"Deposit\"" }), yocto).await;
    }
    println!("{}", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": POOL, "left_point": 284800, "right_point": 324800, "amount_x": "2000000000000", "amount_y": (20 * E24).to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);
    t.state("start").await;
    let cur = t.point().await;
    let swap = |stop: i64, to: Option<&str>, unwrap: bool| {
        let mut m = json!({ "pool_id": POOL, "output_token": WRAP, "stop_point": stop, "skip_unwrap_near": !unwrap });
        if let Some(to) = to {
            m["swap_out_recipient"] = json!(to);
        }
        json!({ "SwapByStopPoint": m }).to_string()
    };
    println!("\n1. stop far away, recipient carol, keep wNEAR");
    println!("{}", t.call("bob.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "100000000000", "msg": swap(cur - 300, Some("carol.sandbox"), false) }), yocto).await);
    t.state("after 1").await;
    let cur = t.point().await;
    println!("\n2. stop close by: a large amount fills only part");
    println!("{}", t.call("bob.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "20000000000000000", "msg": swap(cur - 20, Some("carol.sandbox"), false) }), yocto).await);
    t.state("after 2").await;
    let cur = t.point().await;
    println!("\n3. recipient carol, unwrapped to NEAR");
    println!("{}", t.call("bob.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "100000000000", "msg": swap(cur - 300, Some("carol.sandbox"), true) }), yocto).await);
    t.state("after 3").await;
    println!("\n4. stop above the price: nothing can fill");
    let cur = t.point().await;
    println!("{}", t.call("bob.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": "100000000000", "msg": swap(cur + 100, Some("carol.sandbox"), false) }), yocto).await);
    t.state("after 4").await;
    println!("\n5. recipient not registered on the output token (reversed: selling wNEAR into TKQ to dave)");
    let cur = t.point().await;
    t.call("bob.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(5)).await;
    let m = json!({ "SwapByStopPoint": { "pool_id": POOL, "output_token": TKQ, "stop_point": cur + 300, "skip_unwrap_near": true, "swap_out_recipient": "dave.sandbox" } }).to_string();
    println!("{}", t.call("bob.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": E24.to_string(), "msg": m }), yocto).await);
    println!("dave TKQ {} dave lostfound {} list_lostfound {} bob wNEAR {}", t.ft(TKQ, "dave.sandbox").await, t.view(DCL, "get_lostfound", json!({ "account_id": "dave.sandbox", "token_id": TKQ })).await, t.view(DCL, "list_lostfound", json!({ "from_index": 0, "limit": 10 })).await, t.ft(WRAP, "bob.sandbox").await);
    Ok(())
}
