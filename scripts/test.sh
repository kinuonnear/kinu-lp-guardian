#!/usr/bin/env bash
# Unit tests + the end-to-end sandbox test (owner funds, compromised agent refused, rebalance, rule-based exit).
# If near-sandbox can not download its binary, point NEAR_SANDBOX_BIN_PATH at an existing one.
set -euo pipefail
cd "$(dirname "$0")/.."
./scripts/build.sh
cargo test
