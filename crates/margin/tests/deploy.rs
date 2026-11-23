//! `deploy/deploy.sh` run for real, against stubs for the host's tools, then the two servers it
//! configures built in-process from the configs it rendered.
//!
//! This suite spawns processes because the script under test is the deliverable: nothing short
//! of running it shows what it installs. The stubs stand in for the host (`hostname`, `git`,
//! `tailscale`, `systemctl`, `journalctl`, `curl`, `sleep`); `jq`, `sed` and `install` are real.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use margin::agents::OsRng;
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin_core::{Clock, Rng};
use margin_test_id::test_id;
use serde_json::{Value, json};
use tower::ServiceExt;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const DNS: &str = "product.tail1234.ts.net";

struct Run {
    root: tempfile::TempDir,
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    fn conf(&self) -> PathBuf {
        self.home().join(".config/margin")
    }
    fn scratch_conf(&self) -> PathBuf {
        self.conf().join("scratch")
    }
    fn token(&self) -> String {
        std::fs::read_to_string(self.scratch_conf().join("token")).unwrap()
    }
    fn systemctl_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("systemctl.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn stub(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fresh temporary tree: a copy of `deploy/`, a `HOME`, and the stubs.
fn prepare() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let deploy = root.path().join("deploy");
    std::fs::create_dir_all(&deploy).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), deploy.join(entry.file_name())).unwrap();
        }
    }
    std::fs::write(deploy.join("prod-host"), "product-host\n").unwrap();
    std::fs::create_dir_all(root.path().join("home")).unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let r = root.path().display();
    stub(&bin, "hostname", "echo product-host");
    stub(&bin, "git", &format!("echo {SHA}"));
    stub(
        &bin,
        "tailscale",
        &format!(
            "if [ \"$1\" = ip ]; then echo 127.0.0.1; else echo '{{\"Self\":{{\"DNSName\":\"{DNS}.\"}}}}'; fi"
        ),
    );
    stub(
        &bin,
        "systemctl",
        &format!("echo \"$*\" >> {r}/systemctl.log"),
    );
    stub(&bin, "journalctl", "echo \"journal: $*\"");
    stub(&bin, "sleep", "exit 0");
    // /healthz answers the sha; a file `sha-<port>` in the tree overrides it for that port.
    stub(
        &bin,
        "curl",
        &format!(
            "url=\"${{@: -1}}\"; port=\"${{url##*:}}\"; port=\"${{port%%/*}}\"\n\
             sha={SHA}; [ -f {r}/sha-$port ] && sha=\"$(cat {r}/sha-$port)\"\n\
             echo \"{{\\\"sha\\\":\\\"$sha\\\"}}\""
        ),
    );
    root
}

