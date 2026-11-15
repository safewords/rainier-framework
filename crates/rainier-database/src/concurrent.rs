//! Running independent queries at the same time — [`concurrently!`] and
//! [`all`].
//!
//! Every query a [`Database`](crate::Database) runs takes a connection from
//! the pool for as long as it runs, and gives it back. So two queries that do
//! not need each other's answers can run at once, each on its own connection,
//! and the wait is the slower of the two rather than both end to end:
//!
//! ```ignore
//! // One after the other: four round trips.
//! let unread = statuses.count_matching(unread.clone()).await?;
//! let chats = statuses.matching(unread.limit(500)).await?;
//! let mine = profile_notifications.count_matching(mine).await?;
//! let account = user_notifications.count_matching(account).await?;
//!
//! // At once: the slowest of the four.
//! let (unread, chats, mine, account) = concurrently!(
//!     statuses.count_matching(unread.clone()),
//!     statuses.matching(unread.limit(500)),
//!     profile_notifications.count_matching(mine),
//!     user_notifications.count_matching(account),
//! )?;
//! ```
//!
//! # When it helps, and when it does not
//!
//! Only queries that do not depend on one another. A chain — load the post,
//! then its author, then the author's blocks — cannot run at once, because
//! each needs the last one's answer; the cure for a chain is fewer queries (a
//! join, a batched `WHERE id IN (…)`), not concurrency.
//!
//! What it costs is connections: a request running four queries at once holds
//! four for that moment, where it held one four times over. [`concurrently!`]
//! is for a handful written out by hand. For a list of any length use [`all`],
//! which runs at most [`DEFAULT_LIMIT`] at a time, so one request cannot take
//! the whole pool from every other request.
//!
//! # Errors
//!
//! The first error is returned, and the queries still running are dropped —
//! cancelled where the driver can, their connections returned to the pool.
//! Nothing that already ran is undone: these are independent statements, not
//! a transaction, so do not run writes this way that must succeed or fail
//! together.
//!
//! # What Lookout shows
//!
//! Each query is still its own span under the request, so the waterfall draws
//! them overlapping — which is how to see that it worked.

use std::future::Future;

use futures_util::{StreamExt as _, TryStreamExt as _};

/// How many of [`all`]'s futures run at once.
///
/// A quarter of the pool's default ten connections and then some: enough
/// that a fan-out is fast, few enough that several requests doing it at the
/// same moment still leave connections for everyone else.
pub const DEFAULT_LIMIT: usize = 4;

/// Run independent queries at the same time and wait for all of them.
///
/// Takes futures that return `Result`s with the same error type — every query
/// in this framework does — and returns `Result<(A, B, …)>`: their values in
/// the order written, or the first error. Up to a dozen or so; for a list,
/// use [`all`].
///
/// ```
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// use rainier_database::concurrently;
/// use rainier_support::Result;
///
/// async fn count() -> Result<u64> { Ok(3) }
/// async fn name() -> Result<String> { Ok("ada".into()) }
///
/// let (n, who) = concurrently!(count(), name())?;
/// assert_eq!((n, who.as_str()), (3, "ada"));
/// # Ok::<(), rainier_support::Error>(())
/// # }).unwrap();
/// ```
#[macro_export]
macro_rules! concurrently {
    ($($future:expr),+ $(,)?) => {
        $crate::reexport::try_join!($($future),+)
    };
}

/// Run every future in `futures`, at most [`DEFAULT_LIMIT`] at a time, and
/// return their values in the order given — or the first error.
///
/// For a list of like queries: one per profile, one per id in a page.
///
/// ```
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// use rainier_support::Result;
///
/// async fn double(n: u64) -> Result<u64> { Ok(n * 2) }
///
/// let doubled = rainier_database::concurrent::all((1..=5).map(double)).await?;
/// assert_eq!(doubled, vec![2, 4, 6, 8, 10]);
/// # Ok::<(), rainier_support::Error>(())
/// # }).unwrap();
/// ```
pub async fn all<T, E, F>(futures: impl IntoIterator<Item = F>) -> Result<Vec<T>, E>
where
    F: Future<Output = Result<T, E>>,
{
    all_limited(DEFAULT_LIMIT, futures).await
}

/// [`all`], with at most `limit` running at once. A `limit` of zero is one.
pub async fn all_limited<T, E, F>(
    limit: usize,
    futures: impl IntoIterator<Item = F>,
) -> Result<Vec<T>, E>
where
    F: Future<Output = Result<T, E>>,
{
    futures_util::stream::iter(futures).buffered(limit.max(1)).try_collect().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rainier_support::{Error, Result};

    use super::*;

    async fn after(ms: u64, value: u64) -> Result<u64> {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(value)
    }

    #[tokio::test]
    async fn concurrently_waits_for_the_slowest_not_the_sum() {
        let started = Instant::now();
        let (a, b, c) = concurrently!(after(60, 1), after(60, 2), after(60, 3)).unwrap();
        let took = started.elapsed();

        assert_eq!((a, b, c), (1, 2, 3), "in the order written");
        assert!(took < Duration::from_millis(150), "ran at once: {took:?}");
    }

    #[tokio::test]
    async fn concurrently_returns_the_first_error() {
        async fn boom() -> Result<u64> {
            Err(Error::internal("boom"))
        }
        let result = concurrently!(after(200, 1), boom());

        assert_eq!(result.unwrap_err().message(), "boom");
    }

    #[tokio::test]
    async fn all_keeps_the_order_given_however_they_finish() {
        // The first is the slowest; it is still first.
        let got = all([after(50, 1), after(10, 2), after(30, 3)]).await.unwrap();
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn all_runs_no_more_than_its_limit_at_once() {
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let tasks = (0..12).map(|i| {
            let (running, most) = (Arc::clone(&running), Arc::clone(&most));
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, Error>(i)
            }
        });

        let got = all_limited(3, tasks).await.unwrap();

        assert_eq!(got, (0..12).collect::<Vec<_>>());
        assert_eq!(most.load(Ordering::SeqCst), 3, "never more than the limit");
    }

    #[tokio::test]
    async fn a_limit_of_zero_still_runs_them_one_at_a_time() {
        let got = all_limited(0, [after(1, 7), after(1, 8)]).await.unwrap();
        assert_eq!(got, vec![7, 8]);
    }

    #[tokio::test]
    async fn all_of_nothing_is_nothing() {
        let got: Vec<u64> = all(Vec::<std::future::Ready<Result<u64>>>::new()).await.unwrap();
        assert!(got.is_empty());
    }
}
