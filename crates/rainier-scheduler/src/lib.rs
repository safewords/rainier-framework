//! # rainier-scheduler
//!
//! The task scheduler: one place that declares what runs when, driven by
//! a single system cron entry rather than a `crontab` line per task.
//!
//! ```
//! use rainier_scheduler::Schedule;
//! use std::time::Duration;
//!
//! pub fn schedule(schedule: &mut Schedule) {
//!     schedule
//!         .call("prune-sessions", |_| Box::pin(async { Ok(()) }))
//!         .daily_at("03:00")
//!         .without_overlapping(Duration::from_secs(1800));
//!
//!     schedule
//!         .call("send-digest", |_| Box::pin(async { Ok(()) }))
//!         .weekly_on(1, "09:00")
//!         .on_one_server();
//! }
//! # let mut s = Schedule::new();
//! # schedule(&mut s);
//! # assert_eq!(s.len(), 2);
//! ```
//!
//! ```cron
//! * * * * * cd /srv/app && ./app schedule:run >> /dev/null 2>&1
//! ```
//!
//! One entry. Everything else is in the code, in version control, and
//! deployable without touching a server's crontab.
//!
//! ## The two guarantees, and what they are made of
//!
//! | | Prevents | Lock held for |
//! |---|---|---|
//! | [`without_overlapping`] | a run starting before the last finished | the whole run |
//! | [`on_one_server`] | three machines running the same occurrence | that minute |
//!
//! Both are [atomic locks](rainier_cache::Lock) over the cache, and both are
//! only as shared as the cache is. Over a `MemoryCache`, `on_one_server` is
//! three machines each holding their own lock and each concluding they are the
//! one — which is precisely the situation it exists to prevent, so
//! [`LockManager::is_shared`](rainier_cache::LockManager::is_shared) is worth
//! asserting at boot.
//!
//! See [the lock module](rainier_cache::lock) for what makes them locks and
//! what they do not promise.
//!
//! [`without_overlapping`]: ScheduledTask::without_overlapping
//! [`on_one_server`]: ScheduledTask::on_one_server
//!
//! ## Minute resolution
//!
//! Expressions have five fields, not six. The scheduler is driven by something
//! that wakes once a minute, so a seconds field would be a promise it cannot
//! keep — and the failure mode of accepting one is a task that reads as "every
//! ten seconds" and runs once a minute.
//!
//! ## UTC, unless a task says otherwise
//!
//! [`in_timezone`](ScheduledTask::in_timezone) takes a **fixed offset**, not a
//! named zone. Named zones need a tz database and bring daylight saving with
//! them, where "daily at 02:30" runs twice one night a year and not at all on
//! another. That is a decision an application should make deliberately, with
//! `chrono-tz`, rather than inherit from a convenience method.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod cron;
pub mod schedule;
pub mod task;

pub use cron::CronExpression;
pub use schedule::{RunSummary, Schedule};
pub use task::{ClosureTask, Outcome, ScheduledTask, Skipped, Task};