fn deploy_in(root: tempfile::TempDir) -> Run {
    let out = Command::new("bash")
        .arg(root.path().join("deploy/deploy.sh"))
        .arg(env!("CARGO_BIN_EXE_margin"))
        .env("HOME", root.path().join("home"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env_remove("MARGIN_PORT")
        .env_remove("MARGIN_OTLP_ENDPOINT")
        .output()
        .unwrap();
    Run {
        root,
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn deploy() -> Run {
    let run = deploy_in(prepare());
    assert!(run.status.success(), "{}\n{}", run.stdout, run.stderr);
    run
}

fn redeploy(run: Run) -> Run {
    let again = deploy_in(run.root);
    assert!(again.status.success(), "{}\n{}", again.stdout, again.stderr);
    again
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        1_700_000_000
    }
}

struct CountingRng(AtomicU64);
impl Rng for CountingRng {
    fn fill(&self, buf: &mut [u8]) {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (n >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
}

/// The router a rendered config describes, as `tests/http.rs` builds one.
fn router_for(conf: &Path, seed: u64) -> Router {
    let config = Config::load(&conf.join("margin.toml")).unwrap();
    let state = AppState {
        db: Db::open(&config.database).unwrap(),
        config: Arc::new(config),
        clock: Arc::new(FixedClock),
        rng: Arc::new(CountingRng(AtomicU64::new(seed))),
        metrics: Metrics::detached(),
        highlighter: Arc::new(Highlighter::new()),
    };
    router(state)
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn create(app: &Router, token: &str, title: &str) -> (StatusCode, Value) {
    let (s, body) = send(
        app,
        Method::POST,
        "/api/collections",
        Some(token),
        Some(json!({"title": title, "cover_note": "n", "documents": [
            {"path": "a.rs", "kind": "code", "text": "fn a() {}\nfn b() {}\n"}]})),
    )
    .await;
    (s, serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// A production agent added through the library, as an operator would on the host.
fn add_production_agent(run: &Run) -> String {
    let token_file = run.root.path().join("prod-token");
    margin::agents::add(
        "operator",
        &run.conf().join("agents.toml"),
        &token_file,
        &OsRng,
    )
    .unwrap();
    std::fs::read_to_string(token_file).unwrap()
}

/// A reader's thread on line 1 of the collection's first version.
async fn reader_comments(app: &Router, id: &str) {
    let (_, body) = send(
        app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    let c: Value = serde_json::from_str(&body).unwrap();
    let version_id = c["documents"][0]["versions"][0]["id"]
        .as_i64()
        .or_else(|| c["documents"][0]["version_id"].as_i64())
        .unwrap_or_else(|| panic!("no version id in {body}"));
    let (s, b) = send(
        app,
        Method::POST,
        &format!("/c/{id}/threads"),
        None,
        Some(json!({"version_id": version_id, "first_line": 1, "last_line": 1, "body": "a walk"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[test_id(t4zkcxj)]
#[tokio::test]
async fn a_scratch_collection_is_served_by_scratch_alone() {
    let run = deploy();
    let scratch = router_for(&run.scratch_conf(), 1);
    let production = router_for(&run.conf(), 100);
    let prod_token = add_production_agent(&run);
    let token = run.token();

    let (s, created) = create(&scratch, &token, "walk").await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        created["url"],
        format!("http://{DNS}:7341/c/{id}"),
        "{created}"
    );

    let (s, _) = send(&scratch, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (_, home) = send(&scratch, Method::GET, "/", None, None).await;
    assert!(home.contains(&format!("/c/{id}")), "scratch home lacks it");
    let (_, home) = send(&production, Method::GET, "/", None, None).await;
    assert!(!home.contains(&id), "production home links it");
    let (s, _) = send(
        &production,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    reader_comments(&scratch, &id).await;
    let (s, feed) = send(
        &production,
        Method::GET,
        "/api/changes?after=0",
        Some(&prod_token),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!feed.contains(&id), "production feed carries it: {feed}");
}

#[test_id(to9civq)]
#[tokio::test]
async fn the_scratch_agent_deletes_its_collection_with_its_versions_and_threads() {
    let run = deploy();
    let scratch = router_for(&run.scratch_conf(), 1);
    let token = run.token();
    let (_, created) = create(&scratch, &token, "walk").await;
    let id = created["id"].as_str().unwrap().to_string();
    let (_, body) = send(
        &scratch,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    let c: Value = serde_json::from_str(&body).unwrap();
    let doc_id = c["documents"][0]["id"].as_str().unwrap().to_string();
    let (s, b) = send(
        &scratch,
        Method::POST,
        &format!("/api/documents/{doc_id}/versions"),
        Some(&token),
        Some(json!({"text": "fn a() {}\n"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    reader_comments(&scratch, &id).await;

    let (s, _) = send(
        &scratch,
        Method::DELETE,
        &format!("/api/collections/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, home) = send(&scratch, Method::GET, "/", None, None).await;
    assert!(!home.contains(&id));
    let (s, _) = send(&scratch, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, list) = send(
        &scratch,
        Method::GET,
        "/api/collections",
        Some(&token),
        None,
    )
    .await;
    assert!(!list.contains(&id), "{list}");
}

#[test_id(tx4kzhl)]
#[tokio::test]
async fn a_production_collection_is_out_of_the_scratch_agents_reach() {
    let run = deploy();
    let scratch = router_for(&run.scratch_conf(), 1);
    let production = router_for(&run.conf(), 100);
    let prod_token = add_production_agent(&run);
    let token = run.token();
    let (s, created) = create(&production, &prod_token, "real").await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();

    let (s, _) = send(&scratch, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(
        &scratch,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(
        &scratch,
        Method::DELETE,
        &format!("/api/collections/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let denied = [
        (Method::DELETE, format!("/api/collections/{id}"), None),
        (
            Method::PATCH,
            format!("/api/collections/{id}"),
            Some(json!({"title": "x"})),
        ),
        (
            Method::POST,
            format!("/api/collections/{id}/threads"),
            Some(json!({"path": "a.rs", "first_line": 1, "last_line": 1, "body": "x"})),
        ),
        (Method::GET, format!("/api/collections/{id}/threads"), None),
        (Method::GET, "/api/changes?after=0".to_string(), None),
    ];
    for (method, uri, body) in denied {
        let (s, _) = send(&production, method.clone(), &uri, Some(&token), body).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    let (s, _) = send(
        &production,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[test_id(tqxd8xt)]
#[test]
fn the_scratch_token_is_private_and_a_second_deploy_leaves_it_alone() {
    use std::os::unix::fs::MetadataExt;
    let run = deploy();
    let token = run.token();
    let mode = std::fs::metadata(run.scratch_conf().join("token"))
        .unwrap()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    for dir in [
        run.scratch_conf(),
        run.home().join(".local/share/margin/scratch"),
    ] {
        assert_eq!(std::fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
    }
    assert!(!run.stdout.contains(&token) && !run.stderr.contains(&token));
    let production_agents = std::fs::read_to_string(run.conf().join("agents.toml")).unwrap();
    assert!(
        !production_agents.contains("scratch"),
        "{production_agents}"
    );

    let agents = std::fs::read(run.scratch_conf().join("agents.toml")).unwrap();
    let before = std::fs::read_to_string(run.scratch_conf().join("margin.toml")).unwrap();
    let again = redeploy(run);
    assert_eq!(again.token(), token);
    assert_eq!(
        std::fs::read(again.scratch_conf().join("agents.toml")).unwrap(),
        agents
    );
    let after = std::fs::read_to_string(again.scratch_conf().join("margin.toml")).unwrap();
    assert_eq!(before, after);
    assert!(after.contains("share/margin/scratch/margin.db"), "{after}");
}

#[test_id(tc8zsi6)]
#[test]
fn a_lost_token_or_emptied_agents_file_is_replaced_by_a_pair_that_authenticates() {
    for damage in ["token", "agents.toml"] {
        let run = deploy();
        let old = run.token();
        let target = run.scratch_conf().join(damage);
        if damage == "token" {
            std::fs::remove_file(&target).unwrap();
        } else {
            std::fs::write(&target, "").unwrap();
        }
        let run = redeploy(run);
        let token = run.token();
        assert_ne!(token, old, "{damage}");
        let agents = std::fs::read_to_string(run.scratch_conf().join("agents.toml")).unwrap();
        assert_eq!(
            margin::agents::authenticate(&agents, &token).as_deref(),
            Some("scratch")
        );
        assert_eq!(margin::agents::authenticate(&agents, &old), None);
    }
}

#[test_id(trw4crj)]
#[test]
fn both_units_are_enabled_and_restarted_production_first() {
    let run = deploy();
    let log = run.systemctl_log();
    let at = |line: &str| {
        log.iter()
            .position(|l| l == line)
            .unwrap_or_else(|| panic!("{line} not in {log:?}"))
    };
    let order = [
        at("--user enable margin.service"),
        at("--user restart margin.service"),
        at("--user enable margin-scratch.service"),
        at("--user restart margin-scratch.service"),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{log:?}");
}

#[test_id(trdws0s)]
#[test]
fn scratch_not_reporting_the_sha_fails_the_deploy_and_names_its_unit() {
    let root = prepare();
    std::fs::write(
        root.path().join("sha-7341"),
        "ffffffffffffffffffffffffffffffffffffffff",
    )
    .unwrap();
    let run = deploy_in(root);
    assert!(!run.status.success());
    assert!(
        run.stderr.contains("margin-scratch.service"),
        "{}",
        run.stderr
    );
    assert!(run.stdout.contains("margin production"), "{}", run.stdout);
}

#[test_id(tyolw9t)]
#[test]
fn production_keeps_its_listen_address_base_url_database_and_agents_file() {
    let run = deploy();
    let text = std::fs::read_to_string(run.conf().join("margin.toml")).unwrap();
    let table: toml::Table = text.parse().unwrap();
    let home = run.home();
    let want = [
        ("listen", "127.0.0.1:7340".to_string()),
        ("base_url", format!("http://{DNS}:7340")),
        (
            "database",
            format!("{}/.local/share/margin/margin.db", home.display()),
        ),
        (
            "agents_file",
            format!("{}/.config/margin/agents.toml", home.display()),
        ),
        (
            "otlp_endpoint",
            "http://localhost:8428/opentelemetry/v1/metrics".to_string(),
        ),
        ("instance", "production".to_string()),
    ];
    assert_eq!(table.len(), want.len(), "{text}");
    for (k, v) in want {
        assert_eq!(table[k].as_str(), Some(v.as_str()), "{k}");
    }
    let timer = run.systemctl_log();
    assert!(
        !timer.iter().any(|l| l.contains("margin-backup.timer")),
        "{timer:?}"
    );
    assert!(run.stderr.contains("backups are NOT running"));
}

#[test_id(tasvaux)]
#[test]
fn another_host_is_refused_and_nothing_is_created() {
    let root = prepare();
    std::fs::write(root.path().join("deploy/prod-host"), "elsewhere\n").unwrap();
    let run = deploy_in(root);
    assert!(!run.status.success());
    assert!(run.stderr.contains("refusing"), "{}", run.stderr);
    assert_eq!(std::fs::read_dir(run.home()).unwrap().count(), 0);
}
