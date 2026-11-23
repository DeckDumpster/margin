#!/usr/bin/env bash
# Install the binary CI built and tested, and restart margin on this host.
# usage: deploy.sh <path to the margin binary>
# Run from the production checkout by the Deploy workflow; refuses on any other host.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
binary="${1:?usage: deploy.sh <path to the margin binary>}"
production_port="${MARGIN_PORT:-7340}"
scratch_port=7341
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

unit_of() { if [ "$1" = production ]; then echo margin.service; else echo "margin-$1.service"; fi; }

# render_config <name> <port> <config dir> <data dir>: this instance's margin.toml, and an
# agents file if it has none.
render_config() {
  local name="$1" port="$2" cdir="$3" ddir="$4"
  sed \
    -e "s|@LISTEN@|$ip:$port|" \
    -e "s|@BASE_URL@|http://$dns:$port|" \
    -e "s|@DATABASE@|$ddir/margin.db|" \
    -e "s|@AGENTS_FILE@|$cdir/agents.toml|" \
    -e "s|@OTLP_ENDPOINT@|$otlp|" \
    -e "s|@INSTANCE@|$name|" \
    "$here/margin.toml.in" > "$cdir/margin.toml.new"
  mv -f "$cdir/margin.toml.new" "$cdir/margin.toml"
  if [ ! -e "$cdir/agents.toml" ]; then
    (umask 077 && : > "$cdir/agents.toml")
  fi
}

# restart_instance <name>: enable and restart its unit.
restart_instance() {
  local unit
  unit="$(unit_of "$1")"
  systemctl --user enable "$unit"
  systemctl --user restart "$unit"
}

# wait_healthy <name> <port>: succeed once /healthz reports this sha; else print the unit's log.
wait_healthy() {
  local name="$1" port="$2" unit reported
  unit="$(unit_of "$name")"
  for _ in $(seq 30); do
    if reported="$(curl -fsS --max-time 2 "http://$ip:$port/healthz" 2>/dev/null)" \
      && [ "$(printf '%s' "$reported" | jq -r '.sha')" = "$sha" ]; then
      echo "deploy.sh: margin $name $sha is serving on http://$dns:$port"
      return 0
    fi
    sleep 2
  done
  echo "deploy.sh: $unit did not report $sha within 60s; last unit log:" >&2
  journalctl --user -u "$unit" -n 20 --no-pager >&2 || true
  exit 1
}

# The scratch agent: keep a token and agents file that are both present and non-empty;
# otherwise mint a new pair. The token is never printed.
ensure_scratch_agent() {
  local cdir="$1"
  if [ -s "$cdir/agents.toml" ] && [ -s "$cdir/token" ]; then
    return 0
  fi
  rm -f "$cdir/agents.toml" "$cdir/token"
  "$lib/margin" agent-token add scratch --agents-file "$cdir/agents.toml" --token-file "$cdir/token"
}

install -m 0644 "$here/margin.service" "$unit_dir/margin.service"
install -m 0644 "$here/margin-scratch.service" "$unit_dir/margin-scratch.service"
install -m 0644 "$here/margin-backup.service" "$unit_dir/margin-backup.service"
install -m 0644 "$here/margin-backup.timer" "$unit_dir/margin-backup.timer"
systemctl --user daemon-reload

# Production first: it restarts and reports the sha before scratch is touched.
render_config production "$production_port" "$conf" "$data"
restart_instance production

# The backup key lives in backup.env, placed by hand from the secret margin/backup (README,
# Backups). Without it the timer would fail every 15 minutes, so it is not enabled; say so loudly.
if [ -s "$conf/backup.env" ]; then
  systemctl --user enable --now margin-backup.timer
else
  echo "deploy.sh: WARNING: $conf/backup.env is missing or empty: backups are NOT running" >&2
fi

wait_healthy production "$production_port"

# Scratch: the same binary, its own database, agents file and one agent. Not backed up.
scratch_conf="$conf/scratch"
scratch_data="$data/scratch"
mkdir -p -m 0700 "$scratch_conf" "$scratch_data"
chmod 0700 "$scratch_conf" "$scratch_data"
ensure_scratch_agent "$scratch_conf"
render_config scratch "$scratch_port" "$scratch_conf" "$scratch_data"
restart_instance scratch
wait_healthy scratch "$scratch_port"
