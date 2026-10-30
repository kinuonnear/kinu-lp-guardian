#!/usr/bin/env bash
# Before Guardian accepts a new Rhea DCL build: fetch the code dclv2.ref-labs.near runs now, print
# its hash (compare with GUARDIAN.rheaCodeHash and DCL_CODE_HASH in KINU), and run every sandbox suite
# against it. Only when all pass do the pinned hashes move to the new build.
#   ./scripts/check-new-dcl.sh [out.wasm]
set -euo pipefail
cd "$(dirname "$0")/.."
out=${1:-/tmp/dcl-live.wasm}
curl -sf https://rpc.mainnet.near.org -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"view_code","finality":"final","account_id":"dclv2.ref-labs.near"}}' \
  | python3 -I -c 'import sys, json, base64; r = json.load(sys.stdin)["result"]; open(sys.argv[1], "wb").write(base64.b64decode(r["code_base64"])); print("dclv2.ref-labs.near code_hash", r["hash"])' "$out"
./scripts/build.sh
cd tests
failed=0
for t in realdcl shared attacks direct tax gas_sweep migrate v14 agent_key; do
  echo "=== $t"
  res=$(DCL_WASM="$out" cargo test --test "$t" 2>&1 | grep -aE "test result|panicked" || true)
  echo "$res"
  echo "$res" | grep -q "test result: ok" || failed=1
done
exit $failed
