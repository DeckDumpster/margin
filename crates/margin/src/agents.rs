//! Agents authenticate with a bearer token; the agents file maps each agent's
//! name to the sha256 hex of its token, so the token itself is never stored.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use margin_core::Rng;
use sha2::{Digest, Sha256};

pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The agent whose token this is, per the agents file as it is now.
pub fn authenticate(agents_toml: &str, token: &str) -> Option<String> {
    let table: toml::Table = agents_toml.parse().ok()?;
    let want = hash_token(token);
    table
        .iter()
        .find(|(_, v)| v.as_str().is_some_and(|h| h == want))
        .map(|(name, _)| name.clone())
}

/// `margin agent-token add`: write a fresh token to `token_file` (0600, never overwritten)
/// and its hash under `name` in `agents_file`. Neither is printed.
pub fn add(name: &str, agents_file: &Path, token_file: &Path, rng: &dyn Rng) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "agent name {name:?} must be letters, digits, - or _"
        ));
    }
    let existing = match std::fs::read_to_string(agents_file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("reading {}: {e}", agents_file.display())),
    };
    let mut table: toml::Table = existing
        .parse()
        .map_err(|e| format!("{} is not TOML: {e}", agents_file.display()))?;
    if table.contains_key(name) {
        return Err(format!(
            "agent {name} already exists in {}",
            agents_file.display()
        ));
    }
    let token = margin_core::random_hex(rng, 32);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(token_file)
        .map_err(|e| format!("creating {}: {e}", token_file.display()))?;
    out.write_all(token.as_bytes()).map_err(|e| e.to_string())?;
    table.insert(name.to_string(), toml::Value::String(hash_token(&token)));
    let text = toml::to_string(&table).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(agents_file)
        .map_err(|e| format!("writing {}: {e}", agents_file.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// Operating-system randomness.
pub struct OsRng;

impl Rng for OsRng {
    fn fill(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("operating system randomness is available");
    }
}
