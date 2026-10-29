//! The vault against the real Rhea DCL: the exact wasm of dclv2.ref-labs.near (2.4.0, code
//! FvsWeo3…) and wrap.near (DL2f5x…) from mainnet, in near-sandbox. Run ./scripts/build.sh first.

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub struct T {
    pub net: NetworkConfig,
    pub signer: Arc<Signer>,
}

impl T {
    /// A call's outcome as text: "ok" or the failure, for probing an unknown contract.
    pub async fn try_call(&self, from: &str, contract: &str, method: &str, args: Value, deposit: NearToken, tgas: u64) -> String {
        let out = Contract(contract.parse().unwrap())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(tgas))
            .deposit(deposit)
            .with_signer(from.parse().unwrap(), self.signer.clone())
            .send_to(&self.net)
            .await;
        // The contract's own messages: every ExecutionError / panic text anywhere in the outcome,
        // inner receipts included (a refused ft_on_transfer still reports the outer call as ok).
        let errors = |dbg: &str| -> Vec<String> {
            let mut found = Vec::new();
            for marker in ["ExecutionError(\"", "Smart contract panicked: "] {
                let mut rest = dbg;
                while let Some(i) = rest.find(marker) {
                    let tail = &rest[i + marker.len()..];
                    let end = tail.find('"').unwrap_or(tail.len().min(160));
                    let msg = tail[..end].to_string();
                    if !found.contains(&msg) {
                        found.push(msg);
                    }
                    rest = &tail[end..];
                }
            }
            found
        };
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let errs = errors(&dbg);
                let burnt = dbg
                    .split("total_gas_burnt: NearGas { inner: ")
                    .nth(1)
                    .and_then(|s| s.split(' ').next())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0)
                    / 1e12;
                if r.is_success() && errs.is_empty() {
                    format!("ok ({burnt:.1}T burnt)")
                } else {
                    format!("{} ({burnt:.1}T burnt): {}", if r.is_success() { "ok-outer" } else { "FAIL" }, errs.join(" | "))
                }
            }
            Err(e) => format!("ERR {e}").chars().take(300).collect(),
        }
    }
    pub async fn view(&self, contract: &str, method: &str, args: Value) -> Value {
        Contract(contract.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
}

fn built(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}

/// The real DCL and wrap.near, a 12-decimal token like QTC, and a pool tkq|wrap at 1% (spacing 200).
pub async fn real_dcl() -> testresult::TestResult<(near_sandbox::Sandbox, T)> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tkq", "lp", "agent", "vault"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(100)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, init: Value| {
        let (net, signer, id) = (net.clone(), signer.clone(), id.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call("new", init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    deploy("wrap.sandbox", fixture("wrap-near.wasm"), json!({})).await;
    deploy("dcl.sandbox", fixture("dcl-2.4.0.wasm"), json!({ "owner_id": "owner.sandbox", "wnear_id": "wrap.sandbox", "farming_contract_id": "farm.sandbox" })).await;
    deploy("tkq.sandbox", built("kinu_guardian_testtoken"), json!({ "owner_id": "owner.sandbox", "total_supply": "1000000000000000000", "symbol": "TKQ", "decimals": 12 })).await;
    // As on mainnet, both tokens are on the DCL's whitelist (deposits of other tokens fail with E016).
    let r = t.try_call("owner.sandbox", "dcl.sandbox", "extend_whitelisted_tokens", json!({ "token_ids": ["tkq.sandbox", "wrap.sandbox"] }), NearToken::from_yoctonear(1), 30).await;
    if !r.starts_with("ok") {
        println!("whitelist -> {r}");
    }
    for who in ["dcl.sandbox", "lp.sandbox"] {
        t.try_call("owner.sandbox", "tkq.sandbox", "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125), 30).await;
        t.try_call("owner.sandbox", "wrap.sandbox", "storage_deposit", json!({ "account_id": who, "registration_only": true }), NearToken::from_millinear(125), 30).await;
    }
    Ok((sandbox, t))
}

#[tokio::test]
async fn probe_gas_needs() -> testresult::TestResult<()> {
    let (_sandbox, t) = real_dcl().await?;
    let pool = "tkq.sandbox|wrap.sandbox|10000";
    println!("create_pool -> {}", t.try_call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": "tkq.sandbox", "token_b": "wrap.sandbox", "fee": 10000, "init_point": 0 }), NearToken::from_near(1), 100).await);
    println!("pool -> {}", t.view("dcl.sandbox", "get_pool", json!({ "pool_id": pool })).await);
    // The LP: registered on the DCL, holds TKQ and wNEAR, deposits both into its DCL balance.
    println!("lp register -> {}", t.try_call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500), 30).await);
    println!("tkq to lp -> {}", t.try_call("owner.sandbox", "tkq.sandbox", "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "10000000000000000" }), NearToken::from_yoctonear(1), 30).await);
    println!("wrap -> {}", t.try_call("lp.sandbox", "wrap.sandbox", "near_deposit", json!({}), NearToken::from_near(20), 30).await);
    for (tok, amt) in [("tkq.sandbox", "1000000000000000"), ("wrap.sandbox", "10000000000000000000000000")] {
        let r = t.try_call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), NearToken::from_yoctonear(1), 150).await;
        println!("deposit {tok} -> {r}");
    }
    println!("lp assets -> {}", t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "lp.sandbox" })).await);

    // add_liquidity across the price with both tokens (leftovers on one side), at 40 TGas.
    let add = |extra: Value| {
        let mut a = json!({ "pool_id": pool, "left_point": -1400, "right_point": 1400, "amount_x": "100000000000000", "amount_y": "1000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0" });
        if let Value::Object(m) = extra {
            for (k, v) in m {
                a[k] = v;
            }
        }
        a
    };
    for (label, extra, tgas) in [
        ("plain 40", json!({}), 40),
        ("plain 300", json!({}), 300),
        ("skip_refund_transfer 40", json!({ "skip_refund_transfer": true }), 40),
        ("skip_refund_transfer+skip_unwrap_near 40", json!({ "skip_refund_transfer": true, "skip_unwrap_near": true }), 40),
    ] {
        let r = t.try_call("lp.sandbox", "dcl.sandbox", "add_liquidity", add(extra), NearToken::from_near(0), tgas).await;
        println!("add_liquidity {label} -> {}", r.chars().take(160).collect::<String>());
        println!("   lp assets -> {}", t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "lp.sandbox" })).await);
    }

    // remove_liquidity: full liquidity of each position, at various gas; where do the tokens go?
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "lp.sandbox" })).await;
    for (pos, (tgas, skip)) in positions.as_array().cloned().unwrap_or_default().iter().zip([(30u64, true), (60, false), (110, false)]) {
        let before = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "lp.sandbox" })).await;
        let mut args = json!({ "lpt_id": pos["lpt_id"], "amount": pos["amount"], "min_amount_x": "0", "min_amount_y": "0" });
        if skip {
            args["skip_refund_transfer"] = json!(true);
            args["skip_unwrap_near"] = json!(true);
        }
        print!("[skip={skip}] ");
        let r = t.try_call("lp.sandbox", "dcl.sandbox", "remove_liquidity", args, NearToken::from_near(0), tgas).await;
        let after = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "lp.sandbox" })).await;
        println!("remove_liquidity {tgas}T -> {r}\n   assets {before} -> {after}");
    }

    // withdraw_asset at various gas, with and without skip_unwrap_near.
    for (tok, extra, tgas) in [
        ("tkq.sandbox", json!({}), 25),
        ("tkq.sandbox", json!({}), 40),
        ("tkq.sandbox", json!({}), 60),
        ("wrap.sandbox", json!({}), 25),
        ("wrap.sandbox", json!({}), 60),
        ("wrap.sandbox", json!({ "skip_unwrap_near": true }), 25),
        ("wrap.sandbox", json!({ "skip_unwrap_near": true }), 40),
    ] {
        let mut args = json!({ "token_id": tok, "amount": "1000000000" });
        if let Value::Object(m) = extra.clone() {
            for (k, v) in m {
                args[k] = v;
            }
        }
        let r = t.try_call("lp.sandbox", "dcl.sandbox", "withdraw_asset", args, NearToken::from_near(0), tgas).await;
        println!("withdraw {tok} {extra} {tgas}T -> {}", r.chars().take(160).collect::<String>());
    }
    Ok(())
}

