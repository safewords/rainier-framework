//! Reading your own writes — [`with_sticky_scope`].
//!
//! Splitting reads onto a replica buys throughput and introduces exactly one
//! failure, which is the quiet kind: a read issued straight after a write can
//! land on a replica that has not caught up, and **answer**. Not an error, not
//! a retry — the row is simply not there yet, or still holds its previous
//! value. The record that was just created 404s. The balance that was just
//! debited reads its old total. The row that was just updated renders as it
//! was a moment ago, and the next write is computed from it.
//!
//! Nothing in that sequence raises, so nothing in that sequence is logged, and
//! the report of it arrives as "it saved but it did not save" from somebody who
//! could not reproduce it — because on the second attempt the replica had
//! caught up.
//!
//! `sticky` is the setting that closes it, and what it needs is a **scope**: a
//! unit of work inside which *this scope has already written* is a fact worth
//! remembering. Inside one, a write pins the connection, and every read after
//! it goes to the endpoint the write went to — where the row is certainly
//! there, because that is the endpoint that put it there.
//!
//! ## Why this is a task local and not a field on the connection
//!
//! A [`Database`](crate::Database) is a container singleton. Every request
//! handler, job and console command resolves the *same* handle, so a flag
//! stored on it would be shared by every one of them at once, and a
//! process-global sticky flag is worse than no stickiness at all in both
//! directions:
//!
//! - It never ends. The first write the process makes pins every read for the
//!   rest of its life, and the replicas that were declared to take the read
//!   traffic quietly take none of it.
//! - Or it is cleared by whoever finishes first — which is not whoever set it.
//!   One request's write pins another's reads, and one request's completion
//!   *unpins* a third request that has written and is about to read. That
//!   third request then reads the replica and is handed the stale row this
//!   whole module exists to prevent, intermittently, under load, and never in
//!   a test.
//!
//! A task local is neither. It belongs to one future, follows it across every
//! `.await` and across whichever worker thread tokio resumes it on — which a
//! thread local does not — and ends when that future ends. It is the same
//! mechanism, and for the same reason, that `rainier-container` uses to scope
//! an application to a task.
//!
//! ## What a scope covers, and where it stops
//!
//! Exactly the future handed to [`with_sticky_scope`], and everything that
//! future awaits.
//!
//! It stops at `tokio::spawn`. A spawned task is a new task with no scope, and
//! that is the right default rather than an oversight: the work being spawned
//! is usually a *different* unit of work, and inheriting the parent's pin would
//! send its reads to the primary for reasons that have nothing to do with it.
//! Give it its own scope by wrapping its future in [`with_sticky_scope`] the
//! same way.
//!
//! Scopes nest. An inner one is a fresh scope rather than a view of the outer,
//! so a pin taken inside it does not survive it and does not leak outward.
//!
//! ## Outside a scope, a sticky connection reads from the writer
//!
//! This is the part to know before declaring `sticky`, because it is the one
//! that surprises people.
//!
//! There are only two things a sticky connection can do for a read that no
//! scope is tracking. It can send it to a replica, which is fast and may be
//! stale — and staleness is precisely the failure `sticky` was declared to
//! rule out. Or it can send it to the writer, which is never stale and does
//! not use the replicas.
//!
//! It sends it to the writer. A read split that is not being used shows up as
//! load on the primary and idle replicas: visible, measurable, and fixable.
//! The other answer shows up as a row that is not there, and it does not show
//! up at all until somebody reports it. A connection is free to declare the
//! split **without** `sticky`, which is a deliberate statement that its reads
//! tolerate lag; what is not on offer is the promise plus the staleness.
//!
//! The first read a sticky connection serves outside a scope logs a warning
//! naming this, once per connection per process, so an idle replica is a line
//! in a log rather than a mystery in a dashboard.
//!
//! ## Nothing enters a scope on your behalf yet
//!
//! [`with_sticky_scope`] is called by the caller. The framework does not wrap a
//! request or a job in one, which means a `sticky` connection in an application
//! today reads from its writer everywhere. Wiring it in is one call at each
//! place the framework already owns a unit of work — the HTTP kernel around
//! serving one request, and the queue worker around running one job — and it
//! belongs in those crates rather than being guessed at from here.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

tokio::task_local! {
    /// The sticky scope this task is running inside, if any.
    static SCOPE: Arc<Scope>;
}

