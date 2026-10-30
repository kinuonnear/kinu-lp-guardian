#!/usr/bin/env bash
# Compromised-agent demo on mainnet. Two throwaway accounts, no real user involved:
# "victim" joins lp.kinuonnear.near and names "attacker" as its agent, so attacker holds a valid
# agent key, as a thief would. Every attempt below must fail and leave the victim's record unchanged.
#   ./compromised-agent.sh setup | attack | cleanup
set -u
G=lp.kinuonnear.near; DCL=dclv2.ref-labs.near; WRAP=wrap.near
POOL="17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1|wrap.near|100"
OWNER=kinuonnear.near
K=${DEMO_KEYS:-$HOME/kinu-keys/demo}
VF=$(ls "$K"/victim/*.json | head -1); V=$(basename "$VF" .json)
AF=$(ls "$K"/attacker/*.json | head -1); A=$(basename "$AF" .json)
OUT=${DEMO_LOG:-$(dirname "$0")/mainnet-run.md}

raw() { # file account contract method args deposit
  near contract call-function as-transaction "$3" "$4" json-args "$5" prepaid-gas '300.0 Tgas' attached-deposit "$6" \
    sign-as "$2" network-config mainnet sign-with-access-key-file "$1" send </dev/null 2>&1
}
tx() { grep -oE 'Transaction ID: [1-9A-HJ-NP-Za-km-z]{43,44}' <<<"$1" | head -1 | awk '{print $3}'; }
view() { near contract call-function as-read-only "$1" "$2" json-args "$3" network-config mainnet now </dev/null 2>/dev/null | sed -n '/^[\[{"0-9n]/,$p'; }
account() { view "$G" get_account "{\"user\":\"$V\"}"; }
point() { view "$DCL" get_pool "{\"pool_id\":\"$POOL\"}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["current_point"])'; }
step() { # label file account contract method args deposit
  r=$(raw "$2" "$3" "$4" "$5" "$6" "$7"); h=$(tx "$r"); ok=FAILED; grep -q 'succeeded' <<<"$r" && ok=ok
  printf '%-34s %-7s %s\n' "$1" "$ok" "$h"; [ "$ok" = ok ] || { echo "$r" | tail -12; exit 1; }
}
try() { # label contract method args expected text
  r=$(raw "$AF" "$A" "$2" "$3" "$4" '0 NEAR'); h=$(tx "$r")
  if grep -qi "$5" <<<"$r"; then s=REFUSED; else s="CHECK"; fi
  printf '| %s | %s | `%s` | [tx](https://nearblocks.io/txns/%s) |\n' "$1" "$s" "$5" "$h" | tee -a "$OUT"
  [ "$s" = REFUSED ] || echo "$r" | tail -14
}

case "${1:-}" in
setup)
  if [ "${2:-}" != resume ]; then
    near transaction construct-transaction "$V" "$A" add-action transfer '0.35 NEAR' skip network-config mainnet sign-with-access-key-file "$VF" send </dev/null 2>&1 | tail -2
    step 'register on wrap.near' "$VF" "$V" "$WRAP" storage_deposit '{}' '0.00125 NEAR'
  fi
  step 'wrap 0.1 NEAR' "$VF" "$V" "$WRAP" near_deposit '{}' '0.1 NEAR'
  P="{\"policy\":{\"allowed_pools\":[\"$POOL\"],\"min_width\":1400,\"max_width\":5600,\"max_distance\":3000,\"max_actions\":12,\"window_secs\":86400,\"max_slippage_bps\":100,\"exit_drop_points\":0,\"exit_on_rise\":false,\"rebalance_buffer_points\":3000},\"agent\":\"$A\"}"
  step 'join, agent = attacker' "$VF" "$V" "$G" join "$P" '0.01 NEAR'
  step 'deposit 0.1 wNEAR' "$VF" "$V" "$WRAP" ft_transfer_call "{\"receiver_id\":\"$G\",\"amount\":\"100000000000000000000000\",\"msg\":\"\"}" '1 yoctoNEAR'
  c=$(point); r=$((c-40)); l=$((r-1800)); echo "point $c, range $l..$r"
  step 'open a range' "$VF" "$V" "$G" open "{\"left_point\":$l,\"right_point\":$r}" '0 NEAR'
  account
  ;;
attack)
  before=$(account); c=$(point)
  lpt=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["position"]["lpt_id"])' "$before")
  { echo "# Compromised agent, mainnet run $(date -u +%F\ %H:%M) UTC"; echo; echo "Contract $G v1.5.0. Agent key held by the attacker: \`$A\`. Victim: \`$V\`, lpt \`$lpt\`, pool point $c."; echo; echo '| Attempt | Result | Contract answer | Proof |'; echo '| --- | --- | --- | --- |'; } | tee "$OUT"
  try 'Move the funds to another pool' "$G" request_rebalance "{\"user\":\"$V\",\"request\":{\"pool_id\":\"$WRAP|evil.near|100\",\"left_point\":$((c-700)),\"right_point\":$((c+700))}}" "not in the owner's allowed list"
  try 'Range wider than the owner allows' "$G" request_rebalance "{\"user\":\"$V\",\"request\":{\"pool_id\":\"$POOL\",\"left_point\":$((c-4000)),\"right_point\":$((c+4000))}}" "wider than the owner"
  try 'Range narrower than the minimum' "$G" request_rebalance "{\"user\":\"$V\",\"request\":{\"pool_id\":\"$POOL\",\"left_point\":$((c-50)),\"right_point\":$((c+50))}}" "narrower than the owner"
  try 'Range far from the price' "$G" request_rebalance "{\"user\":\"$V\",\"request\":{\"pool_id\":\"$POOL\",\"left_point\":$((c+10000)),\"right_point\":$((c+11400))}}" "too far from the pool"
  try 'Churn a position for fees and gas' "$G" request_rebalance "{\"user\":\"$V\",\"request\":{\"pool_id\":\"$POOL\",\"left_point\":$((c-700)),\"right_point\":$((c+700))}}" "not far enough outside"
  try 'Close the position (no exit allowed)' "$G" exit "{\"user\":\"$V\"}" "has not allowed the agent to exit"
  try 'Withdraw the tokens' "$G" withdraw "{\"token\":\"$WRAP\"}" "has not joined"
  try 'Cash out' "$G" cash_out '{"leave":false}' "has not joined"
  try 'Change the rules' "$G" set_policy '{"policy":{"allowed_pools":["x|y|100"],"min_width":1,"max_width":1,"max_distance":1,"max_actions":1,"window_secs":1,"max_slippage_bps":1,"exit_drop_points":1,"exit_on_rise":false,"rebalance_buffer_points":0}}' "has not joined"
  try 'Name another agent' "$G" set_agent "{\"agent\":\"$A\"}" "has not joined"
  try 'Take over as owner' "$G" propose '{"action":{"SetOwner":{"owner":"'"$A"'"}}}' "Only the owner"
  try 'Pause the contract' "$G" set_paused '{"paused":true}' "Only the owner"
  try 'Pull the position from Rhea directly' "$DCL" remove_liquidity "{\"lpt_id\":\"$lpt\",\"amount\":\"1\",\"min_amount_x\":\"0\",\"min_amount_y\":\"0\"}" "panicked"
  after=$(account)
  if [ "$before" = "$after" ]; then s='The victim record is byte for byte the same before and after.'; else s='VICTIM RECORD CHANGED'; fi
  { echo; echo "$s"; } | tee -a "$OUT"
  ;;
cleanup)
  step 'victim takes everything out' "$VF" "$V" "$G" cash_out '{"leave":true}' '0 NEAR'
  account
  w=$(view "$WRAP" ft_balance_of "{\"account_id\":\"$V\"}" | tr -d '"')
  echo "wNEAR left: $w"
  [ "$w" = 0 ] || step 'unwrap' "$VF" "$V" "$WRAP" near_withdraw "{\"amount\":\"$w\"}" '1 yoctoNEAR'
  near account delete-account "$A" beneficiary "$OWNER" network-config mainnet sign-with-access-key-file "$AF" send </dev/null 2>&1 | tail -2
  near account delete-account "$V" beneficiary "$OWNER" network-config mainnet sign-with-access-key-file "$VF" send </dev/null 2>&1 | tail -2
  ;;
*) echo "usage: $0 setup|attack|cleanup"; echo "victim $V"; echo "attacker $A" ;;
esac