/// The whole vault life on the real DCL 2.4.0: fund both tokens, the owner opens a range across the
/// price (leftovers), a real swap moves the price out, the agent rebalances, the owner exits,
/// withdraws everything and closes the vault. Every call with 300 TGas, as from a wallet.
#[tokio::test]
async fn vault_on_real_dcl() -> testresult::TestResult<()> {
    let (_sandbox, t) = real_dcl().await?;
    let pool = "tkq.sandbox|wrap.sandbox|10000";
    let ok = |label: &str, r: String| {
        println!("{label:<34} {r}");
        assert!(r.starts_with("ok ("), "{label}: {r}");
    };
    let zero = NearToken::from_near(0);
    let yocto = NearToken::from_yoctonear(1);
    ok("create_pool", t.try_call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": "tkq.sandbox", "token_b": "wrap.sandbox", "fee": 10000, "init_point": 0 }), NearToken::from_near(1), 100).await);

    // Deep liquidity from another LP, so a swap can move the price without draining the vault.
    t.try_call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500), 30).await;
    t.try_call("owner.sandbox", "tkq.sandbox", "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "100000000000000000" }), yocto, 30).await;
    t.try_call("lp.sandbox", "wrap.sandbox", "near_deposit", json!({}), NearToken::from_near(40), 30).await;
    for (tok, amt) in [("tkq.sandbox", "50000000000000000"), ("wrap.sandbox", "30000000000000000000000000")] {
        t.try_call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), yocto, 150).await;
    }
    ok("lp wide liquidity", t.try_call("lp.sandbox", "dcl.sandbox", "add_liquidity", json!({ "pool_id": pool, "left_point": -20000, "right_point": 20000, "amount_x": "20000000000000000", "amount_y": "20000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0" }), zero, 300).await);

    // The vault (v3 build), as the factory would create it, owned by owner.sandbox.
    let policy = json!({ "allowed_pools": [pool], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "rebalance_buffer_points": 100 });
    Contract::deploy("vault.sandbox".parse()?)
        .use_code(built("kinu_guardian_vault"))
        .with_init_call("new", json!({ "owner": "owner.sandbox", "agent": "agent.sandbox", "dcl": "dcl.sandbox", "policy": policy }))?
        .with_signer(t.signer.clone())
        .send_to(&t.net)
        .await?
        .assert_success();
    ok("register_dcl", t.try_call("owner.sandbox", "vault.sandbox", "register_dcl", json!({}), NearToken::from_millinear(500), 300).await);
    for tok in ["tkq.sandbox", "wrap.sandbox"] {
        ok("register_token", t.try_call("owner.sandbox", "vault.sandbox", "register_token", json!({ "token": tok }), NearToken::from_millinear(125), 300).await);
    }
    t.try_call("owner.sandbox", "wrap.sandbox", "near_deposit", json!({}), NearToken::from_near(5), 30).await;
    ok("fund TKQ", t.try_call("owner.sandbox", "tkq.sandbox", "ft_transfer_call", json!({ "receiver_id": "vault.sandbox", "amount": "1000000000000000", "msg": "" }), yocto, 300).await);
    ok("fund wNEAR", t.try_call("owner.sandbox", "wrap.sandbox", "ft_transfer_call", json!({ "receiver_id": "vault.sandbox", "amount": "2000000000000000000000000", "msg": "" }), yocto, 300).await);
    let assets = |t: &T| {
        let t = T { net: t.net.clone(), signer: t.signer.clone() };
        async move { t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "vault.sandbox" })).await }
    };
    println!("vault on Rhea after funding: {}", assets(&t).await);

    // The owner opens a range across the price with both tokens: the add leaves a leftover.
    ok("owner set_range (both tokens)", t.try_call("owner.sandbox", "vault.sandbox", "set_range", json!({ "request": { "pool_id": pool, "left_point": -1400, "right_point": 1400 } }), zero, 300).await);
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await;
    assert_eq!(positions.as_array().map(|a| a.len()), Some(1), "one position: {positions}");
    println!("vault on Rhea after open (leftover stays): {}", assets(&t).await);
    let v = t.view("vault.sandbox", "get_vault", json!({})).await;
    assert!(v["position"].is_object(), "the vault knows its position: {v}");

    // A real swap moves the price below the range (well past the 1% buffer).
    let sell = json!({ "Swap": { "pool_ids": [pool], "output_token": "wrap.sandbox", "min_output_amount": "0", "skip_unwrap_near": true } }).to_string();
    ok("swap TKQ -> wNEAR", t.try_call("lp.sandbox", "tkq.sandbox", "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "6000000000000000", "msg": sell }), yocto, 300).await);
    let point = t.view("dcl.sandbox", "get_pool", json!({ "pool_id": pool })).await["current_point"].as_i64().unwrap();
    println!("price point after swap: {point}");
    assert!(point < -1400 - 200, "the swap moved the price out of the range: {point}");

    // The agent re-enters one-sided just above the price (the position is all TKQ now).
    let left = (point + 20).div_euclid(200) * 200 + 200;
    ok("agent request_rebalance", t.try_call("agent.sandbox", "vault.sandbox", "request_rebalance", json!({ "request": { "pool_id": pool, "left_point": left, "right_point": left + 1400 } }), zero, 300).await);
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await;
    assert_eq!(positions.as_array().map(|a| a.len()), Some(1), "old closed, new open: {positions}");
    assert_eq!(positions[0]["left_point"], json!(left));
    println!("vault on Rhea after rebalance: {}", assets(&t).await);

    // The owner exits: the position closes and both tokens land in the vault's wallet as tokens.
    ok("owner exit", t.try_call("owner.sandbox", "vault.sandbox", "exit", json!({}), zero, 300).await);
    assert!(t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await.as_array().map(|a| a.is_empty()).unwrap_or(false));
    let wallet = |tok: &'static str| {
        let t = T { net: t.net.clone(), signer: t.signer.clone() };
        async move { t.view(tok, "ft_balance_of", json!({ "account_id": "vault.sandbox" })).await.as_str().unwrap_or("0").to_string() }
    };
    let (wq, ww) = (wallet("tkq.sandbox").await, wallet("wrap.sandbox").await);
    let rhea = assets(&t).await;
    println!("after exit: wallet TKQ {wq}, wNEAR {ww}; on Rhea {rhea}");

    // Withdraw everything (Rhea balance + wallet) to the owner, then close the vault.
    for tok in ["tkq.sandbox", "wrap.sandbox"] {
        let on_rhea = rhea[tok].as_str().unwrap_or("0").to_string();
        let in_wallet = wallet(tok).await;
        if on_rhea == "0" && in_wallet == "0" {
            continue;
        }
        ok("owner withdraw_idle", t.try_call("owner.sandbox", "vault.sandbox", "withdraw_idle", json!({ "token": tok, "from_rhea": on_rhea, "from_wallet": in_wallet }), zero, 300).await);
    }
    let empty = assets(&t).await;
    println!("after withdraw: on Rhea {empty}, wallet TKQ {}, wNEAR {}", wallet("tkq.sandbox").await, wallet("wrap.sandbox").await);
    ok("owner close_vault", t.try_call("owner.sandbox", "vault.sandbox", "close_vault", json!({}), zero, 300).await);
    assert!(near_api::Account("vault.sandbox".parse()?).view().fetch_from(&t.net).await.is_err(), "the vault account is gone");
    Ok(())
}

