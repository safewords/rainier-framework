//! # rainier-drivers
//!
//! **Every external service Rainier talks to is interfaced here, and nowhere
//! else.** Redis, Memcached, S3, SQS, DynamoDB — the protocol knowledge, the
//! service operations, the limits, the quirks and the error mapping all live in
//! this crate. The crates that define ports own thin adapters onto it.
//!
//! ## The paradigm
//!
//! ```text
//!   rainier-cache          rainier-queue         rainier-filesystem
//!   (and rainier-database, over rainier-orm's Executor port)
//!   ┌────────────────┐     ┌───────────────┐     ┌──────────────────┐
//!   │ Cache (port)   │     │ Queue (port)  │     │ Filesystem (port)│
//!   │  RedisCache    │     │  SqsQueue     │     │  S3Filesystem    │
//!   │  DynamoDbCache │     │  DatabaseQueue│     │  LocalFilesystem │
//!   └───────┬────────┘     └──────┬────────┘     └────────┬─────────┘
//!           │      adapters — they translate, only        │
//!           └──────────────┬──────────────────────────────┘
//!                          ▼
//!                 ┌──────────────────┐
//!                 │ rainier-drivers  │  service clients, no port traits
//!                 │  RedisClient     │
//!                 │  MemcachedClient │
//!                 │  S3Client        │
//!                 │  SqsClient       │
//!                 │  DynamoDbClient  │
//!                 │  SeaOrmExecutor  │
//!                 │  D1Executor      │
//!                 └──────────────────┘
//! ```
//!
//! Two rules, and they are what make the split hold:
//!
//! **1. A driver never names a port trait.** `SqsClient` has never heard of
//! `Queue`; `RedisClient` has never heard of `Cache`. They expose the
//! *service's* vocabulary — `send`, `receive`, `change_visibility`, `set_nx`,
//! `incr_by` — and nothing about what a caller intends to build from it.
//!
//! That is not stylistic. If a driver implemented `Queue`, this crate would
//! depend on `rainier-queue`, which depends on this crate: a cycle. Keeping the
//! trait out is what lets **one** crate hold every connector while each port
//! keeps its own adapter.
//!
//! **2. An adapter holds no protocol knowledge.** `SqsQueue` in `rainier-queue`
//! decides what an attempt means and where a receipt handle is stashed on a job;
//! it does not know SQS's attribute names, its fifteen-minute delay cap, or that
//! changing a visibility timeout is how a message gets released. Those are here.
//!
//! ## What this buys
//!
//! **One place to look.** "How does Rainier talk to Redis" has one answer, and it
//! is not spread across the cache and the queue.
//!
//! **One client, shared.** Redis is wanted by the cache *and* the queue. Without
//! a shared home each would carry its own client, its own copy of the protocol
//! code, and its own URL parsing — and an application would configure Redis twice
//! and open two sets of connections to one server. The same goes for AWS
//! credentials, which `AwsConnector` resolves once for S3, SQS and DynamoDB
//! together.
//!
//! **Testable in the right halves.** A driver test asks *does this speak the
//! protocol correctly* — the Memcached client is tested against a
//! stub server, including a value containing `CRLF` that a line-based reader
//! would truncate. An adapter test asks *does this translate correctly*, and
//! needs no network at all.
//!
//! ## Where the line falls, concretely
//!
//! | Belongs here | Belongs in the port's crate |
//! |---|---|
//! | opening a client from a URL | deciding what a key means |
//! | `SET key value PX 60000` | choosing the TTL |
//! | SQS's fifteen-minute delay cap | what to do when a job exceeds it |
//! | DynamoDB's TTL lagging 48 hours | filtering an expired read as a miss |
//! | S3 reporting absence two different ways | treating absence as `Ok(None)` |
//! | turning a driver error into an [`Error`](rainier_support::Error) | retry policy |
//!
//! The rule that settles an argument: **this crate must not know what is being
//! stored, only how to store it.**
//!
//! ## Errors are `503`, not `500`
//!
//! Every driver failure is
//! [`ServiceUnavailable`](rainier_support::ErrorKind::ServiceUnavailable). A cache
//! or queue being unreachable is a **dependency outage**: retryable, somebody's to
//! page about, and not a bug in the request that happened to hit it. A `500` puts
//! it in the wrong bucket on every dashboard you have.
//!
//! Driver messages are also not passed through verbatim — a Redis or Memcached
//! error frequently contains the connection string, and therefore the password.
//!
//! ## Official SDKs where they exist
//!
//! The AWS services use the **official AWS SDK** rather than signed requests of
//! our own. An earlier version of this crate hand-rolled SigV4; it worked, and it
//! could only read static credentials from environment variables. Real workloads
//! use an EC2 instance role, an ECS task role, EKS IRSA, SSO or a profile — each
//! needing a provider that discovers, caches and **refreshes** a temporary
//! credential before it expires. Reimplementing that is reimplementing the
//! interesting half.
//!
//! The one client written here rather than taken from a crate is
//! Memcached, for a stated reason: the obvious candidate does not
//! compile on Windows, and six commands of a text protocol is cheaper to carry
//! than a portability problem.
//!
//! ## Features
//!
//! | Feature | Gives you |
//! |---|---|
//! | `redis-driver` | `RedisConnector` and `RedisClient` against one server |
//! | `redis-cluster` | …and against a sharded cluster |
//! | `memcached` | `MemcachedConnector` |
//! | `kafka` | `KafkaConnector` and `KafkaClient` |
//! | `kafka-tls` | …and TLS, for a managed cluster |
//! | `aws` | `AwsConnector` — one AWS configuration for every service |
//! | `aws-s3` | `S3Client` |
//! | `aws-sqs` | `SqsClient` |
//! | `aws-dynamodb` | `DynamoDbClient` |
//! | `sea-orm-executor` | native MySQL / Postgres / SQLite for [`rainier-orm`] |
//! | `d1-http` | Cloudflare D1 |
//! | `libsql-http` | libSQL / Turso |
//!
//! [`rainier-orm`]: https://docs.rs/rainier-orm
//!
//! All off by default, so an application compiles only the clients it uses.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

