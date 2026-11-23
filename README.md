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

**Backups.** `margin-backup.timer` runs `margin-backup.service` every 15 minutes: `margin backup`
takes a consistent snapshot (`VACUUM INTO`, safe beside the running server and its WAL), gzips it
and uploads it as `margin-<UTC yyyymmddThhmmssZ>.db.gz` to a private, versioned S3 bucket in the CI
account, then pushes `margin_backup_*` metrics. The key can only `s3:PutObject`: a compromised host
cannot list, read, overwrite or delete history. Snapshots expire after 180 days.

*Plan and apply* (with the `terraform-deploy` role; the bucket is `margin-backups-<account id>`):

    cd infra-backups
    terraform init -backend-config="bucket=<state bucket>" -backend-config="role_arn=<deploy role>"
    terraform plan -var deploy_role_arn=<deploy role>
    terraform apply -var deploy_role_arn=<deploy role>

*`~/.config/margin/backup.env`* on the host, mode 0600, from the secret `margin/backup` in
Secrets Manager (do not print it):

    AWS_ACCESS_KEY_ID=…
    AWS_SECRET_ACCESS_KEY=…
    AWS_DEFAULT_REGION=us-west-2
    MARGIN_BACKUP_BUCKET=margin-backups-<account id>

`deploy.sh` enables the timer only once `backup.env` is non-empty; until then it prints on stderr
that backups are NOT running, and the deploy carries on.

*Drill.* With your own identity (not the backup key), fetch the newest snapshot and read it back:

    margin restore-drill --bucket margin-backups-<account id>

It lists the bucket, takes the newest object by last-modified, unzips it into a temporary
directory, runs `integrity_check` and `foreign_key_check`, refuses a `user_version` newer than the
binary, opens it with margin's own code and reads every collection, document and thread. It prints
the object key, its age and the collection, document, thread and comment counts, and exits
non-zero on any failure, an empty bucket, or a newest object older than 60 minutes.

*Restore.* Fetch an object with your own identity (`aws s3api get-object --bucket … --key … f.gz`),
`gunzip f.gz`, then stop the unit, put the restored file in place with its `-wal` and `-shm`
removed, and start the unit:

    systemctl --user stop margin.service
    rm -f ~/.local/share/margin/margin.db-wal ~/.local/share/margin/margin.db-shm
    cp f ~/.local/share/margin/margin.db
    systemctl --user start margin.service

**Rollback.** `mv ~/.local/lib/margin/margin.prev ~/.local/lib/margin/margin && systemctl --user restart margin`.
