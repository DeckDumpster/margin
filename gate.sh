#!/usr/bin/env bash
# The one gate margin's CI and `testenv --suites gate` both run. It asserts nothing itself:
# every assertion lives in a cargo test.
set -euo pipefail

CHROME_VERSION="156.0.8078.79"
CHROME_SHA256="7088865f510de22a83226817266274a1a4a57157d78ab429e6315701b19669f7"

TERRAFORM_VERSION="1.13.3"
TERRAFORM_SHA256="71fc43d92ea09907be5d416d2405a6a9c2d1ceaed633f5e175c0af26e8c4b365"

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test -p margin --features integration --test http --test feed

# The browser is the official Chrome for Testing build fetched by version and checked against a
# pinned sha256, so the suite runs the same browser on any distribution. Only its shared
# libraries and unzip come from the distribution's apt, unpinned.
# A fresh VM runs unattended-upgrades; wait for its dpkg lock rather than fail.
apt-get update -o DPkg::Lock::Timeout=600 -o Acquire::Retries=3 >/dev/null
# Library names differ across releases (the t64 rename); take whichever one this archive has.
for lib in libasound2 libatk-bridge2.0-0 libatk1.0-0 libcups2 libdbus-1-3 libdrm2 libgbm1 \
  libglib2.0-0 libnspr4 libnss3 libpango-1.0-0 libcairo2 libx11-6 libxcb1 libxcomposite1 \
  libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 fonts-liberation fonts-noto-cjk \
  fonts-noto-color-emoji; do
  found=""
  for cand in "$lib" "${lib}t64"; do
    if apt-get install -s --no-install-recommends "$cand" >/dev/null 2>&1; then found="$cand"; break; fi
  done
  if [ -z "$found" ]; then
    echo "gate.sh: this archive has neither ${lib} nor ${lib}t64" >&2
    exit 1
  fi
  DEBIAN_FRONTEND=noninteractive apt-get install -o DPkg::Lock::Timeout=600 -y -qq --no-install-recommends "$found" >/dev/null
done
DEBIAN_FRONTEND=noninteractive apt-get install -o DPkg::Lock::Timeout=600 -y -qq --no-install-recommends unzip curl ca-certificates >/dev/null

# The backup bucket's Terraform root: fetched by pinned version and sha256 (unzip and curl are
# installed above), checked and tested here, never applied.
tf_dir="$(mktemp -d)"
trap 'rm -rf "$tf_dir"' EXIT
if ! command -v terraform >/dev/null 2>&1; then
  curl -fsSL --retry 3 --max-time 300 -o "$tf_dir/terraform.zip" \
    "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_amd64.zip"
  echo "${TERRAFORM_SHA256}  $tf_dir/terraform.zip" | sha256sum --check --strict -
  unzip -q "$tf_dir/terraform.zip" terraform -d "$tf_dir"
  export PATH="$tf_dir:$PATH"
fi
(
  cd infra-backups
  terraform fmt -check -recursive
  terraform init -backend=false -input=false
  terraform validate
  terraform test
)

browser_dir="$(mktemp -d)"
trap 'rm -rf "$browser_dir" "$tf_dir"' EXIT
curl -fsSL --retry 3 --max-time 300 -o "$browser_dir/chrome.zip" \
  "https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/linux64/chrome-linux64.zip"
echo "${CHROME_SHA256}  $browser_dir/chrome.zip" | sha256sum --check --strict -
unzip -q "$browser_dir/chrome.zip" -d "$browser_dir"

MARGIN_CHROMIUM="$browser_dir/chrome-linux64/chrome" cargo test -p margin-ui -- --ignored
