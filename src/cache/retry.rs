// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Retrying what a storage service says is temporary.

use opendal::layers::RetryLayer;
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
pub(crate) fn retry_layer() -> RetryLayer {
    RetryLayer::new()
        .with_min_delay(Duration::from_millis(200))
        .with_max_delay(Duration::from_secs(2))
        .with_factor(2.0)
        .with_max_times(3)
        .with_jitter()
}
