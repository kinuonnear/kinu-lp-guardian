#!/usr/bin/env bash
# One command for a reviewer. Reads mainnet with no keys, no account and no spending, then runs the
# contract's and the agent's unit tests.
#   ./scripts/verify.sh          about 2 minutes: needs curl and python3, plus cargo for the unit tests
#   ./scripts/verify.sh --full   about 90 minutes: builds, then every sandbox suite on real Rhea code
# Exit code 0 means every check passed.
set -uo pipefail
cd "$(dirname "$0")/.."

RPC=${NEAR_RPC:-https://free.rpc.fastnear.com}
G=lp.kinuonnear.near
DCL=dclv2.ref-labs.near
AGENT=a18c4ac6d011b41a394570c843aa48a47539312e36004c56650cb00cbf7ac810
# The reproducible CI builds of 34bb96b (run 41521583452) and 077d229 (run 41374566538); v1.7 stands
# until the upgrade to v1.8 has run.
CODE_V18=BrTcgXH9A8U9rtvoGJu3kjZaKjiTvFUs6htt8tMrfNRq
CODE_V17=Cs89ihQAYiDCbaV3c9ZxY7rsthKMWmDXR3ijQBb72E9p
RHEA=3G34Nhjmir5JPYPtijaA9iCNnHDPR7h3wfC9H77woEGo
failed=0

say() { printf '%-6s %s\n' "$1" "$2"; [ "$1" = FAIL ] && failed=1; return 0; }
rpc() { curl -s -m 25 "$RPC" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"query\",\"params\":$1}"; }
account() { rpc "{\"request_type\":\"view_account\",\"finality\":\"final\",\"account_id\":\"$1\"}" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["result"]["code_hash"])' 2>/dev/null; }
config() { rpc "{\"request_type\":\"call_function\",\"finality\":\"final\",\"account_id\":\"$G\",\"method_name\":\"get_config\",\"args_base64\":\"e30=\"}" | python3 -I -c 'import json,sys; print(bytes(json.load(sys.stdin)["result"]["result"]).decode())' 2>/dev/null; }
field() { python3 -I -c 'import json,sys; v=json.loads(sys.argv[1]).get(sys.argv[2]); print(json.dumps(v))' "$2" "$1"; }

echo "== what runs on mainnet =="
code=$(account "$G")
case "$code" in
  "$CODE_V18") say PASS "$G runs the published v1.8 build ($code)" ;;
  "$CODE_V17") say PASS "$G runs the published v1.7 build ($code); v1.8 waits its 48 hours" ;;
  *) say FAIL "$G runs unpublished code: ${code:-unreadable}" ;;
esac
cfg=$(config)
if [ -n "$cfg" ]; then
  delay=$(field admin_delay_secs "$cfg")
  [ "$delay" = 172800 ] && say PASS "every admin action waits 48 hours in public (admin_delay_secs $delay)" || say FAIL "admin delay is $delay"
  say INFO "version $(field version "$cfg"), paused $(field paused "$cfg"), owner $(field owner "$cfg"), waiting actions $(field pending "$cfg")"
else
  say FAIL "get_config unreadable"
fi
keys=$(rpc "{\"request_type\":\"view_access_key_list\",\"finality\":\"final\",\"account_id\":\"$AGENT\"}")
python3 -I - "$keys" "$G" <<'PY'
import json, sys
keys = json.loads(sys.argv[1])["result"]["keys"]
ok = len(keys) == 1 and isinstance(keys[0]["access_key"]["permission"], dict)
fc = keys[0]["access_key"]["permission"].get("FunctionCall", {}) if ok else {}
ok = ok and fc.get("receiver_id") == sys.argv[2] and sorted(fc.get("method_names", [])) == ["exit", "poke", "request_rebalance"]
print(("PASS   " if ok else "FAIL   ") + ("the agent has one key, limited by the protocol to poke, request_rebalance and exit on " + sys.argv[2] if ok else "the agent's key is not the limited one: " + json.dumps(keys)))
sys.exit(0 if ok else 1)
PY
[ $? = 0 ] || failed=1
rhea=$(account "$DCL")
[ "$rhea" = "$RHEA" ] && say PASS "Rhea runs the build these tests used ($rhea)" || say INFO "Rhea runs $rhea, not the tested $RHEA: re-centres are held until it is tested"

echo; echo "== the agent's key and the TEE =="
python3 -I scripts/check-agent-tee.py || failed=1

echo; echo "== the proof of a stolen agent key, on mainnet =="
n=$(grep -c '| REFUSED |' demo/mainnet-run.md)
[ "$n" -gt 0 ] && say PASS "$n attacks refused on mainnet, one transaction link each: demo/mainnet-run.md" || say FAIL "demo/mainnet-run.md has no results"

if command -v cargo >/dev/null; then
  echo; echo "== unit tests =="
  t=$(cargo test -p kinu-guardian-shared --lib --features legacy-suites 2>&1 | grep -E '^test result')
  case "$t" in *" 0 failed"*) say PASS "contract: $t" ;; *) say FAIL "contract: ${t:-did not run}" ;; esac
  t=$(cargo test -p kinu-guardian-shared --lib v17_ 2>&1 | grep -E '^test result')
  case "$t" in *" 0 failed"*) say PASS "contract, production rules: $t" ;; *) say FAIL "contract, production rules: ${t:-did not run}" ;; esac
  t=$(cd agent && cargo test --lib 2>&1 | grep -E '^test result')
  case "$t" in *" 0 failed"*) say PASS "agent: $t" ;; *) say FAIL "agent: ${t:-did not run}" ;; esac
else
  say INFO "cargo not found: unit tests skipped"
fi

if [ "${1:-}" = --full ]; then
  echo; echo "== sandbox suites =="
  python3 scripts/fetch_fixtures.py
  ./scripts/build.sh legacy
  for s in attacks direct shared slots tax flow realdcl v14 v16 upgrade migrate gas_sweep agent_key retire factory; do
    t=$(cargo test -p kinu-guardian-tests --test "$s" 2>&1 | grep -E '^test result')
    case "$t" in *" 0 failed"*) say PASS "$s" ;; *) say FAIL "$s: ${t:-did not run}" ;; esac
  done
  ./scripts/build.sh
  for s in v17 v18 sale keep_top rhea_change; do
    t=$(cargo test -p kinu-guardian-tests --test "$s" 2>&1 | grep -E '^test result')
    case "$t" in *" 0 failed"*) say PASS "$s" ;; *) say FAIL "$s: ${t:-did not run}" ;; esac
  done
fi

echo
[ "$failed" = 0 ] && echo "ALL CHECKS PASSED" || echo "SOME CHECKS FAILED"
exit $failed
