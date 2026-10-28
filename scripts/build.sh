#!/usr/bin/env bash
# Builds every contract to target/near/<name>/<name>.wasm (used by the sandbox tests).
set -euo pipefail
cd "$(dirname "$0")/.."
for c in vault factory mockdcl testtoken probe; do
  cargo near build non-reproducible-wasm --no-abi --manifest-path contracts/$c/Cargo.toml
done
