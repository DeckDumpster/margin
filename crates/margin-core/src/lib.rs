//! Pure margin logic: no I/O. Time and randomness arrive through traits so
//! every consumer can be driven by a simulation.

pub mod lines;

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, from whatever clock the caller injects.
pub trait Clock: Send + Sync {
    fn now_unix(&self) -> i64;
}

/// Source of random bytes; ids and tokens are drawn from it.
pub trait Rng: Send + Sync {
    fn fill(&self, buf: &mut [u8]);
}

/// Lower-case hex of `n` random bytes.
pub fn random_hex(rng: &dyn Rng, n: usize) -> String {
    let mut buf = vec![0u8; n];
    rng.fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh collection or document id: 8 random bytes in hex.
pub fn new_id(rng: &dyn Rng) -> String {
    random_hex(rng, 8)
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}
