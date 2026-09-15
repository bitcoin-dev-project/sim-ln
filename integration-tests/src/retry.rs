//! Retry with capped exponential backoff, used to make real-node startup error resistant: nodes
//! and their RPCs come up at unpredictable times, so every setup step polls rather than assuming
//! readiness.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

use anyhow::anyhow;
use tokio::time::Instant;

/// Backoff schedule for [`with_backoff`]. Delays double from `initial` up to `max`, and the
/// operation as a whole fails once `timeout` has elapsed.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub timeout: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(5),
            timeout: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// A schedule for operations that are expected to take a while to converge, such as waiting
    /// for gossip to propagate through a network.
    pub fn slow() -> Self {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(10),
            timeout: Duration::from_secs(300),
        }
    }
}

/// Runs `op` until it succeeds or `backoff.timeout` elapses, sleeping between attempts. The
/// returned error names the operation and includes the last underlying error, so failures point
/// at the step (and node) that never became ready.
pub async fn with_backoff<T, E, F, Fut>(
    description: &str,
    backoff: Backoff,
    mut op: F,
) -> Result<T, anyhow::Error>
where
    E: Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let start = Instant::now();
    let mut delay = backoff.initial;

    loop {
        match op().await {
            Ok(t) => return Ok(t),
            Err(e) => {
                if start.elapsed() + delay > backoff.timeout {
                    return Err(anyhow!(
                        "{description}: not ready after {:?}, last error: {e}",
                        start.elapsed()
                    ));
                }

                log::debug!("{description}: retrying in {delay:?} after error: {e}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(backoff.max);
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_succeeds_after_failures() {
        let attempts = AtomicU32::new(0);
        let result = with_backoff("test op", Backoff::default(), || async {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                Err("not yet")
            } else {
                Ok(42)
            }
        })
        .await
        .unwrap();

        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_times_out_with_context() {
        let backoff = Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            timeout: Duration::from_millis(20),
        };
        let err = with_backoff("flaky node startup", backoff, || async {
            Err::<(), _>("connection refused")
        })
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("flaky node startup"));
        assert!(msg.contains("connection refused"));
    }
}
