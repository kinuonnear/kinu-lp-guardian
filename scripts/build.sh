#!/usr/bin/env bash
# Builds every contract to target/near/<name>/<name>.wasm (used by the sandbox tests).
set -euo pipefail
cd "$(dirname "$0")/.."
# `./scripts/build.sh legacy` builds the guardian for the suites written before v1.7: no deposit gap,
# every token listed, so each keeps testing its own subject. Never deployed.
features=""
[ "${1:-}" = legacy ] && features="legacy-suites"
for c in vault factory mockdcl testtoken probe cleanup; do
  cargo near build non-reproducible-wasm --no-abi --manifest-path contracts/$c/Cargo.toml
done
cargo near build non-reproducible-wasm --no-abi ${features:+--features $features} --manifest-path contracts/guardian/Cargo.toml
# The sandbox timelock test also needs the guardian with a 10-minute admin delay (never deployed).
cargo near build non-reproducible-wasm --no-abi --features short-delay${features:+,$features} --manifest-path contracts/guardian/Cargo.toml --out-dir target/near/kinu_guardian_shared_short
