//! OutLayer entry point. Input (stdin JSON, untrusted): {"cmd":"pubkey"} or {"cmd":"watch","walls":[..]}.
//! Output (stdout, at most 900 bytes): what it flagged and did per wall.
//!
//! Its NEAR account is the implicit account of the TEE key `sentinel`; the module never sees the key.

#[cfg(target_arch = "wasm32")]
mod host {
    pub mod rpc_host {
        wit_bindgen::generate!({ world: "rpc-test", path: "../agent/wit-rpc" });
    }
    pub mod keys_host {
        wit_bindgen::generate!({ world: "signing-keys-host", path: "../agent/wit-keys" });
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
    let out = run_impl::run(&input).unwrap_or_else(|e| serde_json::json!({ "ok": false, "error": e }));
    let mut s = out.to_string();
    s.truncate(900);
    print!("{s}");
    let _ = std::io::stdout().flush();
}

#[cfg(target_arch = "wasm32")]
mod run_impl {
    use super::host::{keys, rpc};
    use kinu_guardian_agent::{implicit_account, near_public_key, FunctionCall, NearAction, PublicKey, Signature, SignedTransaction, Transaction};
    use kinu_sentinel::{plan, Move, Request, WallConfig};
    use serde_json::{json, Value};

    /// The wall code this build watches: the global contract hash every wall is deployed from.
    const WALL_CODE: &str = "6A2G79nESwLhy1Vnn2xe3nfCRd4pDWKydgjiX4vowvC";
    const MAX_WALLS: usize = 5;
    const T: u64 = 1_000_000_000_000;

    fn rpc_result(raw: &str) -> Result<Value, String> {
        let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if let Some(e) = v.get("error") {
            return Err(e.to_string().chars().take(200).collect());
        }
        Ok(v.get("result").cloned().unwrap_or(v))
    }

    fn view(wall: &str, method: &str) -> Result<Value, String> {
        let (res, err) = rpc::view(wall, method, "{}", "final");
        if !err.is_empty() {
            return Err(format!("{wall}.{method}: {err}"));
        }
        serde_json::from_str(&res).map_err(|e| format!("{wall}.{method}: {e}"))
    }

    fn now_secs() -> Result<u64, String> {
        let (res, err) = rpc::block("final");
        if !err.is_empty() {
            return Err(format!("block: {err}"));
        }
        let b = rpc_result(&res)?;
        Ok(b["header"]["timestamp"].as_u64().ok_or("no block time")? / 1_000_000_000)
    }

    fn send(me: &str, pk: &[u8; 32], wall: &str, method: &str, args: String, tgas: u64) -> Result<String, String> {
        let (ak, err) = rpc::view_access_key(me, &near_public_key(pk), "final");
        if !err.is_empty() {
            return Err(format!("access key: {err} (fund the sentinel account {me})"));
        }
        let ak = rpc_result(&ak)?;
        let nonce = ak["nonce"].as_u64().ok_or("no nonce")? + 1;
        let block_hash: [u8; 32] = bs58::decode(ak["block_hash"].as_str().ok_or("no block hash")?)
            .into_vec()
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|_| "bad block hash")?;
        let call = NearAction::FunctionCall(FunctionCall { method_name: method.to_string(), args: args.into_bytes(), gas: tgas * T, deposit: 0 });
        let tx = Transaction { signer_id: me.to_string(), public_key: PublicKey::Ed25519(*pk), nonce, receiver_id: wall.to_string(), block_hash, actions: vec![call] };
        let sig = keys::sign("sentinel", None, &tx.hash()).map_err(|e| format!("sign: {e}"))?;
        let signature: [u8; 64] = sig.try_into().map_err(|_| "signature is not 64 bytes")?;
        let signed = SignedTransaction { transaction: tx, signature: Signature::Ed25519(signature) };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(borsh::to_vec(&signed).map_err(|e| e.to_string())?);
        let (res, err) = rpc::send_tx(&b64, "FINAL");
        if !err.is_empty() {
            return Err(format!("send_tx: {err}"));
        }
        let v = rpc_result(&res)?;
        let hash = v["transaction"]["hash"].as_str().unwrap_or("?").to_string();
        if v["status"].get("Failure").is_some() {
            return Err(format!("tx {hash} failed: {}", v["status"]["Failure"].to_string().chars().take(120).collect::<String>()));
        }
        Ok(hash)
    }

    fn watch(me: &str, pk: &[u8; 32], wall: &str) -> Result<Value, String> {
        let (acc, err) = rpc::view_account(wall, "final");
        if !err.is_empty() {
            return Err(format!("account: {err}"));
        }
        if rpc_result(&acc)?["global_contract_hash"].as_str() != Some(WALL_CODE) {
            return Err("not the published wall code".into());
        }
        let config: WallConfig = serde_json::from_value(view(wall, "get_config")?).map_err(|e| e.to_string())?;
        if config.sentinel.as_deref() != Some(me) {
            return Err("this wall names another sentinel".into());
        }
        let pending: Vec<Request> = serde_json::from_value(view(wall, "get_pending")?).map_err(|e| e.to_string())?;
        let p = plan(&config, &pending, now_secs()?);
        let flags: Vec<Value> = p.flags.iter().map(|f| json!({ "id": f.id, "why": f.why, "unlock": f.unlock_secs, "stop_at": f.stop_at })).collect();
        let (did, tx) = match p.act {
            Move::Nothing => ("nothing".to_string(), Value::Null),
            Move::Cancel { id, reason } => (format!("cancelled {id}"), json!(send(me, pk, wall, "cancel", json!({ "id": id, "reason": reason }).to_string(), 30)?)),
            Move::Freeze { reason } => (format!("froze: {reason}"), json!(send(me, pk, wall, "freeze", "{}".to_string(), 100)?)),
        };
        Ok(json!({ "wall": wall, "did": did, "tx": tx, "flags": flags }))
    }

    pub fn run(input: &str) -> Result<Value, String> {
        let req: Value = serde_json::from_str(input).unwrap_or(Value::Null);
        let key = keys::public_key("sentinel", None).map_err(|e| format!("public_key: {e}"))?;
        let pk: [u8; 32] = key.try_into().map_err(|_| "public key is not 32 bytes".to_string())?;
        let me = implicit_account(&pk);
        if req["cmd"] != "watch" {
            return Ok(json!({ "ok": true, "account": me, "public_key": near_public_key(&pk), "wall_code": WALL_CODE }));
        }
        let walls: Vec<&str> = req["walls"].as_array().map(|a| a.iter().filter_map(Value::as_str).take(MAX_WALLS).collect()).unwrap_or_default();
        let seen: Vec<Value> = walls.iter().map(|w| watch(&me, &pk, w).unwrap_or_else(|e| json!({ "wall": w, "error": e }))).collect();
        Ok(json!({ "ok": true, "walls": seen }))
    }
}
