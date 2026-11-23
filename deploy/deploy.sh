#!/usr/bin/env bash
# Install the binary CI built and tested, and restart margin on this host.
# usage: deploy.sh <path to the margin binary>
# Run from the production checkout by the Deploy workflow; refuses on any other host.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
binary="${1:?usage: deploy.sh <path to the margin binary>}"
port="${MARGIN_PORT:-7340}"
otlp="${MARGIN_OTLP_ENDPOINT:-http://localhost:8428/opentelemetry/v1/metrics}"

want="$(tr -d '[:space:]' < "$here/prod-host")"
have="$(hostname)"
if [ "$have" != "$want" ]; then
  echo "deploy.sh: refusing: this host is $have, the product host is $want" >&2
  exit 1
fi

sha="$(git -C "$here" rev-parse HEAD)"
lib="$HOME/.local/lib/margin"
conf="$HOME/.config/margin"
data="$HOME/.local/share/margin"
unit_dir="$HOME/.config/systemd/user"
mkdir -p "$lib" "$conf" "$data" "$unit_dir"

# Install by copy then rename, keeping exactly one previous copy for rollback.
cp "$binary" "$lib/margin.new"
chmod 0755 "$lib/margin.new"
if [ -e "$lib/margin" ]; then
  mv -f "$lib/margin" "$lib/margin.prev"
fi
mv -f "$lib/margin.new" "$lib/margin"

# Listen on this host's tailnet address and nowhere else; both are read from tailscale now.
ip="$(tailscale ip -4 | head -n 1)"
dns="$(tailscale status --json | jq -r '.Self.DNSName' | sed 's/\.$//')"
if [ -z "$ip" ] || [ -z "$dns" ] || [ "$dns" = "null" ]; then
  echo "deploy.sh: refusing: tailscale reports no address or DNS name" >&2
  exit 1
fi
sed \
  -e "s|@LISTEN@|$ip:$port|" \
  -e "s|@BASE_URL@|http://$dns:$port|" \
  -e "s|@DATABASE@|$data/margin.db|" \
  -e "s|@AGENTS_FILE@|$conf/agents.toml|" \
  -e "s|@OTLP_ENDPOINT@|$otlp|" \
  "$here/margin.toml.in" > "$conf/margin.toml.new"
mv -f "$conf/margin.toml.new" "$conf/margin.toml"

if [ ! -e "$conf/agents.toml" ]; then
  (umask 077 && : > "$conf/agents.toml")
fi

install -m 0644 "$here/margin.service" "$unit_dir/margin.service"
systemctl --user daemon-reload
systemctl --user enable margin.service
systemctl --user restart margin.service

for _ in $(seq 30); do
  if reported="$(curl -fsS --max-time 2 "http://$ip:$port/healthz" 2>/dev/null)" \
    && [ "$(printf '%s' "$reported" | jq -r '.sha')" = "$sha" ]; then
    echo "deploy.sh: margin $sha is serving on http://$dns:$port"
    exit 0
  fi
  sleep 2
done

echo "deploy.sh: margin did not report $sha within 60s; last unit log:" >&2
journalctl --user -u margin.service -n 20 --no-pager >&2 || true
exit 1
