#!/usr/bin/env python3
"""Is the agent's key the one an attested OutLayer run returned? No keys, no account, standard library only.

The run: {"cmd":"pubkey"} sent on chain through the agent's project. Its attestation names the wasm that ran
and the hashes of its input and output. This script reads the record, then checks each field against the chain.
Intel's signature over the quote needs Intel's collateral, so it is left to outlayer-verify or the report page.
    ./scripts/check-agent-tee.py [tx]
"""
import base64, hashlib, json, os, shutil, subprocess, sys, urllib.request

AGENT = "a18c4ac6d011b41a394570c843aa48a47539312e36004c56650cb00cbf7ac810"
PROJECT = "agent.kinuonnear.near/kinu-guardian"
INPUT = '{"cmd":"pubkey"}'
RPC = os.environ.get("NEAR_RPC", "https://free.rpc.fastnear.com")
ARCHIVE = os.environ.get("NEAR_ARCHIVE_RPC", "https://archival-rpc.mainnet.fastnear.com")
# Agent wasm sha256 -> the transaction of its attested pubkey run.
PROOFS = {
    "f7da11faa80b342b7e243a4a529766efe3e30a99673acbd2a260a8c92767c169": "2GiU6VyTeCtD5WQueuEkZqfoHsodQzBDXqGVJFLTix5J",
    "5aaad2670b5b364cdd629bb642ddfa39edb87b039c046164929faa10258165dc": "5DwL6X2Ss1jPajPgPrGEDMErVfLgAqGj2n9M92QBKVtV",
}
failed = False


def say(ok, text):
    global failed
    failed = failed or ok is False
    print(f"{'INFO' if ok is None else 'PASS' if ok else 'FAIL':<6} {text}")


def post(url, body):
    req = urllib.request.Request(url, json.dumps(body).encode(), {"content-type": "application/json", "user-agent": "kinu-verify"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def rpc(url, method, params):
    return post(url, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).get("result")


def view(account, method, args):
    r = rpc(RPC, "query", {"request_type": "call_function", "finality": "final", "account_id": account, "method_name": method,
                           "args_base64": base64.b64encode(json.dumps(args).encode()).decode()})
    return json.loads(bytes(r["result"]))


def sha(data):
    return hashlib.sha256(data).hexdigest()


def b58(text):
    n = 0
    for c in text:
        n = n * 58 + "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".index(c)
    return n.to_bytes(32, "big")


def main():
    project = view("outlayer.near", "get_project", {"project_id": PROJECT})
    active = project["active_version"]
    count = view("outlayer.near", "get_version_count", {"project_id": PROJECT})
    say(None, f"OutLayer project {PROJECT}: active wasm {active[:12]}..., {count} versions published")
    tx_hash = sys.argv[1] if len(sys.argv) > 1 else PROOFS.get(active)
    if not tx_hash:
        return say(False, f"no attested pubkey run recorded for the active wasm {active[:12]}...")

    req = urllib.request.Request(f"https://api.outlayer.ai/attestations/by-tx/{tx_hash}", headers={"user-agent": "kinu-verify"})
    with urllib.request.urlopen(req, timeout=30) as r:
        rec = json.load(r)
    say(rec.get("wasm_hash") == active and rec.get("executed_wasm_sha256") == active, f"the attested run executed the active agent wasm ({str(rec.get('wasm_hash'))[:12]}...)")
    say(rec.get("project_id") == PROJECT, f"it ran through the agent's project ({rec.get('project_id')})")

    tx = rpc(ARCHIVE, "EXPERIMENTAL_tx_status", {"tx_hash": tx_hash, "sender_account_id": rec["caller_account_id"], "wait_until": "EXECUTED_OPTIMISTIC"})
    call = next(a["FunctionCall"] for a in tx["transaction"]["actions"] if "FunctionCall" in a)
    sent = json.loads(base64.b64decode(call["args"])).get("input_data", "")
    say(sent == INPUT and sha(sent.encode()) == rec["input_hash"], f"the chain's input {sent} hashes to the attested input_hash")
    out = base64.b64decode(tx["status"]["SuccessValue"])
    say(sha(out) == rec["output_hash"], "the chain's output hashes to the attested output_hash")
    answer = json.loads(out)
    key = answer.get("public_key", "")
    say(answer.get("agent_account") == AGENT and key.startswith("ed25519:") and b58(key[8:]).hex() == AGENT, f"the run returned the agent's account, the hex of its key {key[:18]}...")
    keys = rpc(RPC, "query", {"request_type": "view_access_key_list", "finality": "final", "account_id": AGENT})["keys"]
    say([k["public_key"] for k in keys] == [key], "that key is the only key on the agent's account")

    quote = base64.b64decode(rec["tdx_quote"])
    part = lambda at: quote[at:at + 48].hex()
    measurements = {"mrtd": part(184), "rtmr0": part(376), "rtmr1": part(424), "rtmr2": part(472), "rtmr3": part(520)}
    say(view("worker.outlayer.near", "is_measurements_approved", {"measurements": measurements}) is True, "the five TDX measurements in its quote are approved on worker.outlayer.near")

    job = rec["task_id"]
    if shutil.which("outlayer-verify"):
        done = subprocess.run(["outlayer-verify", "job", str(job), "--network", "mainnet", "--api", "https://api.outlayer.ai"], capture_output=True, text=True)
        say(done.returncode == 0, f"outlayer-verify: Intel's signature, approved build and binding, job {job}")
    else:
        say(None, f"Intel's signature: open https://app.outlayer.ai/attestation/{job}?network=mainnet and press Verify, or run: outlayer-verify job {job} --network mainnet --api https://api.outlayer.ai")


try:
    main()
except Exception as e:
    say(False, f"the TEE check could not finish: {e}")
sys.exit(1 if failed else 0)