/// kinu-1's exact code (v2, 8SKHf63T, from mainnet) holding both tokens on the real DCL, as on
/// mainnet: every way out the owner has, tried. Records which ones work.
#[tokio::test]
async fn v2_vault_ways_out() -> testresult::TestResult<()> {
    let (_sandbox, t) = real_dcl().await?;
    let pool = "tkq.sandbox|wrap.sandbox|10000";
    let zero = NearToken::from_near(0);
    let yocto = NearToken::from_yoctonear(1);
    t.try_call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": "tkq.sandbox", "token_b": "wrap.sandbox", "fee": 10000, "init_point": 0 }), NearToken::from_near(1), 100).await;
    let policy = json!({ "allowed_pools": [pool], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 0, "rebalance_buffer_points": 100 });
    Contract::deploy("vault.sandbox".parse()?)
        .use_code(fixture("v2/vault.wasm"))
        .with_init_call("new", json!({ "owner": "owner.sandbox", "agent": "agent.sandbox", "dcl": "dcl.sandbox", "policy": policy }))?
        .with_signer(t.signer.clone())
        .send_to(&t.net)
        .await?
        .assert_success();
    t.try_call("owner.sandbox", "vault.sandbox", "register_dcl", json!({}), NearToken::from_millinear(500), 300).await;
    for tok in ["tkq.sandbox", "wrap.sandbox"] {
        t.try_call("owner.sandbox", "vault.sandbox", "register_token", json!({ "token": tok }), NearToken::from_millinear(125), 300).await;
    }
    t.try_call("owner.sandbox", "wrap.sandbox", "near_deposit", json!({}), NearToken::from_near(5), 30).await;
    t.try_call("owner.sandbox", "tkq.sandbox", "ft_transfer_call", json!({ "receiver_id": "vault.sandbox", "amount": "39072427408", "msg": "" }), yocto, 300).await;
    t.try_call("owner.sandbox", "wrap.sandbox", "ft_transfer_call", json!({ "receiver_id": "vault.sandbox", "amount": "1329088000000000000000000", "msg": "" }), yocto, 300).await;
    let assets = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "vault.sandbox" })).await;
    println!("v2 vault on Rhea (as kinu-1): {assets}");
    let tries = [
        ("withdraw_idle TKQ", "withdraw_idle", json!({ "token": "tkq.sandbox", "from_rhea": "39072427408", "from_wallet": "0" })),
        ("withdraw_idle wNEAR", "withdraw_idle", json!({ "token": "wrap.sandbox", "from_rhea": "1329088000000000000000000", "from_wallet": "0" })),
        ("set_range across the price", "set_range", json!({ "request": { "pool_id": pool, "left_point": -1400, "right_point": 1400 } })),
        ("set_range above (TKQ side)", "set_range", json!({ "request": { "pool_id": pool, "left_point": 200, "right_point": 1600 } })),
        ("set_range below (wNEAR side)", "set_range", json!({ "request": { "pool_id": pool, "left_point": -1600, "right_point": -200 } })),
        ("set_range very wide", "set_range", json!({ "request": { "pool_id": pool, "left_point": -400000, "right_point": 400000 } })),
        ("close_vault", "close_vault", json!({})),
    ];
    for (label, method, args) in tries {
        let r = t.try_call("owner.sandbox", "vault.sandbox", method, args, zero, 300).await;
        let after = t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "vault.sandbox" })).await;
        let pos = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "vault.sandbox" })).await;
        println!("{label:<30} {}\n   on Rhea {after}, positions {}", r.chars().take(150).collect::<String>(), pos.as_array().map(|a| a.len()).unwrap_or(0));
    }
    Ok(())
}

