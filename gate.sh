#!/usr/bin/env bash
# The one gate margin's CI and `testenv --suites gate` both run. It asserts nothing itself:
# every assertion lives in a cargo test.
set -euo pipefail

CHROMIUM_VERSION="156.0.8078.79-1~deb12u1"

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test -p margin --features integration --test http

apt-get update -o Acquire::Retries=3
if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "chromium=${CHROMIUM_VERSION}" >/dev/null; then
  echo "gate.sh: chromium ${CHROMIUM_VERSION} did not install; the archive offers:" >&2
  apt-cache policy chromium >&2
  cat /etc/apt/sources.list /etc/apt/sources.list.d/* >&2 2>/dev/null || true
  exit 1
fi

MARGIN_CHROMIUM="$(command -v chromium)" cargo test -p margin-ui -- --ignored
