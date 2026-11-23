use std::path::PathBuf;
use std::sync::Arc;

use margin::agents::OsRng;
use margin::api::{AppState, router};
use margin::backup::{self, AwsCli};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin_core::SystemClock;

const USAGE: &str = "usage:\n  margin serve --config <path>\n  margin backup --config <path>\n  margin restore-drill --bucket <name>\n  margin agent-token add <name> --agents-file <path> --token-file <path>";

fn flag(args: &[String], name: &str) -> Result<PathBuf, String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}\n{USAGE}"))
}

async fn serve(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let clock: Arc<dyn margin_core::Clock> = Arc::new(SystemClock);
    let state = AppState {
        db: Db::open(&config.database)?,
        metrics: Metrics::start(config.otlp_endpoint.clone(), clock.clone()),
        clock,
        rng: Arc::new(OsRng),
        highlighter: Arc::new(Highlighter::new()),
        config: Arc::new(config.clone()),
    };
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    eprintln!(
        "margin {} listening on {}",
        margin::BUILD_SHA,
        listener.local_addr()?
    );
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// `margin backup`: the bucket comes from the environment, the metrics endpoint from the config.
fn run_backup(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let bucket = backup::bucket_from(std::env::var(backup::BUCKET_ENV).ok())?;
    let store = AwsCli::new(bucket.clone());
    let endpoint = config.otlp_endpoint.clone();
    let sent = backup::execute(
        &config.database,
        &std::env::temp_dir(),
        &store,
        &SystemClock,
        &|req| {
            if let Err(e) = margin::metrics::send_once(&endpoint, req) {
                eprintln!("margin: backup metrics push failed: {e}");
            }
        },
    )?;
    println!("uploaded {} ({} bytes) to {bucket}", sent.key, sent.bytes);
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result: Result<(), Box<dyn std::error::Error>> = match args.first().map(String::as_str) {
        Some("serve") => match flag(&args, "--config") {
            Ok(p) => match Config::load(&p) {
                Ok(c) => serve(c).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e.into()),
        },
        Some("backup") => flag(&args, "--config")
            .map_err(Into::into)
            .and_then(|p| Config::load(&p))
            .and_then(|c| run_backup(&c)),
        Some("restore-drill") => match args
            .iter()
            .position(|a| a == "--bucket")
            .and_then(|i| args.get(i + 1))
        {
            None => Err(format!("missing --bucket\n{USAGE}").into()),
            Some(bucket) => backup::drill(&AwsCli::new(bucket.clone()), &SystemClock)
                .await
                .map(|report| println!("{report}"))
                .map_err(Into::into),
        },
        Some("agent-token") if args.get(1).map(String::as_str) == Some("add") && args.len() > 2 => {
            (|| -> Result<(), Box<dyn std::error::Error>> {
                let agents = flag(&args, "--agents-file")?;
                let token = flag(&args, "--token-file")?;
                margin::agents::add(&args[2], &agents, &token, &OsRng)?;
                Ok(())
            })()
        }
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("margin: {e}");
        std::process::exit(1);
    }
}