/// How many positions one Rhea account can hold, and whether more storage NEAR raises the limit.
#[tokio::test]
async fn probe_position_slots() -> testresult::TestResult<()> {
    let (_sandbox, t) = real_dcl().await?;
    let pool = "tkq.sandbox|wrap.sandbox|10000";
    let yocto = NearToken::from_yoctonear(1);
    t.try_call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": "tkq.sandbox", "token_b": "wrap.sandbox", "fee": 10000, "init_point": 0 }), NearToken::from_near(1), 100).await;
    t.try_call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500), 30).await;
    t.try_call("owner.sandbox", "tkq.sandbox", "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "10000000000000000" }), yocto, 30).await;
    t.try_call("lp.sandbox", "tkq.sandbox", "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": "10000000000000000", "msg": "\"Deposit\"" }), yocto, 150).await;
    let storage = |t: &T| {
        let t = T { net: t.net.clone(), signer: t.signer.clone() };
        async move { t.view("dcl.sandbox", "get_user_storage_detail", json!({ "user_id": "lp.sandbox" })).await }
    };
    println!("start: {}", storage(&t).await);
    let mut opened = 0;
    let mut extra = 0;
    for i in 0..70 {
        let left = 200 + 200 * (i % 30);
        let r = t.try_call("lp.sandbox", "dcl.sandbox", "add_liquidity", json!({ "pool_id": pool, "left_point": left, "right_point": left + 1400, "amount_x": "1000000000000", "amount_y": "0", "min_amount_x": "0", "min_amount_y": "0", "skip_refund_transfer": true, "skip_unwrap_near": true }), NearToken::from_near(0), 300).await;
        if r.starts_with("ok") {
            opened += 1;
            continue;
        }
        println!("position {} refused: {}", opened + 1, r.chars().take(200).collect::<String>());
        println!("   storage: {}", storage(&t).await);
        if extra >= 2 {
            break;
        }
        // Does more storage NEAR raise the limit?
        extra += 1;
        let r = t.try_call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_near(1), 30).await;
        println!("   +1 NEAR storage -> {r}");
        println!("   storage: {}", storage(&t).await);
    }
    println!("opened {opened} positions; final storage {}", storage(&t).await);
    Ok(())
}

