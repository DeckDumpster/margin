//! margin: a reader for commenting on whole documents. The server library; the
//! binary in `main.rs` only parses arguments.

pub mod agents;
pub mod api;
pub mod config;
pub mod db;
pub mod highlight;
pub mod metrics;
pub mod page;
pub mod store;
pub mod threads;

/// The sha this binary was built from, `"unknown"` when the build did not say.
pub const BUILD_SHA: &str = match option_env!("MARGIN_BUILD_SHA") {
    Some(sha) => sha,
    None => "unknown",
};
