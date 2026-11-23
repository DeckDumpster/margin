#!/usr/bin/env bash
# The one gate margin's CI and `testenv --suites gate` both run. It asserts nothing itself:
# every assertion lives in a cargo test.
set -euo pipefail

# `--build-only` compiles everything the gate tests and runs no test, so the harness can time
# only the tests against its gate cap. With no argument the gate runs in full.
build_only=0
case "${1-}" in
  "") ;;
  --build-only) build_only=1 ;;
  *) echo "usage: bash gate.sh [--build-only]" >&2; exit 2 ;;
esac
if [ "$#" -gt 1 ]; then echo "usage: bash gate.sh [--build-only]" >&2; exit 2; fi

CHROME_VERSION="156.0.8078.79"
CHROME_SHA256="7088865f510de22a83226817266274a1a4a57157d78ab429e6315701b19669f7"


TERRAFORM_VERSION="1.13.3"
TERRAFORM_SHA256="71fc43d92ea09907be5d416d2405a6a9c2d1ceaed633f5e175c0af26e8c4b365"

# Everything fetched from the network (the browser, Terraform, its providers, the .deb
# archives) is kept under CARGO_HOME, keyed by the pinned sha256, so a run fetches a pin once.
# testenv mounts CARGO_HOME from the host, so only the first run on a box pays; a CI runner
# starts with it empty and fetches each. A fetch lands in a scratch directory beside its final
# name and is renamed in only after its sha256 checks, so a concurrent or interrupted run never
# leaves a half-written entry behind the final name.
cache="${CARGO_HOME:-$HOME/.cargo}/margin-gate"
mkdir -p "$cache/apt/partial"

# One feature-unified build serves every test below: the plain run, the integration tests and
# the browser tests are the same compile, so nothing is built twice.
tests=(cargo test --workspace --all-targets --features margin/integration)

# The distribution's shared libraries for the browser, plus unzip, curl and jq. Library
# names differ across releases (the t64 rename); take whichever one this archive has. One
# resolve and one install for the whole list; the .deb files are kept in the cache.
apt_libs() {
  # A fresh VM runs unattended-upgrades; wait for its dpkg lock rather than fail.
  apt-get update -o DPkg::Lock::Timeout=600 -o Acquire::Retries=3 >/dev/null
  local known lib cand found pkgs=()
  known="$(apt-cache pkgnames | sort)"
  for lib in libasound2 libatk-bridge2.0-0 libatk1.0-0 libcups2 libdbus-1-3 libdrm2 libgbm1 \
    libglib2.0-0 libnspr4 libnss3 libpango-1.0-0 libcairo2 libx11-6 libxcb1 libxcomposite1 \
    libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 fonts-liberation fonts-noto-cjk \
    fonts-noto-color-emoji; do
    found=""
    for cand in "$lib" "${lib}t64"; do
      if grep -qxF "$cand" <<<"$known"; then found="$cand"; break; fi
    done
    if [ -z "$found" ]; then
      echo "gate.sh: this archive has neither ${lib} nor ${lib}t64" >&2
      exit 1
    fi
    pkgs+=("$found")
  done
  DEBIAN_FRONTEND=noninteractive apt-get install -o DPkg::Lock::Timeout=600 \
    -o Dir::Cache::Archives="$cache/apt" "$@" -y -qq --no-install-recommends \
    "${pkgs[@]}" unzip curl ca-certificates jq >/dev/null
}

# fetch <name> <url> <sha256>: the unpacked archive is at $cache/<name>-<sha256> afterwards.
fetch() {
  local name="$1" url="$2" sha="$3" dest scratch
  dest="$cache/$name-$sha"
  if [ -d "$dest" ]; then return 0; fi
  scratch="$(mktemp -d "$cache/.fetch.XXXXXX")"
  curl -fsSL --retry 3 --max-time 600 -o "$scratch/a.zip" "$url"
  echo "$sha  $scratch/a.zip" | sha256sum --check --strict - >/dev/null
  mkdir "$scratch/out"
  unzip -q "$scratch/a.zip" -d "$scratch/out"
  # Another run may have renamed the same pin in meanwhile; either copy is the checked one.
  mv -T "$scratch/out" "$dest" 2>/dev/null || true
  rm -rf "$scratch"
  [ -d "$dest" ]
}

# The browser is the official Chrome for Testing build fetched by version and checked against a
# pinned sha256, so the suite runs the same browser on any distribution. Terraform is fetched by pinned version and sha256.
fetch_tools() {
  fetch chrome "https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/linux64/chrome-linux64.zip" "$CHROME_SHA256"
  fetch terraform "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_amd64.zip" "$TERRAFORM_SHA256"
}

if [ "$build_only" = 1 ]; then
  # Compiles and fetches what the gate needs and runs no test, so the gate itself is charged
  # only for running them.
  cargo clippy --workspace --all-targets -- -D warnings
  "${tests[@]}" --no-run
  apt_libs
  fetch_tools
  exit 0
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
apt_libs
fetch_tools
"${tests[@]}"

# The backup bucket's Terraform root: checked and tested here, never applied.
export PATH="$cache/terraform-$TERRAFORM_SHA256:$PATH"
export TF_PLUGIN_CACHE_DIR="$cache/terraform-plugins"
mkdir -p "$TF_PLUGIN_CACHE_DIR"
(
  cd infra-backups
  terraform fmt -check -recursive
  terraform init -backend=false -input=false
  terraform validate
  terraform test
)

MARGIN_CHROMIUM="$cache/chrome-$CHROME_SHA256/chrome-linux64/chrome" \
  "${tests[@]}" -- --ignored