#[tokio::test]
async fn probe_dcl_init() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm"] {
        let id: AccountId = format!("{name}.sandbox").parse()?;
        sandbox.create_account(id).initial_balance(NearToken::from_near(100)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    Contract::deploy("wrap.sandbox".parse()?).use_code(fixture("wrap-near.wasm")).without_init_call().with_signer(signer.clone()).send_to(&net).await?.assert_success();
    println!("wrap new() -> {}", t.try_call("wrap.sandbox", "wrap.sandbox", "new", json!({}), NearToken::from_near(0), 100).await);
    Contract::deploy("dcl.sandbox".parse()?).use_code(fixture("dcl-2.4.0.wasm")).without_init_call().with_signer(signer.clone()).send_to(&net).await?.assert_success();
    for args in [
        json!({}),
        json!({ "owner_id": "owner.sandbox" }),
        json!({ "owner_id": "owner.sandbox", "wnear_id": "wrap.sandbox" }),
        json!({ "owner_id": "owner.sandbox", "wnear_id": "wrap.sandbox", "farming_contract_id": "farm.sandbox" }),
    ] {
        let r = t.try_call("dcl.sandbox", "dcl.sandbox", "new", args.clone(), NearToken::from_near(0), 100).await;
        println!("dcl new({args}) -> {r}");
        if r.starts_with("ok (") {
            break;
        }
    }
    println!("metadata {}", t.view("dcl.sandbox", "get_metadata", json!({})).await);
    println!("fee tiers {}", t.view("dcl.sandbox", "get_fee_tiers", json!({})).await);
    Ok(())
}

