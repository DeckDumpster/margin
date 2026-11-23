# margin

A reader for commenting on whole documents from a phone.

A document is shown whole, with line numbers. A comment anchors on a line, on a range of
lines, or on selected words, and any number of anchors may cover the same line. Agents post
documents and comments over HTTP and read a change feed.

## Production

margin runs on the product host named in `deploy/prod-host`, as the systemd user unit
`margin.service`. It listens on the host's tailnet address only and has no sign-in: the tailnet
is the guard, so the server refuses an unspecified listen address (every interface). Its data is
one SQLite file under `~/.local/share/margin/`.

**Deploy.** Every push to `main` runs `CI` (`.github/workflows/ci.yml`): the gate, then a release
build with `MARGIN_BUILD_SHA` set, uploaded as the artifact `margin-<sha>`. When CI succeeds,
`Deploy` (`.github/workflows/deploy.yml`) runs on the `margin-prod` runner on the host, downloads
that artifact, fast-forwards `~/margin-prod` to the sha, and runs `deploy/deploy.sh`. The host
never builds; it installs what CI tested, keeps one previous copy as `margin.prev`, renders
`~/.config/margin/margin.toml` from `deploy/margin.toml.in` with the tailnet address, restarts the
unit and waits for `/healthz` to report the deployed sha.

**One-time setup.** Register the runner on the host (the token is single-use and stays out of
transcripts; the header of `deploy/runner-setup.sh` has the `gh api` command that mints it):

    MARGIN_RUNNER_TOKEN=… MARGIN_RUNNER_VERSION=… deploy/runner-setup.sh

**Agents.** An agent posts with a bearer token. On the host:

    margin agent-token add <name> --agents-file ~/.config/margin/agents.toml --token-file <out>

writes a fresh token to `<out>` (mode 0600) and its sha256 to the agents file; neither is printed.

    curl -H "Authorization: Bearer $(cat <out>)" -H 'Content-Type: application/json' \
      -d '{"title":"…","cover_note":"…","documents":[{"path":"a.rs","kind":"code","text":"…"}]}' \
      "$BASE_URL/api/collections"      # → 201 {"id": …, "url": …}

**Metrics.** margin pushes OTLP to the `otlp_endpoint` in its config; `deploy/grafana/margin.json`
is the dashboard.

**Rollback.** `mv ~/.local/lib/margin/margin.prev ~/.local/lib/margin/margin && systemctl --user restart margin`.
