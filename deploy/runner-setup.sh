#!/usr/bin/env bash
# Register this host's `margin-prod` runner as a systemd user service (one-time).
#
# The registration token never goes through a transcript. On a machine with `gh`, run
#   gh api -X POST repos/DeckDumpster/margin/actions/runners/registration-token --jq .token
# then, on the product host:
#   MARGIN_RUNNER_TOKEN=<that token> deploy/runner-setup.sh
set -euo pipefail

: "${MARGIN_RUNNER_TOKEN:?set MARGIN_RUNNER_TOKEN to a one-time registration token (see the header)}"
repo_url="https://github.com/DeckDumpster/margin"
dir="$HOME/actions-runner-margin"
version="${MARGIN_RUNNER_VERSION:?set MARGIN_RUNNER_VERSION to the actions/runner release to install, e.g. 2.330.0}"

mkdir -p "$dir" "$HOME/margin-prod"
cd "$dir"
if [ ! -x ./config.sh ]; then
  curl -fsSL "https://github.com/actions/runner/releases/download/v${version}/actions-runner-linux-x64-${version}.tar.gz" | tar xz
fi
./config.sh --unattended --url "$repo_url" --token "$MARGIN_RUNNER_TOKEN" \
  --name "margin-prod-$(hostname)" --labels margin-prod --replace

if [ ! -d "$HOME/margin-prod/.git" ]; then
  git clone "$repo_url" "$HOME/margin-prod"
fi

unit_dir="$HOME/.config/systemd/user"
mkdir -p "$unit_dir"
cat > "$unit_dir/margin-runner.service" <<UNIT
[Unit]
Description=margin-prod GitHub Actions runner

[Service]
WorkingDirectory=$dir
ExecStart=$dir/run.sh
Restart=always

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable --now margin-runner.service
loginctl enable-linger "$USER"
echo "runner registered; the Deploy workflow runs on it"
