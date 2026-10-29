//! OutLayer entry point. Input (stdin JSON, untrusted): {"cmd":"pubkey"}, {"cmd":"tick","users":[..]}
//! or {"cmd":"retire"}. Output (stdout, at most 900 bytes): what the agent read, decided and sent.
//!
//! The agent's NEAR account is the implicit account of its TEE key `agent` (manifest: bind "wasm"):
//! the keystore derives it from this exact build and the caller, the module never sees it, and a new
//! build has a new key that users must name as their agent again.

#[cfg(target_arch = "wasm32")]
mod host {
    pub mod rpc_host {
        wit_bindgen::generate!({ world: "rpc-test", path: "wit-rpc" });
    }
    pub mod keys_host {
        wit_bindgen::generate!({ world: "signing-keys-host", path: "wit-keys" });
    }
    pub use keys_host::outlayer::signing_keys::api as keys;
    pub use rpc_host::near::rpc::api as rpc;
}

/// Embedded as the `outlayer.manifest` custom section, covered by the wasm's sha256.
#[cfg(target_arch = "wasm32")]
#[link_section = "outlayer.manifest"]
#[used]
static MANIFEST: [u8; include_bytes!("../manifest.json").len()] = *include_bytes!("../manifest.json");

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("Build for wasm32-wasip2 and run on OutLayer; `cargo test` covers the logic.");
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use std::io::{Read, Write};
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let out = match run(&input) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    };
    let mut s = out.to_string();
    s.truncate(900);
    print!("{s}");
    let _ = std::io::stdout().flush();
}

#[cfg(target_arch = "wasm32")]
mod run_impl {
    use super::host::{keys, rpc};
    use kinu_guardian_agent::*;
    use serde_json::{Value, json};

    /// The shared Guardian contract this build serves, and the code it must run (cargo-near build of
    /// contracts/guardian; scripts/codehash.py). A changed contract needs a new agent build.
    pub const GUARDIAN: &str = "lp.kinuonnear.near";
    pub const GUARDIAN_CODE_HASH: &str = "3M8vEyfNDVxHQTNUwafhSmPsG6FfRBy5eqnHQLG1iYot";
    pub const DCL: &str = "dclv2.ref-labs.near";
    /// Where `retire` returns the agent's gas: KINU's account, which funds the agent.
    pub const RETIRE_TO: &str = "kinuonnear.near";
    const GAS: u64 = 300_000_000_000_000;

    fn view(contract: &str, method: &str, args: Value) -> Result<Value, String> {
        let (res, err) = rpc::view(contract, method, &args.to_string(), "final");
        if !err.is_empty() {
            return Err(format!("{contract}.{method}: {err}"));
        }
        serde_json::from_str(&res).map_err(|e| format!("{contract}.{method}: {e}"))
    }

    /// view_account, view_access_key, block and send_tx answer with the whole JSON-RPC response.
    fn rpc_result(raw: &str) -> Result<Value, String> {
        let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if let Some(e) = v.get("error") {
            return Err(e.to_string().chars().take(200).collect());
        }
        Ok(v.get("result").cloned().unwrap_or(v))
    }

    fn my_key() -> Result<[u8; 32], String> {
        let pk = keys::public_key("agent", None).map_err(|e| format!("public_key: {e}"))?;
        pk.try_into().map_err(|_| "public key is not 32 bytes".to_string())
    }

    /// Builds, signs (in the TEE) and sends one function call from the agent's implicit account.
    fn send(me: &str, pk: &[u8; 32], receiver: &str, method: &str, args: String) -> Result<String, String> {
        let call = NearAction::FunctionCall(FunctionCall { method_name: method.to_string(), args: args.into_bytes(), gas: GAS, deposit: 0 });
        send_actions(me, pk, receiver, vec![call])
    }

