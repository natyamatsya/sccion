// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Retrying what a storage service says is temporary.

use opendal::layers::RetryLayer;
use std::future::Future;
use std::time::Duration;

/// The retry layer every opendal backend is built with.
///
/// Without one, a temporary failure was final: a lookup counted as an error
/// and a write was lost, so the next run missed. The case that made this
/// visible is the GitHub Actions cache, which answers a repository's
/// concurrent jobs with HTTP 429 (`rate limit exceeded`) and lost up to two
/// thirds of a CI run's writes that way. opendal retries only the errors it
/// marks temporary, so a missing entry is still answered at once.
///
/// Short, because a lookup holds its compilation up: three retries from
/// 200 ms, doubling, each with up to a further 200 ms of jitter so that
/// concurrent jobs do not retry in step -- about two seconds at most, against
/// the 60 s a lookup is allowed before sccion compiles anyway.
///
/// It goes outside the logging layer, so each failed attempt is logged.
/// Writes get more on top, from [`write_with_backoff`].
pub(crate) fn retry_layer() -> RetryLayer {
    RetryLayer::new()
        .with_min_delay(Duration::from_millis(200))
        .with_max_delay(Duration::from_secs(2))
        .with_factor(2.0)
        .with_max_times(3)
        .with_jitter()
}

/// How long a write waits before each further attempt, once the retry layer
/// has given up on one: about half a minute in all.
const WRITE_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// Runs `write` until it succeeds, fails for good, or the backoff runs out.
///
/// The retry layer's two seconds are sized for lookups, which hold their
/// compilation up. They are not enough for the GitHub Actions cache under
/// load: its rate limit stayed exhausted through every one of them, and the
/// write was lost. A write holds nothing up -- the client has its result
/// before the write starts -- and a lost one costs the next run a hit, so a
/// write gets to wait much longer. An error the retry layer gave up on comes
/// back `persistent` rather than `temporary`; both are worth another round.
///
/// The server waits at most ten seconds for writes still running when it
/// shuts down, so a write that is still backing off then is lost as before.
pub(crate) async fn write_with_backoff<T, F, Fut>(mut write: F) -> opendal::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = opendal::Result<T>>,
{
    let mut delays = WRITE_BACKOFF.iter();
    loop {
        match write().await {
            Err(e) if e.is_temporary() || e.is_persistent() => match delays.next() {
                Some(&delay) => {
                    // Up to half the delay again, so concurrent jobs spread out.
                    let delay = delay.mul_f64(1.0 + rand::random::<f64>() / 2.0);
                    warn!("cache write failed ({e}), trying again in {delay:.1?}");
                    tokio::time::sleep(delay).await;
                }
                None => return Err(e),
            },
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::write_with_backoff;
    use opendal::{Error, ErrorKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_write_is_tried_again_until_it_succeeds() {
        let attempts = AtomicUsize::new(0);
        let result = write_with_backoff(|| async {
            if attempts.fetch_add(1, Ordering::SeqCst) < 3 {
                Err(Error::new(ErrorKind::RateLimited, "429").set_temporary())
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_gives_up_once_the_backoff_runs_out() {
        let attempts = AtomicUsize::new(0);
        let result: opendal::Result<()> = write_with_backoff(|| async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(Error::new(ErrorKind::RateLimited, "429").set_persistent())
        })
        .await;
        assert_eq!(result.unwrap_err().kind(), ErrorKind::RateLimited);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1 + super::WRITE_BACKOFF.len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanent_error_is_not_tried_again() {
        let attempts = AtomicUsize::new(0);
        let result: opendal::Result<()> = write_with_backoff(|| async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(Error::new(ErrorKind::PermissionDenied, "403"))
        })
        .await;
        assert_eq!(result.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