/// Run `future` as one sticky unit of work.
///
/// Inside it, a write through a connection declared `sticky` pins that
/// connection: every read after it, anywhere in this future, goes to the
/// endpoint the write went to rather than to a replica that may not have the
/// row yet.
///
/// The scope ends when the future does. Nothing outside it is affected, which
/// is the property that makes this safe to call on a request path where
/// hundreds of these are in flight at once.
///
/// ```
/// # use rainier_database::{in_sticky_scope, with_sticky_scope};
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// assert!(!in_sticky_scope());
///
/// with_sticky_scope(async {
///     assert!(in_sticky_scope());
/// })
/// .await;
///
/// assert!(!in_sticky_scope());
/// # }
/// ```
pub async fn with_sticky_scope<F>(future: F) -> F::Output
where
    F: Future,
{
    SCOPE.scope(Arc::new(Scope::default()), future).await
}

/// Whether this task is running inside a [`with_sticky_scope`].
///
/// Worth asserting in a test that cares which endpoint a query reached, and
/// worth checking at a boundary that is meant to have entered one.
pub fn in_sticky_scope() -> bool {
    SCOPE.try_with(|_| ()).is_ok()
}

/// What a scope remembers about one connection.
///
/// A `Vec` rather than a map because the length is the number of connections an
/// application declared — one, usually — and a linear scan over that beats
/// hashing a key to find it.
#[derive(Default)]
struct Scope {
    marks: Mutex<Vec<Mark>>,
}

/// One connection's pins inside one scope.
#[derive(Clone, Copy)]
struct Mark {
    /// Which connection this is about — see [`next_connection_id`].
    connection: usize,
    /// The write endpoint this scope settled on, once it has written.
    write: Option<usize>,
    /// Where reads in this scope go.
    read: ReadPin,
}

/// Where a scope's reads go, once it has decided.
#[derive(Clone, Copy)]
enum ReadPin {
    /// Nothing has read or written yet.
    Unpinned,
    /// This scope has written, so its reads go where the write went.
    Writer,
    /// This scope has read, and keeps reading from the same replica.
    Replica(usize),
}

/// Where one read should go.
pub(crate) enum Read {
    /// To the endpoint this scope wrote to.
    Writer,
    /// To this replica, which is the one this scope has been reading from.
    Replica(usize),
}

impl Scope {
    /// The lock, with a poisoned one taken over rather than propagated.
    ///
    /// A panic somewhere else in this scope says nothing about whether these
    /// pins are still true, and refusing to route a query because of it would
    /// turn an unrelated panic into a failed request.
    fn marks(&self) -> MutexGuard<'_, Vec<Mark>> {
        self.marks.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Where this scope's write to `connection` goes, choosing once if it has not
/// written before.
///
/// `choose` is only called the first time, which is what makes the choice
/// stick: a scope that writes twice writes to the same endpoint both times.
/// Against a topology with more than one primary that is not a nicety — two
/// writes from one unit of work landing on two primaries is a conflict for the
/// cluster to resolve later, out of order, invisibly.
///
/// `None` when there is no scope, in which case the caller chooses for itself.
pub(crate) fn write_endpoint(connection: usize, choose: impl FnOnce() -> usize) -> Option<usize> {
    SCOPE
        .try_with(|scope| {
            let mut marks = scope.marks();
            let at = mark(&mut marks, connection);

            let chosen = match marks[at].write {
                Some(chosen) => chosen,
                None => {
                    let chosen = choose();
                    marks[at].write = Some(chosen);
                    chosen
                }
            };

            // The whole point: from here on, a read in this scope is a read of
            // something this scope may have just written, so it goes where the
            // write went. Set on every write and not only the first, because a
            // read between two writes moves the pin back to a replica for
            // nobody's benefit.
            marks[at].read = ReadPin::Writer;
            chosen
        })
        .ok()
}

/// Where this scope's read of `connection` goes, choosing a replica once if it
/// has neither read nor written before.
///
/// Holding the choice is a second guarantee beside reading your own writes:
/// two replicas are two points in the replication stream, so a scope that read
/// one row from replica A and the next from replica B can see the second row as
/// it was *before* the first — a record that exists and then does not, inside
/// one request. Staying on one replica makes a scope's view of the world move
/// in one direction.
///
/// `None` when there is no scope.
pub(crate) fn read_endpoint(connection: usize, choose: impl FnOnce() -> usize) -> Option<Read> {
    SCOPE
        .try_with(|scope| {
            let mut marks = scope.marks();
            let at = mark(&mut marks, connection);

            match marks[at].read {
                ReadPin::Writer => Read::Writer,
                ReadPin::Replica(replica) => Read::Replica(replica),
                ReadPin::Unpinned => {
                    let replica = choose();
                    marks[at].read = ReadPin::Replica(replica);
                    Read::Replica(replica)
                }
            }
        })
        .ok()
}

/// This connection's entry in the scope, created if this is its first query.
fn mark(marks: &mut Vec<Mark>, connection: usize) -> usize {
    if let Some(at) = marks.iter().position(|mark| mark.connection == connection) {
        return at;
    }
    marks.push(Mark { connection, write: None, read: ReadPin::Unpinned });
    marks.len() - 1
}

/// A fresh identity for a connection that can be pinned.
///
/// Per connection rather than per scope, because two declared connections are
/// two databases: writing to the primary of one says nothing about whether the
/// other's replica is caught up, and pinning both would send a reporting
/// warehouse's reads to a primary that has nothing to do with it.
///
/// Handed out only to a connection that actually splits its reads; everything
/// else never asks.
pub(crate) fn next_connection_id() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn there_is_no_scope_until_one_is_entered() {
        assert!(!in_sticky_scope());
        assert!(write_endpoint(0, || 0).is_none());
        assert!(read_endpoint(0, || 0).is_none());

        with_sticky_scope(async {
            assert!(in_sticky_scope());
        })
        .await;

        assert!(!in_sticky_scope());
    }