/// What add_liquidity and remove_liquidity return on the real DCL (the shared contract keeps a
/// per-user ledger from these).
#[tokio::test]
async fn probe_return_values() -> testresult::TestResult<()> {
    let (_sandbox, t) = real_dcl().await?;
    let pool = "tkq.sandbox|wrap.sandbox|10000";
    let yocto = NearToken::from_yoctonear(1);
    t.try_call("owner.sandbox", "dcl.sandbox", "create_pool", json!({ "token_a": "tkq.sandbox", "token_b": "wrap.sandbox", "fee": 10000, "init_point": 0 }), NearToken::from_near(1), 100).await;
    t.try_call("lp.sandbox", "dcl.sandbox", "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500), 30).await;
    t.try_call("owner.sandbox", "tkq.sandbox", "ft_transfer", json!({ "receiver_id": "lp.sandbox", "amount": "10000000000000000" }), yocto, 30).await;
    t.try_call("lp.sandbox", "wrap.sandbox", "near_deposit", json!({}), NearToken::from_near(20), 30).await;
    for (tok, amt) in [("tkq.sandbox", "1000000000000000"), ("wrap.sandbox", "10000000000000000000000000")] {
        t.try_call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": "dcl.sandbox", "amount": amt, "msg": "\"Deposit\"" }), yocto, 150).await;
    }
    let raw = |method: &str, args: Value| {
        let t = T { net: t.net.clone(), signer: t.signer.clone() };
        let method = method.to_string();
        async move {
            let r = Contract("dcl.sandbox".parse().unwrap()).call_function(&method, args).transaction().gas(NearGas::from_tgas(300)).with_signer("lp.sandbox".parse().unwrap(), t.signer.clone()).send_to(&t.net).await.unwrap();
            let dbg = format!("{r:?}");
            // SuccessValue(...) of the final outcome.
            let i = dbg.find("status: SuccessValue(").map(|i| i + "status: SuccessValue(".len()).unwrap_or(0);
            let s: String = dbg[i..].chars().take(200).collect();
            match r.into_result().ok().and_then(|ok| ok.json::<Value>().ok()) { Some(v) => format!("json {v}"), None => format!("raw {s}") }
        }
    };
    let add = raw("add_liquidity", json!({ "pool_id": pool, "left_point": -1400, "right_point": 1400, "amount_x": "100000000000000", "amount_y": "1000000000000000000000000", "min_amount_x": "0", "min_amount_y": "0", "skip_refund_transfer": true, "skip_unwrap_near": true })).await;
    println!("add_liquidity returns: {add}");
    let positions = t.view("dcl.sandbox", "list_liquidities", json!({ "account_id": "lp.sandbox" })).await;
    println!("list_liquidities: {positions}");
    let p = &positions[0];
    let rem = raw("remove_liquidity", json!({ "lpt_id": p["lpt_id"], "amount": p["amount"], "min_amount_x": "0", "min_amount_y": "0", "skip_refund_transfer": true, "skip_unwrap_near": true })).await;
    println!("remove_liquidity returns: {rem}");
    println!("assets after: {}", t.view("dcl.sandbox", "list_user_assets", json!({ "account_id": "lp.sandbox" })).await);
    Ok(())
}
