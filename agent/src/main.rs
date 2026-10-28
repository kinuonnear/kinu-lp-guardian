//! OutLayer entry point. Input (stdin JSON, untrusted): {"cmd":"pubkey"} or {"cmd":"tick","vault":"<account>"}.
//! Output (stdout, at most 900 bytes): what the agent read, decided and sent.
//!
//! The agent's NEAR account is the implicit account of its TEE key `agent` (manifest: bind "wasm"):
//! the keystore derives it from this exact build and the caller, the module never sees it, and a new
//! build has a new key that the owner must name on the vault again.

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

    /// The published vault build this agent serves (cargo-near build of the vault in this repo).
    pub const VAULT_CODE_HASH: &str = "CGYt988XAD9ALcafEU2VPFLCMG6MxfwbN14siLkvB1Kj";
    pub const DCL: &str = "dclv2.ref-labs.near";
    const GAS: u64 = 300_000_000_000_000;

    fn view(contract: &str, method: &str, args: Value) -> Result<Value, String> {
        let (res, err) = rpc::view(contract, method, &args.to_string(), "final");
        if !err.is_empty() {
            return Err(format!("{contract}.{method}: {err}"));
        }
        serde_json::from_str(&res).map_err(|e| format!("{contract}.{method}: {e}"))
    }

    fn my_key() -> Result<[u8; 32], String> {
        let pk = keys::public_key("agent", None).map_err(|e| format!("public_key: {e}"))?;
        pk.try_into().map_err(|_| "public key is not 32 bytes".to_string())
    }

    /// Builds, signs (in the TEE) and sends one function call from the agent's implicit account.
    fn send(me: &str, pk: &[u8; 32], receiver: &str, method: &str, args: String) -> Result<String, String> {
        let (ak, err) = rpc::view_access_key(me, &near_public_key(pk), "final");
        if !err.is_empty() {
            return Err(format!("access key: {err} (fund the agent account {me})"));
        }
        let ak: Value = serde_json::from_str(&ak).map_err(|e| e.to_string())?;
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
            actions: vec![NearAction::FunctionCall(FunctionCall { method_name: method.to_string(), args: args.into_bytes(), gas: GAS, deposit: 0 })],
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
        let v: Value = serde_json::from_str(&res).unwrap_or(Value::Null);
        let failed = v["status"].get("Failure").is_some();
        let hash = v["transaction"]["hash"].as_str().unwrap_or("?").to_string();
        if failed {
            return Err(format!("tx {hash} failed: {}", v["status"]["Failure"].to_string().chars().take(160).collect::<String>()));
        }
        Ok(hash)
    }

    /// A live 1Click quote for a Bunker exit, fetched from inside the TEE (network allowlist:
    /// 1click.chaindefuser.com). The vault checks 1Click's signature itself; this only finds one.
    fn bunker_quote(vault: &str, token: &str, amount: u128, recipient: &str) -> Result<(String, String), String> {
        let body = json!({
            "dry": false, "swapType": "EXACT_INPUT", "slippageTolerance": 100,
            "originAsset": format!("nep141:{token}"), "depositType": "ORIGIN_CHAIN",
            "destinationAsset": "nep141:zec.omft.near", "amount": amount.to_string(),
            "refundTo": vault, "refundType": "ORIGIN_CHAIN", "recipient": recipient,
            "recipientType": "DESTINATION_CHAIN", "deadline": deadline_in_30_min()?,
        });
        let resp = wasi_http_client::Client::new()
            .post("https://1click.chaindefuser.com/v0/quote")
            .header("content-type", "application/json")
            .body(body.to_string().as_bytes())
            .send()
            .map_err(|e| format!("1click: {e}"))?;
        let j: Value = serde_json::from_slice(&resp.body().map_err(|e| format!("1click body: {e}"))?).map_err(|e| e.to_string())?;
        let signed = signed_message(&j).ok_or("1click: unexpected quote")?;
        let signature = j["signature"].as_str().ok_or("1click: no signature")?.to_string();
        Ok((signed, signature))
    }

    fn deadline_in_30_min() -> Result<String, String> {
        let (b, err) = rpc::block("final");
        if !err.is_empty() {
            return Err(err);
        }
        let b: Value = serde_json::from_str(&b).map_err(|e| e.to_string())?;
        let ns = b["header"]["timestamp_nanosec"].as_str().and_then(|s| s.parse::<u64>().ok()).ok_or("no block time")?;
        Ok(iso(ns / 1_000_000_000 + 1800))
    }

    fn iso(secs: u64) -> String {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", rem / 3600, rem % 3600 / 60, rem % 60)
    }

    /// The exact string 1Click signed: the SDK's field lists, absent fields left out, keys sorted
    /// explicitly (independent of serde_json's map order), compact JSON like JSON.stringify.
    fn signed_message(j: &Value) -> Option<String> {
        let r = j.get("quoteRequest")?;
        let q = j.get("quote")?;
        let mut m: Vec<(&str, &Value)> = Vec::new();
        for k in ["dry", "swapType", "slippageTolerance", "originAsset", "depositType", "destinationAsset", "amount", "refundTo", "refundType", "recipient", "recipientType", "deadline"] {
            if let Some(v) = r.get(k) {
                m.push((k, v));
            }
        }
        for k in ["quoteWaitingTimeMs", "referral", "virtualChainRecipient", "virtualChainRefundRecipient", "customRecipientMsg"] {
            if let Some(v) = r.get(k).filter(|v| truthy(v)) {
                m.push((k, v));
            }
        }
        for k in ["amountIn", "amountInFormatted", "amountInUsd", "minAmountIn", "amountOut", "amountOutFormatted", "amountOutUsd", "minAmountOut"] {
            if let Some(v) = q.get(k) {
                m.push((k, v));
            }
        }
        // The quote's deadline replaces the request's, as in the SDK's object spread.
        for k in ["depositAddress", "depositMemo", "deadline", "timeWhenInactive", "timeEstimate", "refundFee", "withdrawFee"] {
            // A live quote's own deadline (even when absent) replaces the request's.
            m.retain(|(name, _)| *name != k);
            if let Some(v) = q.get(k).filter(|v| truthy(v)) {
                m.push((k, v));
            }
        }
        let ts = j.get("timestamp")?;
        m.push(("timestamp", ts));
        m.sort_by(|a, b| a.0.cmp(b.0));
        let body: Vec<String> = m.iter().map(|(k, v)| format!("{}:{}", Value::from(*k), v)).collect();
        Some(format!("{{{}}}", body.join(",")))
    }

    fn truthy(v: &Value) -> bool {
        !(v.is_null() || v == &json!(false) || v == &json!(0) || v == &json!(""))
    }

    pub fn run(input: &str) -> Result<Value, String> {
        let req: Value = serde_json::from_str(input).unwrap_or(Value::Null);
        let pk = my_key()?;
        let me = implicit_account(&pk);
        if req["cmd"] == "pubkey" {
            return Ok(json!({ "ok": true, "agent_account": me, "public_key": near_public_key(&pk) }));
        }
        let vault = req["vault"].as_str().ok_or("input needs {\"cmd\":\"tick\",\"vault\":\"...\"}")?;
        // Only the published vault build: an attacker's contract named in the input gets nothing.
        let (acct, err) = rpc::view_account(vault, "final");
        if !err.is_empty() {
            return Err(format!("vault account: {err}"));
        }
        let acct: Value = serde_json::from_str(&acct).map_err(|e| e.to_string())?;
        if acct["code_hash"] != VAULT_CODE_HASH {
            return Ok(json!({ "ok": true, "agent": me, "action": "noop", "why": "not the published vault build", "code_hash": acct["code_hash"] }));
        }
        let v: VaultView = serde_json::from_value(view(vault, "get_vault", json!({}))?).map_err(|e| e.to_string())?;
        let pool_id = v.position.as_ref().map(|p| p.pool_id.clone()).or_else(|| v.policy.allowed_pools.first().cloned()).ok_or("no pool")?;
        let pool: PoolView = serde_json::from_value(view(DCL, "get_pool", json!({ "pool_id": pool_id }))?).map_err(|e| e.to_string())?;
        let mut wallet = Vec::new();
        for t in [&pool.token_x, &pool.token_y] {
            let b = view(t, "ft_balance_of", json!({ "account_id": vault }))?;
            wallet.push((t.clone(), b.as_str().and_then(|s| s.parse().ok()).unwrap_or(0u128)));
        }
        let assets = view(DCL, "list_user_assets", json!({ "account_id": vault }))?;
        let funded = assets.as_object().is_some_and(|m| m.values().any(|a| a.as_str().is_some_and(|s| s != "0")));
        let action = decide(&World { me: &me, vault: &v, pool: &pool, wallet: &wallet, dcl_funded: funded });
        let quote = match &action {
            Action::BunkerExit { token, amount } => Some(bunker_quote(vault, token, *amount, v.exit_zec.as_deref().unwrap_or_default())?),
            _ => None,
        };
        let summary = format!("{action:?}");
        let Some((method, args)) = call_for(&action, quote.as_ref().map(|(s, g)| (s.as_str(), g.as_str()))) else {
            return Ok(json!({ "ok": true, "agent": me, "point": pool.current_point, "action": summary }));
        };
        let tx = send(&me, &pk, vault, method, args)?;
        Ok(json!({ "ok": true, "agent": me, "point": pool.current_point, "action": summary, "tx": tx }))
    }
}

#[cfg(target_arch = "wasm32")]
use run_impl::run;