    #[tokio::test]
    async fn a_scope_reads_from_one_replica_and_keeps_reading_from_it() {
        with_sticky_scope(async {
            let mut offered = 0;
            let mut choose = || {
                offered += 1;
                offered - 1
            };

            let first = read_endpoint(7, &mut choose).expect("in a scope");
            assert!(matches!(first, Read::Replica(0)));

            // The chooser is not consulted again, so the scope cannot drift
            // onto a replica at a different point in the stream.
            let second = read_endpoint(7, &mut choose).expect("in a scope");
            assert!(matches!(second, Read::Replica(0)));
            assert_eq!(offered, 1);
        })
        .await;
    }

    #[tokio::test]
    async fn a_write_sends_the_reads_after_it_to_the_writer() {
        with_sticky_scope(async {
            assert!(matches!(read_endpoint(7, || 3), Some(Read::Replica(3))));

            assert_eq!(write_endpoint(7, || 1), Some(1));

            // The row the write just made is on the writer and may not be
            // anywhere else yet.
            assert!(matches!(read_endpoint(7, || 3), Some(Read::Writer)));
        })
        .await;
    }

    #[tokio::test]
    async fn a_scope_that_writes_twice_writes_to_the_same_endpoint() {
        with_sticky_scope(async {
            let mut offered = 0;
            let mut choose = || {
                offered += 1;
                offered - 1
            };

            assert_eq!(write_endpoint(7, &mut choose), Some(0));
            assert_eq!(write_endpoint(7, &mut choose), Some(0));
            assert_eq!(offered, 1, "two writes from one scope split across two primaries");
        })
        .await;
    }

    #[tokio::test]
    async fn a_pin_is_per_connection_and_not_per_scope() {
        with_sticky_scope(async {
            write_endpoint(1, || 0);

            // Connection 1 has been written to; connection 2 has not, and its
            // replica is not implicated by a write to a different database.
            assert!(matches!(read_endpoint(1, || 5), Some(Read::Writer)));
            assert!(matches!(read_endpoint(2, || 5), Some(Read::Replica(5))));
        })
        .await;
    }

    #[tokio::test]
    async fn a_nested_scope_is_a_fresh_one() {
        with_sticky_scope(async {
            write_endpoint(1, || 0);
            assert!(matches!(read_endpoint(1, || 5), Some(Read::Writer)));

            with_sticky_scope(async {
                // The inner scope has written nothing.
                assert!(matches!(read_endpoint(1, || 5), Some(Read::Replica(5))));
            })
            .await;

            // …and leaving it did not clear the outer scope's pin.
            assert!(matches!(read_endpoint(1, || 5), Some(Read::Writer)));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_scopes_write_does_not_pin_another_scopes_reads() {
        // The failure a process-global flag has and this does not. Both halves
        // run concurrently on a multi-threaded runtime, so a shared flag would
        // have the writer's pin visible to the reader.
        let writing = tokio::spawn(with_sticky_scope(async {
            write_endpoint(9, || 0);
            tokio::task::yield_now().await;
            matches!(read_endpoint(9, || 4), Some(Read::Writer))
        }));

        let reading = tokio::spawn(with_sticky_scope(async {
            tokio::task::yield_now().await;
            matches!(read_endpoint(9, || 4), Some(Read::Replica(4)))
        }));

        assert!(writing.await.unwrap(), "the writer lost its own pin");
        assert!(reading.await.unwrap(), "a write in another scope pinned these reads");
    }

    #[tokio::test]
    async fn a_spawned_task_starts_outside_the_scope_that_spawned_it() {
        with_sticky_scope(async {
            let spawned = tokio::spawn(async { in_sticky_scope() });
            assert!(!spawned.await.unwrap());
        })
        .await;
    }

    #[test]
    fn every_connection_gets_its_own_identity() {
        assert_ne!(next_connection_id(), next_connection_id());
    }
}