#[cfg(feature = "aws")]
pub mod aws;
#[cfg(feature = "aws-dynamodb")]
pub mod dynamodb;
#[cfg(feature = "kafka")]
pub mod kafka;
#[cfg(feature = "memcached")]
pub mod memcached;
#[cfg(feature = "redis-driver")]
pub mod redis;
#[cfg(feature = "redis-driver")]
pub mod redis_streams;
#[cfg(feature = "aws-s3")]
pub mod s3;
#[cfg(feature = "sql")]
pub mod sql;
#[cfg(feature = "aws-sqs")]
pub mod sqs;

#[cfg(feature = "aws")]
pub use aws::{sdk_error, AwsConnector};
#[cfg(feature = "aws-dynamodb")]
pub use dynamodb::{DynamoDbClient, DynamoItem};
#[cfg(feature = "kafka")]
pub use kafka::{
    partition_for_key, KafkaClient, KafkaConnector, KafkaCredentials, KafkaFetch, KafkaMessage,
    KafkaOffset, KafkaPosition, KafkaRecord, SaslMechanism,
};
#[cfg(feature = "memcached")]
pub use memcached::{
    check_key, expiry_seconds, MemcachedConnection, MemcachedConnector, MemcachedGuard, Stored,
    MAX_RELATIVE_TTL,
};
#[cfg(feature = "redis-driver")]
pub use redis::{Reconnect, RedisClient, RedisConnection, RedisConnector, RedisSettings};
#[cfg(feature = "redis-driver")]
pub use redis_streams::StreamEntry;
#[cfg(feature = "aws-s3")]
pub use s3::{S3Client, S3Head, S3Object};
#[cfg(feature = "aws-sqs")]
pub use sqs::{SqsClient, SqsMessage};

/// The Redis client, re-exported so every consumer resolves one version and can
/// build commands this crate does not wrap.
#[cfg(feature = "redis-driver")]
pub use ::redis as redis_client;

#[cfg(all(
    test,
    not(any(feature = "redis-driver", feature = "memcached", feature = "aws", feature = "kafka"))
))]
mod tests {
    /// With no driver enabled the crate is empty, which is the point of the
    /// feature flags — this asserts it still compiles that way.
    #[test]
    fn the_crate_compiles_with_no_drivers() {}
}
