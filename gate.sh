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

# Everything fetched from the network (the two browsers, Terraform, its providers, the .deb
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

# The distribution's shared libraries for the two browsers, plus unzip, curl and jq. Library
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
    fonts-noto-color-emoji \
    libsoup-3.0-0 gstreamer1.0-libav gstreamer1.0-plugins-bad gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good libegl1 libenchant-2-2 libepoxy0 libevdev2 libgles2 \
    libgstreamer-gl1.0-0 libgtk-4-1 libgudev-1.0-0 libharfbuzz-icu0 libhyphen0 liblcms2-2 \
    libmanette-0.2-0 libnotify4 libopengl0 libopenjp2-7 libproxy1v5 libsecret-1-0 \
    libwayland-server0 libwebpdemux2 libwoff1 libxslt1.1 libatomic1 libevent-2.1-7 \
    $webkit_libs; do
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
# pinned sha256, so the suite runs the same browser on any distribution. The WebKit build is
# per distribution. Terraform is fetched by pinned version and sha256.
fetch_tools() {
  fetch chrome "https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/linux64/chrome-linux64.zip" "$CHROME_SHA256"
  fetch webkit "https://cdn.playwright.dev/builds/webkit/${WEBKIT_REVISION}/${webkit_build}.zip" "$WEBKIT_SHA256"
  fetch terraform "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_amd64.zip" "$TERRAFORM_SHA256"
  chmod +x "$cache/webkit-$WEBKIT_SHA256/pw_run.sh"
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

MARGIN_CHROMIUM="$cache/chrome-$CHROME_SHA256/chrome-linux64/chrome" MARGIN_WEBKIT="$cache/webkit-$WEBKIT_SHA256/pw_run.sh" \
  "${tests[@]}" -- --ignored
