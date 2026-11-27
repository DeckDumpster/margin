# margin

A place to read a whole document on a phone and write in its margin, with an agent on the other
end of every comment.

## Why it exists

Reviewing an agent's work kept meaning reading it somewhere built for something else. A pull
request shows a diff, when what needed reading was the whole file. A terminal review tool shows
the whole file, but only at a desk. A chat window can show neither, and a comment made there
has no line to hang on.

margin is the smallest thing that fixes that. An agent posts a set of documents with a note
saying what to read first. A person opens one link on a phone, reads each document whole, and
comments on a line, a range of lines or a few selected words. Each comment reaches the agent as
it is made, and the agent answers in the same thread. When the document is revised, the new
version appears in the same place with the threads carried to where their lines went.

It was built rather than adopted so that changing how the page behaves is a small change in one
small repository. It serves one reader and a few named agents, on a private network, and it is
deliberately not a diff viewer, a forge or a multi-user product.

## What it does

**For the reader**

- The home page lists open reviews, most recently active first.
- A review opens a page at a time: a cover note (rendered markdown) on the first page, a list
  of all its files, then that page's files, each whole, with line numbers and syntax colour. A
  page holds whole files up to `page_lines` lines; a longer file has a page to itself. The list
  opens any file, and the end of each page leads to the next. A bar stays at the top once you
  scroll, with the current file and controls to jump to the top and between files.
- Tap a line number to comment on that line. Press, hold and drag to comment on a range; the
  page scrolls under your finger. Select words the phone's usual way to comment on exactly
  those words.
- Any number of threads may cover the same line. The margin shows one mark per thread.
- Replies appear in the thread without a reload.
- Publish ends your pass. Comments an agent wrote beforehand and held back appear beside yours
  only then, so its read does not steer yours.
- Long lines wrap by default; a toggle turns wrapping off and each file then scrolls sideways
  inside its own frame. The page itself never scrolls sideways.

**For an agent**

Everything an agent does is plain HTTP with a bearer token.

| Call | What it does |
|---|---|
| `POST /api/collections` | open a review: a title, a cover note, and documents (`path`, `kind`, `text`) |
| `GET /api/collections`, `GET /api/collections/{id}` | list reviews; read one |
| `PATCH`, `DELETE /api/collections/{id}` | change or remove a review |
| `POST /api/documents/{id}/versions` | post a new version of a document; threads are carried to it |
| `GET`, `POST /api/collections/{id}/threads` | read threads; start one on an anchor, shown now or held until publish |
| `POST /api/threads/{id}/comments` | answer in a thread |
| `PATCH`, `DELETE /api/comments/{id}` | edit or remove a comment |
| `GET /api/changes?after=N&wait=S` | the change feed: everything after sequence `N`, held open up to `S` seconds |

The change feed is a long poll, so an agent learns of a comment when it is posted, with nothing
running on a timer.

```sh
curl -H "Authorization: Bearer $(cat token-file)" -H 'Content-Type: application/json' \
  -d '{"title":"…","cover_note":"…","source":{"repository":"https://github.com/org/name","commit":"<40 hex>"},"documents":[{"path":"a.rs","kind":"code","text":"…"}]}' \
  "$BASE_URL/api/collections"      # 201 {"id": …, "url": …}
```

## How it is put together

**The model.** A *document* is immutable text with a path, a kind (code or prose) and numbered
lines; a revision is a new *version* of the same document. A *collection* is an ordered set of
documents with a cover note and a state: open, published or closed. An *anchor* is a document
version with a first and last line, and optionally a start and end column for a highlight of
some words. A *thread* hangs on one anchor and holds *comments*, each with an author (the
reader, or a named agent) and a visibility (now, or after publish).

**Carrying threads to a new version.** An anchor moves by matching the text of its lines. Where
the text is gone the thread is kept at the nearest surviving line and says so. A highlight
follows its exact words, and falls back to its line when the words changed.

**The page.** Server-rendered HTML, one stylesheet and one script, with no build step. A change
to the surface is a change to a template.

**The server.** One Rust binary and one SQLite file. There is no sign-in: it is meant to listen
on a private network address, and it refuses to start on an unspecified address (every
interface). Agents are named in a file of token hashes.

| Path | Holds |
|---|---|
| `crates/margin-core` | anchors, line handling, carrying an anchor to a new version, clock and wait ports |
| `crates/margin` | the server: store, API, page, change feed, highlighting, backup, metrics |
| `crates/margin/templates`, `crates/margin/static` | the page, its stylesheet and its script |
| `crates/margin-ui` | the one browser test: a review is opened, found, read and commented on |
| `deploy/` | the deploy script, unit files, config template and dashboard |
| `infra-backups/` | Terraform for the backup bucket |

## Running it

```sh
margin agent-token add <name> --agents-file agents.toml --token-file <out>
margin serve --config margin.toml
```

`agent-token add` writes a fresh token to `<out>` (mode 0600) and its hash to the agents file;
neither is printed. The config has seven keys:

```toml
listen = "<address>:<port>"        # a specific address, never every interface
base_url = "http://<name>:<port>"  # what links in API answers start with
database = "/path/to/margin.db"
agents_file = "/path/to/agents.toml"
otlp_endpoint = "http://<collector>"
instance = "prod"
page_lines = 1500                  # the most lines of whole files one reader page shows
```

## Checking a change

```sh
bash gate.sh
```

runs formatting, lints, every test, the Terraform checks and the browser test. It fetches a
pinned Chrome and Terraform once and keeps them under `CARGO_HOME`. Tests sit at the lowest
level that can see the behaviour: what the server renders is asserted on the handler's response
in `crates/margin/tests/http.rs`, and the single browser test covers the reader's main path. A
change to that path adds a step to that test; it does not add another.

## Operating it

**Deploy.** A push to `main` runs `CI`, which builds a release binary stamped with the commit
and uploads it. `Deploy` then runs on a self-hosted runner on the host named in
`deploy/prod-host`: it downloads that binary, fast-forwards its checkout and runs
`deploy/deploy.sh`. The host never builds. The script keeps the previous binary as
`margin.prev`, renders the config from `deploy/margin.toml.in`, restarts `margin.service` (a
systemd user unit) and waits for `/healthz` to report the deployed commit.

**Rollback.** `mv ~/.local/lib/margin/margin.prev ~/.local/lib/margin/margin && systemctl --user restart margin`.

**Scratch.** Every deploy also serves a second instance, `margin-scratch.service`, from the same
binary on the next port, with its own database, agents file and one agent. It is for trying
things; it is not backed up, and whoever uses it deletes what they post.

**Backups.** `margin-backup.timer` runs `margin backup` every 15 minutes: a consistent snapshot
of the database, gzipped and uploaded to a private, versioned bucket with a key that can only
write. `margin restore-drill --bucket <bucket>` fetches the newest snapshot, checks its
integrity, opens it with margin's own code, reads every collection, and fails if the newest
snapshot is more than an hour old. To restore, stop the unit, put the unzipped snapshot in
place of the database with its `-wal` and `-shm` files removed, and start the unit. The bucket
is defined in `infra-backups/`.

**Metrics.** margin pushes OTLP to `otlp_endpoint`; `deploy/grafana/margin.json` is the
dashboard.

**First-time setup of a host.** `deploy/runner-setup.sh` registers the deploy runner; its header
says how to mint the single-use registration token. The backup key (the secret `margin/backup`) goes in
`~/.config/margin/backup.env` (mode 0600); until that file exists the deploy says on stderr that
backups are not running, and carries on.
