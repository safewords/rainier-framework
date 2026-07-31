//! Where queued jobs wait — [`QueueDriver`].

use rainier_support::setting_enum;

setting_enum! {
    /// Which [`Queue`](crate::Queue) to build.
    ///
    /// ```
    /// use rainier_queue::QueueDriver;
    /// use rainier_support::Setting;
    ///
    /// assert!(!QueueDriver::Sync.is_deferred(), "sync runs the job inline");
    /// assert!(QueueDriver::parse("database").unwrap().survives_a_restart());
    /// ```
    pub enum QueueDriver: "queue driver" {
        /// Run the job inline, on the thread that dispatched it.
        ///
        /// The default because it needs no worker: a fresh clone dispatches a
        /// job and the job happens. Also the reason a slow job makes a slow
        /// request — which is the thing a queue exists to avoid, so this is a
        /// development setting.
        #[default]
        Sync = "sync",

        /// A queue in this process's memory.
        ///
        /// For tests that want a job to be *queued* rather than *run*, so they
        /// can assert on it. Lost on restart, invisible to other processes.
        Memory = "memory",

        /// Two tables in the application's own database.
        ///
        /// Durable, shared, and needs no new infrastructure — the reason it is
        /// the usual production answer. Needs the migrations
        /// (`DatabaseQueue::migrations()`) and a `queue:work` process.
        Database = "database",

        /// Redis streams.
        ///
        /// Durable-ish, and the qualifier is the point: Redis acknowledges a
        /// write before it is on disk and before a replica has it, so a job
        /// can be lost to a crash or a failover. Right for work you can afford
        /// to lose, wrong for work you cannot — see the
        /// [driver's docs](crate::redis). Needs the `redis` feature.
        Redis = "redis",

        /// Amazon SQS.
        ///
        /// Durable and managed, with the visibility timeout doing the
        /// reservation. Needs the `sqs` feature.
        Sqs = "sqs",

        /// A Kafka topic.
        ///
        /// **Read [the driver's docs](crate::kafka) before choosing it.** A log
        /// is not a queue: concurrency is the partition count rather than the
        /// worker count, a delayed job blocks its partition, and a retry goes
        /// to the end of the topic instead of back where it was. Right when the
        /// jobs are already events on a topic; wrong as a general job queue.
        /// Needs the `kafka` feature.
        Kafka = "kafka",
    }
}

impl QueueDriver {
    /// Whether dispatching returns before the job has run.
    ///
    /// `false` for [`Sync`](Self::Sync), which is the whole difference between
    /// it and the others — and the reason a test that asserts on a side effect
    /// passes under `sync` and hangs under everything else.
    pub fn is_deferred(&self) -> bool {
        !matches!(self, Self::Sync)
    }

    /// Whether a job dispatched now survives **this** process exiting.
    ///
    /// True for anything that stores the job somewhere else. It is not the
    /// same question as whether the job is safe — see
    /// [`may_lose_an_accepted_job`](Self::may_lose_an_accepted_job), which is
    /// the one that catches Redis.
    pub fn survives_a_restart(&self) -> bool {
        matches!(self, Self::Database | Self::Redis | Self::Sqs | Self::Kafka)
    }

    /// Whether a dispatch this driver **accepted** can still be lost.
    ///
    /// True only for [`Redis`](Self::Redis), and it is the distinction the
    /// obvious question misses: a Redis job survives your process restarting,
    /// because it lives in another one — and can still vanish, because Redis
    /// acknowledges a write before it is on disk (`appendfsync everysec`
    /// leaves a window of about a second) and before any replica has it (
    /// replication is asynchronous, so a failover can drop confirmed writes).
    ///
    /// A database queue's insert is committed before it returns; SQS is
    /// replicated across availability zones before it acknowledges. Neither
    /// can tell you a job is queued and then lose it.
    ///
    /// Kafka is produced to with `acks=all`, so a push returns once every
    /// in-sync replica holds the record — which is the same guarantee, *and is
    /// only as good as the replication*. A single-broker cluster has one
    /// in-sync replica and Kafka does not fsync each write, so a development
    /// cluster can lose an accepted job even though this reports it cannot.
    /// `replication.factor` and `min.insync.replicas` of at least two are what
    /// make the answer true.
    ///
    /// So this is the predicate a deploy check should look at when the work is
    /// a payment rather than a cache warm.
    pub fn may_lose_an_accepted_job(&self) -> bool {
        matches!(self, Self::Redis)
    }

    /// Whether this driver needs a `queue:work` process to make progress.
    pub fn needs_a_worker(&self) -> bool {
        self.is_deferred()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn sync_is_the_only_driver_that_runs_inline() {
        assert!(!QueueDriver::Sync.is_deferred());
        assert!(!QueueDriver::Sync.needs_a_worker());

        for driver in QueueDriver::ALL.iter().filter(|d| **d != QueueDriver::Sync) {
            assert!(driver.is_deferred(), "{driver} should defer");
            assert!(driver.needs_a_worker(), "{driver} should need a worker");
        }
    }

    #[test]
    fn only_the_backed_drivers_survive_a_restart() {
        assert!(QueueDriver::Database.survives_a_restart());
        assert!(QueueDriver::Sqs.survives_a_restart());
        assert!(QueueDriver::Kafka.survives_a_restart());
        assert!(QueueDriver::Redis.survives_a_restart(), "it lives in another process");
        assert!(!QueueDriver::Memory.survives_a_restart());
    }

    #[test]
    fn only_redis_can_lose_a_job_it_already_accepted() {
        // The distinction `survives_a_restart` cannot make: the job outlives
        // your process *and* can still be gone, because Redis acknowledges
        // before the write is durable or replicated.
        assert!(QueueDriver::Redis.may_lose_an_accepted_job());

        assert!(!QueueDriver::Database.may_lose_an_accepted_job());
        assert!(!QueueDriver::Sqs.may_lose_an_accepted_job());
        // `acks=all`, so the push returns once the replicas have it — which is
        // a claim about a replicated cluster, as the doc comment says.
        assert!(!QueueDriver::Kafka.may_lose_an_accepted_job());

        // Memory loses everything, but `survives_a_restart` already says so —
        // this predicate is for the ones that look safe.
        assert!(!QueueDriver::Memory.may_lose_an_accepted_job());

        // Sync has already run the job by the time the process exits, which is
        // not the same as surviving — there was nothing left to survive.
        assert!(!QueueDriver::Sync.survives_a_restart());
    }
}
