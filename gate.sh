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

# Playwright v1.64.1's WebKit, revision 2370: one build per distribution, each pinned by sha256.
WEBKIT_REVISION="2370"

TERRAFORM_VERSION="1.13.3"
TERRAFORM_SHA256="71fc43d92ea09907be5d416d2405a6a9c2d1ceaed633f5e175c0af26e8c4b365"

if [ "$build_only" = 1 ]; then
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace --all-targets --no-run
  cargo test -p margin --features integration --test http --test feed --test deploy --no-run
  cargo test -p margin-ui --no-run
  exit 0
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test -p margin --features integration --test http --test feed

# The browser is the official Chrome for Testing build fetched by version and checked against a
# pinned sha256, so the suite runs the same browser on any distribution. Only its shared
# libraries and unzip come from the distribution's apt, unpinned.
# The WebKit build is per distribution; any other is refused by name rather than guessed at.
# shellcheck source=/dev/null
distro="$(. /etc/os-release && echo "${ID-}-${VERSION_ID-}")"
case "$distro" in
  debian-12)
    webkit_build="webkit-debian-12"
    WEBKIT_SHA256="4eeb0c622e298eaadabc822a2c87ee36e96fb2c6595c0598e991e7632fb78f0f"
    webkit_libs="libicu72 libavif15"
    ;;
  ubuntu-24.04)
    webkit_build="webkit-ubuntu-24.04"
    WEBKIT_SHA256="63b31a5d68d908d9aee2e445b88edc4c1e1d1eba488f5bd7e35668f91315b968"
    webkit_libs="libicu74 libavif16 libflite1 libjpeg-turbo8 libopus0 libwebp7 libwebpmux3 libxml2 libx264-164 libgstreamer-plugins-base1.0-0"
    ;;
  *)
    echo "gate.sh: no WebKit build is pinned for ${distro}; only debian-12 and ubuntu-24.04 are" >&2
    exit 1
    ;;
esac

# A fresh VM runs unattended-upgrades; wait for its dpkg lock rather than fail.
apt-get update -o DPkg::Lock::Timeout=600 -o Acquire::Retries=3 >/dev/null
# Library names differ across releases (the t64 rename); take whichever one this archive has.
for lib in libasound2 libatk-bridge2.0-0 libatk1.0-0 libcups2 libdbus-1-3 libdrm2 libgbm1 \
  libglib2.0-0 libnspr4 libnss3 libpango-1.0-0 libcairo2 libx11-6 libxcb1 libxcomposite1 \
  libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 fonts-liberation fonts-noto-cjk \
  fonts-noto-color-emoji \
  libsoup-3.0-0 gstreamer1.0-libav gstreamer1.0-plugins-bad gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good libegl1 libenchant-2-2 libepoxy0 libevdev2 libgles2 \
  libgstreamer-gl1.0-0 libgtk-4-1 libgudev-1.0-0 libharfbuzz-icu0 libhyphen0 liblcms2-2 \
  libmanette-0.2-0 libnotify4 libopengl0 libopenjp2-7 libproxy1v5 libsecret-1-0 \
  libwayland-server0 libwebpdemux2 libwoff1 libxslt1.1 libatomic1 libevent-2.1-7 \
  $webkit_libs; do
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
DEBIAN_FRONTEND=noninteractive apt-get install -o DPkg::Lock::Timeout=600 -y -qq --no-install-recommends unzip curl ca-certificates jq >/dev/null
cargo test -p margin --features integration --test deploy

# The backup bucket's Terraform root: fetched by pinned version and sha256 (unzip and curl are
# installed above), checked and tested here, never applied.
tf_dir="$(mktemp -d)"
trap 'rm -rf "$tf_dir"' EXIT
if ! terraform version 2>/dev/null | head -n 1 | grep -qx "Terraform v${TERRAFORM_VERSION}"; then
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

curl -fsSL --retry 3 --max-time 600 -o "$browser_dir/webkit.zip" \
  "https://cdn.playwright.dev/builds/webkit/${WEBKIT_REVISION}/${webkit_build}.zip"
echo "${WEBKIT_SHA256}  $browser_dir/webkit.zip" | sha256sum --check --strict -
mkdir "$browser_dir/webkit"
unzip -q "$browser_dir/webkit.zip" -d "$browser_dir/webkit"
chmod +x "$browser_dir/webkit/pw_run.sh"

MARGIN_CHROMIUM="$browser_dir/chrome-linux64/chrome" MARGIN_WEBKIT="$browser_dir/webkit/pw_run.sh" \
  cargo test -p margin-ui -- --ignored
