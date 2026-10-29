#!/usr/bin/env bash
# Builds every contract to target/near/<name>/<name>.wasm (used by the sandbox tests).
set -euo pipefail
cd "$(dirname "$0")/.."
for c in guardian vault factory mockdcl testtoken probe cleanup; do
  cargo near build non-reproducible-wasm --no-abi --manifest-path contracts/$c/Cargo.toml
done
# The sandbox timelock test also needs the guardian with a 10-minute admin delay (never deployed).
cargo near build non-reproducible-wasm --no-abi --features short-delay --manifest-path contracts/guardian/Cargo.toml --out-dir target/near/kinu_guardian_shared_short