    fn send_actions(me: &str, pk: &[u8; 32], receiver: &str, actions: Vec<NearAction>) -> Result<String, String> {
        let (ak, err) = rpc::view_access_key(me, &near_public_key(pk), "final");
        if !err.is_empty() {
            return Err(format!("access key: {err} (fund the agent account {me})"));
        }
        let ak = rpc_result(&ak)?;
        let nonce = ak["nonce"].as_u64().ok_or("no nonce")? + 1;
        let block_hash: [u8; 32] = bs58::decode(ak["block_hash"].as_str().ok_or("no block hash")?)
            .into_vec()
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "bad block hash")?;
        let tx = Transaction {
            signer_id: me.to_string(),
            public_key: PublicKey::Ed25519(*pk),
            nonce,
            receiver_id: receiver.to_string(),
            block_hash,
            actions,
        };
        // The key signs the hash of a transaction this module composed from checked chain state.
        let sig = keys::sign("agent", None, &tx.hash()).map_err(|e| format!("sign: {e}"))?;
        let signature: [u8; 64] = sig.try_into().map_err(|_| "signature is not 64 bytes")?;
        let signed = SignedTransaction { transaction: tx, signature: Signature::Ed25519(signature) };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(borsh::to_vec(&signed).map_err(|e| e.to_string())?);
        let (res, err) = rpc::send_tx(&b64, "FINAL");
        if !err.is_empty() {
            return Err(format!("send_tx: {err}"));
        }
        let v = rpc_result(&res)?;
        let failed = v["status"].get("Failure").is_some();
        let hash = v["transaction"]["hash"].as_str().unwrap_or("?").to_string();
        if failed {
            return Err(format!("tx {hash} failed: {}", v["status"]["Failure"].to_string().chars().take(120).collect::<String>()));
        }
        Ok(hash)
    }

    pub fn run(input: &str) -> Result<Value, String> {
        let req: Value = serde_json::from_str(input).unwrap_or(Value::Null);
        let pk = my_key()?;
        let me = implicit_account(&pk);
        if req["cmd"] == "pubkey" {
            return Ok(json!({ "ok": true, "agent_account": me, "public_key": near_public_key(&pk) }));
        }
        if req["cmd"] == "retire" {
            // Hands the agent's gas back to KINU (a constant of this build, never from the input),
            // for a build that is being replaced.
            let (mine, err) = rpc::view_account(&me, "final");
            if !err.is_empty() {
                return Err(format!("agent account: {err}"));
            }
            let amount: u128 = rpc_result(&mine)?["amount"].as_str().and_then(|a| a.parse().ok()).ok_or("no balance")?;
            let keep: u128 = 10_000_000_000_000_000_000_000; // 0.01 NEAR for this transaction and storage
            if amount <= keep {
                return Ok(json!({ "ok": true, "agent": me, "action": "retire: nothing to return" }));
            }
            let tx = send_actions(&me, &pk, RETIRE_TO, vec![NearAction::Transfer(amount - keep)])?;
            return Ok(json!({ "ok": true, "agent": me, "action": "retire", "to": RETIRE_TO, "yocto": (amount - keep).to_string(), "tx": tx }));
        }
        if req["cmd"] != "tick" {
            return Err("input needs {\"cmd\":\"tick\",\"users\":[...]}, pubkey or retire".into());
        }
        // Only the published contract: if the Guardian account ever runs other code, the agent stops.
        let (acct, err) = rpc::view_account(GUARDIAN, "final");
        if !err.is_empty() {
            return Err(format!("guardian account: {err}"));
        }
        let runs = rpc_result(&acct)?["code_hash"].as_str().unwrap_or("").to_string();
        if runs != GUARDIAN_CODE_HASH {
            return Ok(json!({ "ok": true, "agent": me, "action": "noop", "why": "the Guardian account does not run the published code", "code_hash": runs }));
        }
        let users = users_from_input(&req);
        if users.is_empty() {
            return Err("no valid users in the input".into());
        }
        // One pool read per pool, shared by the users in it.
        let mut pools: Vec<PoolView> = Vec::new();
        let mut out = Vec::new();
        for user in &users {
            out.push(tick_one(&me, &pk, user, &mut pools).unwrap_or_else(|e| json!({ "u": user, "err": e.chars().take(120).collect::<String>() })));
        }
        Ok(json!({ "ok": true, "agent": me, "results": out }))
    }

    fn tick_one(me: &str, pk: &[u8; 32], user: &str, pools: &mut Vec<PoolView>) -> Result<Value, String> {
        let a = view(GUARDIAN, "get_account", json!({ "user": user }))?;
        if a.is_null() {
            return Ok(json!({ "u": user, "a": "noop", "why": "not joined" }));
        }
        let account: AccountView = serde_json::from_value(a).map_err(|e| e.to_string())?;
        let pool_id = account.position.as_ref().map(|p| p.pool_id.clone()).or_else(|| account.policy.allowed_pools.first().cloned()).ok_or("no pool")?;
        let pool = match pools.iter().find(|p| p.pool_id == pool_id) {
            Some(p) => p.clone(),
            None => {
                let p: PoolView = serde_json::from_value(view(DCL, "get_pool", json!({ "pool_id": pool_id }))?).map_err(|e| e.to_string())?;
                pools.push(p.clone());
                p
            }
        };
        let action = decide(&World { me, account: &account, pool: &pool });
        // Cost guard: move a position only when a day of fees in the new range pays for the gas.
        // If the market data is unreachable the move goes ahead (a stuck position earns nothing).
        let mut guard = Value::Null;
        if let (Action::Rebalance { left_point, right_point, .. }, Some(old)) = (&action, &account.position) {
            match day_of_fees(&pool, old, *left_point, *right_point) {
                Ok((daily, near_usd)) => {
                    guard = json!({ "fees_day_usd": (daily * 1e4).round() / 1e4, "gas_usd": (REBALANCE_GAS_NEAR * near_usd * 1e4).round() / 1e4 });
                    if !rebalance_pays(daily, near_usd) {
                        return Ok(json!({ "u": user, "p": pool.current_point, "a": "noop", "why": "a day of fees would not pay for the move", "g": guard }));
                    }
                }
                Err(e) => guard = json!({ "unavailable": e.chars().take(60).collect::<String>() }),
            }
        }
        let summary = format!("{action:?}");
        let Some((method, args)) = call_for(&action, user) else {
            return Ok(json!({ "u": user, "p": pool.current_point, "a": summary }));
        };
        let tx = send(me, pk, GUARDIAN, method, args)?;
        Ok(json!({ "u": user, "p": pool.current_point, "a": summary, "tx": tx, "g": guard }))
    }

    /// GeckoTerminal JSON (network allowlist: api.geckoterminal.com).
    fn gecko(path: &str) -> Result<Value, String> {
        let resp = wasi_http_client::Client::new()
            .get(&format!("https://api.geckoterminal.com/api/v2/{path}"))
            .header("accept", "application/json")
            .send()
            .map_err(|e| format!("gecko: {e}"))?;
        serde_json::from_slice(&resp.body().map_err(|e| format!("gecko body: {e}"))?).map_err(|e| e.to_string())
    }

    /// A day of fees the re-entered range would earn at the pool's 24h volume, and NEAR's USD price.
    fn day_of_fees(pool: &PoolView, old: &PositionRef, left: i32, right: i32) -> Result<(f64, f64), String> {
        let liq = view(DCL, "get_liquidity", json!({ "lpt_id": old.lpt_id }))?;
        let old_l: f64 = liq["amount"].as_str().and_then(|s| s.parse().ok()).ok_or("no position liquidity")?;
        let new_l = reentry_liquidity(old, old_l, pool.current_point, left, right);
        let pool_l: f64 = pool.liquidity.parse().unwrap_or(0.0);
        let id = pool.pool_id.replace('|', ":");
        let g = gecko(&format!("networks/near/pools/refv2-{id}"))?;
        let volume: f64 = g["data"]["attributes"]["volume_usd"]["h24"].as_str().and_then(|s| s.parse().ok()).ok_or("no 24h volume")?;
        let p = gecko("simple/networks/near/token_price/wrap.near")?;
        let near_usd: f64 = p["data"]["attributes"]["token_prices"]["wrap.near"].as_str().and_then(|s| s.parse().ok()).ok_or("no NEAR price")?;
        Ok((daily_fee_usd(volume, pool.fee, new_l, pool_l), near_usd))
    }
}

#[cfg(target_arch = "wasm32")]
use run_impl::run;
