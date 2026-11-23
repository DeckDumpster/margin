//! The change feed's wake-up: a `watch` channel carrying the newest change sequence number, and
//! the long-poll loop that awaits it. Nothing sleeps and nothing blocks a worker thread: a
//! waiting request is one future parked on the channel under `tokio::time::timeout`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// The longest any feed call is held open, whatever `wait` asked for.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// Rows in one feed answer.
pub const MAX_ROWS: usize = 100;

/// The wait a caller asked for, in seconds, held to [`MAX_WAIT`].
pub fn clamp_wait(seconds: u64) -> Duration {
    Duration::from_secs(seconds).min(MAX_WAIT)
}

#[derive(Clone)]
pub struct Notifier {
    tx: Arc<watch::Sender<i64>>,
}

impl Notifier {
    pub fn new(newest: i64) -> Notifier {
        Notifier {
            tx: Arc::new(watch::channel(newest).0),
        }
    }

    /// Announce that changes up to `seq` are committed. A stale `seq` wakes no one.
    pub fn publish(&self, seq: i64) {
        self.tx.send_if_modified(|cur| {
            let newer = seq > *cur;
            if newer {
                *cur = seq;
            }
            newer
        });
    }

    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.tx.subscribe()
    }
}

/// What one look at the store found.
pub enum Probe<T> {
    /// There is something to answer with.
    Ready(T),
    /// There is not; this is the answer if the wait runs out.
    Empty(T),
}

/// Look, and while the look comes back empty await the notifier until `wait` has passed. The
/// subscription is taken before the first look, so a change committed between a look and the
/// await is not lost.
pub async fn long_poll<T, E, F, Fut>(
    notifier: &Notifier,
    wait: Duration,
    mut probe: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Probe<T>, E>>,
{
    let mut rx = notifier.subscribe();
    let deadline = Instant::now() + wait;
    loop {
        match probe().await? {
            Probe::Ready(t) => return Ok(t),
            Probe::Empty(t) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(t);
                }
                match tokio::time::timeout(left, rx.changed()).await {
                    Ok(Ok(())) => {}
                    _ => return Ok(t),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    #[test_id(tmu5usr)]
    #[test]
    fn a_wait_over_thirty_seconds_is_held_to_thirty() {
        assert_eq!(clamp_wait(600), Duration::from_secs(30));
        assert_eq!(clamp_wait(7), Duration::from_secs(7));
        assert_eq!(clamp_wait(0), Duration::ZERO);
    }

    #[test_id(t40axu1)]
    #[tokio::test(start_paused = true)]
    async fn a_poll_wakes_when_published_and_not_for_a_stale_seq() {
        let n = Notifier::new(5);
        let looks = std::sync::atomic::AtomicUsize::new(0);
        let n2 = n.clone();
        let publisher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            n2.publish(3); // stale: no wake
            tokio::time::sleep(Duration::from_secs(1)).await;
            n2.publish(6);
        });
        let started = Instant::now();
        let got: Result<i64, ()> = long_poll(&n, Duration::from_secs(30), || async {
            let k = looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if k == 0 {
                Probe::Empty(0)
            } else {
                Probe::Ready(6)
            })
        })
        .await;
        publisher.await.unwrap();
        assert_eq!(got, Ok(6));
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test_id(t3is95j)]
    #[tokio::test(start_paused = true)]
    async fn an_empty_poll_answers_empty_when_the_wait_ends() {
        let n = Notifier::new(0);
        let started = Instant::now();
        let got: Result<i64, ()> =
            long_poll(&n, Duration::from_secs(4), || async { Ok(Probe::Empty(9)) }).await;
        assert_eq!(got, Ok(9));
        assert_eq!(started.elapsed(), Duration::from_secs(4));
    }
}
