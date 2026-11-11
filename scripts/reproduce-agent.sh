#!/usr/bin/env bash
# Rebuild the agent's wasm in Docker and compare it with the hash OutLayer attests.
# The wasm holds source paths, so the build runs at the path GitHub Actions uses.
#   ./scripts/reproduce-agent.sh [commit] [expected sha256]
set -euo pipefail
cd "$(dirname "$0")/.."
COMMIT=${1:-4c20512}
WANT=${2:-24c822b5cae861d91b08d0d0b5bad60f41ee75e36b0eb276474284a35b3f0fe1}
IMAGE=rust:1.93.0@sha256:bbde3ca426faeebab3eb58b8005d3d6da70607817a14bb92a55c94611d4d905f
SRC=$(mktemp -d)
trap 'docker run --rm -v "$SRC":/s alpine:3 rm -rf /s/agent >/dev/null 2>&1 || true; rm -rf "$SRC"' EXIT
git archive "$COMMIT" | tar -x -C "$SRC"
BASE=/home/runner/work/kinu-lp-guardian/kinu-lp-guardian
GOT=$(docker run --rm -v "$SRC":$BASE -w $BASE/agent -e HOME=/home/runner -e CARGO_HOME=/home/runner/.cargo -e RUSTUP_HOME=/home/runner/.rustup "$IMAGE" bash -c '
  export PATH=/home/runner/.cargo/bin:$PATH
  cp -r /usr/local/rustup /home/runner/.rustup
  mkdir -p /home/runner/.cargo/bin && cp /usr/local/cargo/bin/* /home/runner/.cargo/bin/
  rustup target add wasm32-wasip2 >/dev/null 2>&1
  cargo build --release --locked --target wasm32-wasip2 >/dev/null 2>&1
  sha256sum target/wasm32-wasip2/release/kinu-guardian-agent.wasm' | cut -d' ' -f1)
echo "expected $WANT"
echo "built    $GOT"
[ "$GOT" = "$WANT" ] && echo "PASS   the agent wasm is reproduced from commit $COMMIT" || { echo "FAIL   the build differs"; exit 1; }
