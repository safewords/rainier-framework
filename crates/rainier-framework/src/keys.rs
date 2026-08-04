//! The configuration keys Rainier itself reads — a typed index of the tree.
//!
//! Some frameworks answer "what can I configure?" with a shipped file to read
//! through. Rainier's answer is this module: every key the framework writes
//! or reads, with the type stored at it.
//!
//! ```
//! use rainier_framework::config::Config;
//! use rainier_framework::keys;
//!
//! let config = Config::new();
//! config.set(keys::APP_NAME, "My App".to_string()).unwrap();
//!
//! assert_eq!(config.get(keys::APP_NAME).as_deref(), Some("My App"));
//! ```
//!
//! Using them is optional — every `Config` method still takes a `&str` — but
//! it is what turns two classes of silent mistake into compile errors:
//!
//! ```compile_fail
//! # use rainier_framework::{config::Config, keys};
//! # let config = Config::new();
//! // The port is a u16, so this does not compile.
//! config.set(keys::SERVER_PORT, "8000").unwrap();
//! ```
//!
//! ```compile_fail
//! # use rainier_framework::{config::Config, keys};
//! # let config = Config::new();
//! // Neither does a driver spelled as a string.
//! config.set(keys::CACHE_DRIVER, "redis").unwrap();
//! ```
//!
//! ## Reading a driver
//!
//! The driver keys hold [settings](rainier_support::Setting) — closed sets with
//! their own parser. Read them with [`Config::setting`], which distinguishes
//! *unset* (use the default) from *set to nonsense* (fail, and list what was
//! expected):
//!
//! ```
//! use rainier_framework::cache::CacheDriver;
//! use rainier_framework::config::Config;
//! use rainier_framework::keys;
//!
//! let config = Config::new();
//! config.set(keys::CACHE_DRIVER, CacheDriver::Redis).unwrap();
//!
//! match config.setting(keys::CACHE_DRIVER).unwrap() {
//!     CacheDriver::Memory => { /* … */ }
//!     CacheDriver::Redis | CacheDriver::RedisCluster => { /* … */ }
//!     CacheDriver::Memcached => { /* … */ }
//!     CacheDriver::DynamoDb => { /* … */ }
//!     // A `_` arm is worth having: a driver added to the framework is a new
//!     // variant, and a new variant stops an exhaustive match compiling.
//!     other => panic!("this application does not build a {other}"),
//! }
//! ```
//!
//! [`Config::setting`]: rainier_config::Config::setting
//!
//! ## Adding your own
//!
//! An application declares its keys the same way, next to the section that
//! writes them:
//!
//! ```
//! use rainier_framework::config::config_keys;
//!
//! config_keys! {
//!     /// How many posts a listing shows.
//!     pub POSTS_PER_PAGE: u64 = "posts.per_page";
//! }
//! ```

use rainier_cache::{CacheDriver, Stores as CacheStores};
use rainier_config::{config_keys, AppEnv};
use rainier_crypt::{CryptScheme, HashDriver};
use rainier_database::Databases;
use rainier_filesystem::Disks;
use rainier_mail::{MailDriver, MailEncryption};
use rainier_queue::{Connections as QueueConnections, QueueDriver};
use rainier_session::SessionDriver;
use rainier_telemetry::LogFormat;

config_keys! {
    // --- app ---------------------------------------------------------------

    /// The application's display name, in page titles and mail.
    pub APP_NAME: String = "app.name";

    /// Which deployment this is.
    ///
    /// Defaults to [`AppEnv::Production`] when `APP_ENV` is unset — the safe
    /// direction to be wrong in.
    pub APP_ENV: AppEnv = "app.env";

    /// Whether a failure may show its internals to the client.
    ///
    /// Separate from [`APP_ENV`] because they drift the moment someone debugs
    /// staging.
    pub APP_DEBUG: bool = "app.debug";

    /// The application's canonical base URL, used to generate absolute links.
    pub APP_URL: String = "app.url";

    /// The directory the application was started from. Set by the framework;
    /// there is no environment variable for it.
    pub APP_BASE_PATH: String = "app.base_path";

    // --- server ------------------------------------------------------------

    /// The interface the HTTP server binds to.
    pub SERVER_HOST: String = "server.host";

    /// The port the HTTP server binds to.
    pub SERVER_PORT: u16 = "server.port";

    /// The largest request body that will be buffered, in bytes.
    pub SERVER_MAX_BODY_BYTES: u64 = "server.max_body_bytes";

    /// How long a handler may take before the request is cancelled and
    /// answered `408`, in seconds. `0` turns it off.
    ///
    /// Off by default, because the right ceiling is a fact about your
    /// application and a wrong one cancels work that was going to succeed.
    /// `30` is a reasonable first answer for an API; a route that legitimately
    /// takes longer should carry its own
    /// [`Timeout`](rainier_middleware::Timeout) rather than raising this for
    /// everything.
    ///
    /// This bounds the **handler**, not the response body — a streaming or
    /// server-sent-events route returns its response immediately and streams
    /// afterwards, so it is unaffected.
    pub SERVER_REQUEST_TIMEOUT_SECS: u64 = "server.request_timeout_secs";

    /// Whether to gzip text responses on the way out.
    ///
    /// Off by default: a deployment behind nginx, a CDN or a load balancer
    /// usually compresses there, and doing it twice is CPU spent to produce
    /// the same bytes. Turn it on when Rainier is the thing clients talk to.
    ///
    /// See [`Compress`](rainier_middleware::Compress) for what it will and
    /// will not compress.
    pub SERVER_COMPRESSION: bool = "server.compression";

    // --- observability ------------------------------------------------------
    //
    // All three are off by default. Each costs something a request pays for —
    // a lock and a timer, a document rendered at boot, a span per request —
    // and an application that has not asked for them should not pay it.

    /// Whether to record Prometheus metrics.
    pub METRICS_ENABLED: bool = "metrics.enabled";

    /// The path the scrape endpoint is served at.
    ///
    /// Configurable because it is the one endpoint you may want somewhere
    /// unguessable, on a deployment where it cannot be put behind auth.
    pub METRICS_PATH: String = "metrics.path";

    /// Whether to serve the OpenAPI document.
    pub OPENAPI_ENABLED: bool = "openapi.enabled";

    /// The path the document is served at.
    pub OPENAPI_PATH: String = "openapi.path";

    /// The `info.title` of the document.
    pub OPENAPI_TITLE: String = "openapi.title";

    /// The `info.version` of the document — your API's version, not the
    /// framework's.
    pub OPENAPI_VERSION: String = "openapi.version";

    /// The base URL clients should use, if the document should name one.
    pub OPENAPI_SERVER: String = "openapi.server";

    /// Whether to join and propagate W3C trace context.
    ///
    /// Cheap: no exporter, no collector, just the `traceparent` header and a
    /// trace id on every log line. Worth having on even without OTLP.
    pub TELEMETRY_ENABLED: bool = "telemetry.enabled";

    /// The OTLP collector's gRPC endpoint — `http://localhost:4317`.
    ///
    /// Absent unless set, and absent means spans are not exported. Needs the
    /// `otlp` feature.
    pub TELEMETRY_ENDPOINT: String = "telemetry.endpoint";

    /// What this service calls itself in a trace.
    pub TELEMETRY_SERVICE_NAME: String = "telemetry.service_name";

    /// What fraction of traces this service *starts* to record, `0.0` to
    /// `1.0`.
    ///
    /// A trace that arrives with a decision keeps it, whatever this says.
    pub TELEMETRY_SAMPLE_RATIO: f64 = "telemetry.sample_ratio";

    /// Which encryption envelope this application writes — `native` or
    /// `php`.
    ///
    /// `php` exists for a database a PHP application already filled, and
    /// is a migration position rather than a destination: it cannot rotate a
    /// key without re-encrypting, because its payload names no key.
    pub APP_CIPHER: CryptScheme = "app.cipher";

    /// The shape of a log line — `auto`, `pretty`, `compact`, `json`.
    ///
    /// `auto` is JSON in production and staging and pretty everywhere else,
    /// which is what you want without having to say so. Set it explicitly to
    /// read production logs by eye for an afternoon.
    pub LOG_FORMAT: LogFormat = "telemetry.log_format";

    // --- hashing -----------------------------------------------------------

    /// Which algorithm password hashing writes — `argon2id` or `bcrypt`.
    ///
    /// Verification is deliberately not governed by this: a stored hash names
    /// its own algorithm, and every registered driver's rows keep verifying
    /// whatever is selected. Changing it is a deploy, and rows convert on the
    /// next successful login.
    pub HASH_DRIVER: HashDriver = "hashing.driver";

    // --- database ----------------------------------------------------------

    /// The database DSN — `sqlite::memory:`, `mysql://…`, `postgres://…`.
    ///
    /// One connection written as one string, which is the whole of the database
    /// configuration for nearly every application: set it and the framework
    /// opens exactly one database, bound as a
    /// [`Database`](rainier_database::Database) and as the default of a
    /// [`DatabaseManager`](rainier_database::DatabaseManager) with nothing else
    /// in it. Leave it unset and no database is opened at all, which is what an
    /// application that has none should get.
    ///
    /// It declares the **default connection** the way `FILESYSTEM_DISK` names
    /// the default disk. More than one connection is [`DATABASES`], and the two
    /// are refused together — see there.
    pub DATABASE_URL: String = "database.url";

    /// Every database connection the application declares, and which of them is
    /// the default.
    ///
    /// A whole section rather than a scalar, for the reason [`FILESYSTEMS`] is
    /// one: a connection is not a single setting. It names its own engine, host,
    /// database and credentials, and a read replica, a reporting warehouse and a
    /// database some other system also writes to share none of them. Building
    /// the second from the first's DSN gives it the right *name* pointed at the
    /// wrong database — which is the quietest failure the framework has, because
    /// a query against the wrong database does not raise. It **answers**: the
    /// rows come back, the types match, and the report renders.
    ///
    /// ```
    /// use rainier_framework::config::Config;
    /// use rainier_framework::database::{Databases, SqliteDatabase};
    /// use rainier_framework::keys;
    ///
    /// let config = Config::new();
    /// config.set(keys::DATABASES, Databases::new("reporting")
    ///     .with("reporting", SqliteDatabase::new("storage/reporting.sqlite"))).unwrap();
    ///
    /// assert_eq!(config.string(keys::DATABASE_DEFAULT).as_deref(), Some("reporting"));
    /// ```
    ///
    /// Unlike `filesystems`, the framework seeds **nothing** here. A seeded disk
    /// costs a directory nobody writes to; a seeded connection opens a pool at
    /// boot against a database the application never asked for.
    ///
    /// Declaring this **and** [`DATABASE_URL`] is a boot failure rather than a
    /// precedence rule. Both name the default connection, so one of them would
    /// be inert while still sitting in the configuration being read by whoever
    /// changes it next — and the query that then runs against the winner comes
    /// back with rows rather than an error.
    pub DATABASES: Databases = "databases";

    /// Which declared connection a query naming none runs against.
    ///
    /// A connection that is not declared is a **boot failure**, not a fallback
    /// to whichever one is first: the fallback would answer, from a database
    /// nobody named, in a way no caller can tell from a correct answer.
    pub DATABASE_DEFAULT: String = "databases.default";

    // --- filesystems -------------------------------------------------------

    /// Every disk the application declares, and which of them is the default.
    ///
    /// A whole section rather than a scalar, because a disk is not one setting:
    /// it names its own driver and its own bucket, endpoint, region and
    /// credentials, and two disks on two services have nothing to share. The
    /// version of this that was a single driver plus one set of connection
    /// settings could not express the second disk at all, and building it from
    /// the first one's connector gave it the right bucket name pointed at the
    /// wrong host.
    ///
    /// ```
    /// use rainier_framework::config::Config;
    /// use rainier_framework::filesystem::{DiskConfig, Disks};
    /// use rainier_framework::keys;
    ///
    /// let config = Config::new();
    /// config.set(keys::FILESYSTEMS, Disks::new("uploads")
    ///     .with("uploads", DiskConfig::local("storage/app"))).unwrap();
    ///
    /// assert_eq!(config.string(keys::FILESYSTEM_DEFAULT).as_deref(), Some("uploads"));
    /// ```
    ///
    /// The framework seeds one `local` disk under `storage/app`, so a fresh
    /// clone has working storage; an application adds its own with
    /// [`Config::merge`](rainier_config::Config::merge) or replaces the section
    /// outright.
    pub FILESYSTEMS: Disks = "filesystems";

    /// Which declared disk [`Storage`](rainier_filesystem::Storage) uses when a
    /// call does not name one.
    ///
    /// A disk that is not declared is a **boot failure**, not a fallback to the
    /// framework's default: a write aimed at a disk nobody configured must not
    /// quietly land in a directory that goes away with the container.
    pub FILESYSTEM_DEFAULT: String = "filesystems.default";

    // --- cache -------------------------------------------------------------

    /// Which cache store to build.
    pub CACHE_DRIVER: CacheDriver = "cache.driver";

    /// The Redis DSN, or a comma-separated seed list for a cluster.
    pub CACHE_REDIS_URL: String = "cache.redis_url";

    /// `host:port` of the Memcached server.
    pub CACHE_MEMCACHED_URL: String = "cache.memcached_url";

    /// Prepended to every cache key, so two applications can share a server.
    pub CACHE_PREFIX: String = "cache.prefix";

    /// Every cache store the application declares, and which of them is the
    /// default.
    ///
    /// A whole section rather than a driver name and one set of settings, for
    /// the reason [`FILESYSTEMS`] and [`QUEUES`] are: two stores on two servers
    /// share no connector and no timeouts, and building the second from the
    /// first's connector gives it the right *name* pointed at the wrong server.
    ///
    /// The failure that produces is quiet in the way a cache's failures always
    /// are. Everything downstream of a cache is built to treat absence as
    /// normal — a miss is not an error — so a store on the wrong server is not
    /// an outage, it is a permanent miss that reads as a slow application. And
    /// when what was cached was a rate-limit counter or a lock, it is not slow,
    /// it is wrong.
    ///
    /// ```
    /// use rainier_framework::cache::{StoreConfig, Stores};
    /// use rainier_framework::config::Config;
    /// use rainier_framework::keys;
    ///
    /// let config = Config::new();
    /// config.set(keys::CACHE_STORES, Stores::new("scratch")
    ///     .with("scratch", StoreConfig::memory())).unwrap();
    ///
    /// assert!(config.get(keys::CACHE_STORES).is_some());
    /// ```
    ///
    /// Declaring this **and** [`CACHE_DRIVER`] is a boot failure rather than a
    /// precedence rule, for the same reason [`QUEUES`] and [`QUEUE_DRIVER`] are.
    pub CACHE_STORES: CacheStores = "cache.stores";

    // --- session -----------------------------------------------------------

    /// Where session state lives.
    pub SESSION_DRIVER: SessionDriver = "session.driver";

    /// How long a session survives without a request, in seconds.
    pub SESSION_LIFETIME: i64 = "session.lifetime";

    /// The name of the session cookie.
    pub SESSION_COOKIE: String = "session.cookie";

    /// Whether the session cookie is `Secure`. Must be true over HTTPS.
    pub SESSION_SECURE: bool = "session.secure";

    // --- queue -------------------------------------------------------------

    /// Where queued jobs wait.
    ///
    /// One backend, which is the whole of the queue configuration for nearly
    /// every application: set it and the framework builds exactly one
    /// connection, named after the driver, and binds the
    /// [`QueueManager`](rainier_queue::QueueManager) over it. Leave it unset and
    /// no queue is built at all.
    ///
    /// The settings that connection needs come from the environment beside it —
    /// `REDIS_URL` for `redis`, `SQS_QUEUE_URL` for `sqs`, [`KAFKA_BROKERS`] and
    /// friends for `kafka` — because one backend needs no section to name it.
    /// More than one is [`QUEUES`], and the two are refused together.
    pub QUEUE_DRIVER: QueueDriver = "queue.driver";

    /// The queue a job goes on when it does not name one.
    ///
    /// A *queue*, not a connection: the lane a job waits in inside whichever
    /// backend it was dispatched to. [`QUEUE_DEFAULT_CONNECTION`] is the other
    /// question, and the two are worth keeping apart — `high` and `bulk` are
    /// queues on one Redis, while `primary` and `bulk` may be two Redises.
    pub QUEUE_DEFAULT: String = "queue.default";

    /// Every queue connection the application declares, and which of them is the
    /// default.
    ///
    /// A whole section rather than a driver name and one set of settings, for
    /// the reason [`FILESYSTEMS`] is one: two connections on two backends share
    /// no client, no credential and no endpoint, and building the second from
    /// the first's client gives it the right *name* pointed at the wrong store.
    ///
    /// The failure that produces is quieter than the filesystem's. A disk on the
    /// wrong bucket reads back empty and somebody notices a missing file. A job
    /// pushed to the wrong backend is **accepted** — the push succeeds, an id
    /// comes back, the caller carries on — and then waits in a store no worker
    /// drains. Nothing raises, nothing retries, and there is no failed-job row,
    /// because the job never failed. It was never run.
    ///
    /// ```
    /// use rainier_framework::config::Config;
    /// use rainier_framework::keys;
    /// use rainier_framework::queue::{ConnectionConfig, Connections};
    ///
    /// let config = Config::new();
    /// config.set(keys::QUEUES, Connections::new("bulk")
    ///     .with("bulk", ConnectionConfig::memory())).unwrap();
    ///
    /// assert_eq!(config.string(keys::QUEUE_DEFAULT_CONNECTION).as_deref(), Some("bulk"));
    /// ```
    ///
    /// Declaring this **and** [`QUEUE_DRIVER`] is a boot failure rather than a
    /// precedence rule, for the same reason [`DATABASES`] and [`DATABASE_URL`]
    /// are — except that here the losing declaration stays invisible for longer,
    /// since a queue nobody drains reports nothing at all.
    pub QUEUES: QueueConnections = "queues";

    /// Which declared connection a dispatch naming none goes to.
    ///
    /// Not [`QUEUE_DEFAULT`], which names a queue inside a connection. A
    /// connection that is not declared is a **boot failure**, not a fallback:
    /// falling back would push the job to a backend nobody named and hand the
    /// caller an id for it.
    pub QUEUE_DEFAULT_CONNECTION: String = "queues.default";

    // --- kafka -------------------------------------------------------------

    /// The bootstrap brokers, comma-separated — `kafka-1:9092,kafka-2:9092`.
    ///
    /// Empty means no cluster is configured, which is what an application that
    /// does not use Kafka leaves it as.
    pub KAFKA_BROKERS: String = "kafka.brokers";

    /// Which set of cursors this deployment shares.
    ///
    /// A consumer group by another name. Two deployments reading one topic
    /// under different groups each get every record; under the same group they
    /// share it out. Changing it makes a worker start over from the beginning
    /// of the topic, so it is not a name to tidy up later.
    pub KAFKA_GROUP: String = "kafka.group";

    /// Prefixes every topic this application produces to or reads from.
    pub KAFKA_TOPIC_PREFIX: String = "kafka.topic_prefix";

    /// The topic broadcasts are published to.
    pub KAFKA_BROADCAST_TOPIC: String = "kafka.broadcast_topic";

    /// Whether to connect over TLS. Needs the `kafka-tls` feature.
    pub KAFKA_TLS: bool = "kafka.tls";

    /// The SASL username, if the cluster wants one.
    pub KAFKA_USERNAME: String = "kafka.username";

    /// The SASL password.
    ///
    /// Read from the environment like every other secret, and never written to
    /// a config file that a repository can hold.
    pub KAFKA_PASSWORD: String = "kafka.password";

    /// Which SASL mechanism the username and password are for.
    ///
    /// `plain`, `scram-sha-256` or `scram-sha-512`. `PLAIN` sends the password
    /// in the clear and belongs inside TLS.
    pub KAFKA_SASL_MECHANISM: String = "kafka.sasl_mechanism";

    // --- mail --------------------------------------------------------------

    /// Where mail goes.
    pub MAIL_DRIVER: MailDriver = "mail.driver";

    /// The default `From` address.
    pub MAIL_FROM_ADDRESS: String = "mail.from.address";

    /// The default `From` display name.
    pub MAIL_FROM_NAME: String = "mail.from.name";

    /// Redirect **every** message here instead of to its real recipients.
    ///
    /// Set it in staging and leave it set. Empty means "do not".
    pub MAIL_ALWAYS_TO: String = "mail.always_to";

    /// Where the `file` transport writes its `.eml` files.
    pub MAIL_FILE_PATH: String = "mail.file_path";

    /// The SMTP server. Required by the `smtp` driver, ignored by the rest.
    pub MAIL_HOST: String = "mail.host";

    /// The SMTP port. `0` — the default — means "whatever `MAIL_ENCRYPTION`'s
    /// arrangement conventionally uses": 587, 465 or 25.
    pub MAIL_PORT: i64 = "mail.port";

    /// The SMTP username. Empty means the server wants no authentication —
    /// a capture container, an internal relay.
    pub MAIL_USERNAME: String = "mail.username";

    /// The SMTP password.
    pub MAIL_PASSWORD: String = "mail.password";

    /// How the SMTP connection is secured: `starttls`, `tls` or `none`.
    pub MAIL_ENCRYPTION: MailEncryption = "mail.encryption";

    /// Seconds before an unanswered send is a failed one.
    pub MAIL_TIMEOUT: i64 = "mail.timeout";

    /// The Postmark server token. Required by the `postmark` driver.
    pub MAIL_POSTMARK_TOKEN: String = "mail.postmark.token";

    /// The Mailgun sending domain. Required by the `mailgun` driver.
    pub MAIL_MAILGUN_DOMAIN: String = "mail.mailgun.domain";

    /// The Mailgun API key. Required by the `mailgun` driver.
    pub MAIL_MAILGUN_SECRET: String = "mail.mailgun.secret";

    /// The Mailgun API base — `https://api.eu.mailgun.net` for an EU-region
    /// domain. Empty means the US endpoint.
    pub MAIL_MAILGUN_ENDPOINT: String = "mail.mailgun.endpoint";

    /// The SendGrid API key. Required by the `sendgrid` driver.
    pub MAIL_SENDGRID_KEY: String = "mail.sendgrid.key";

    /// The Resend API key. Required by the `resend` driver.
    /// The Cloudflare API token for Email Service — `MAIL_CLOUDFLARE_TOKEN`.
    ///
    /// Needs the **Email Sending: Edit** permission. It is the SMTP
    /// *password*; the username is always the literal string `api_token`, which
    /// the driver supplies so nobody has to know it.
    pub MAIL_CLOUDFLARE_TOKEN: String = "mail.cloudflare_token";

    pub MAIL_RESEND_KEY: String = "mail.resend.key";

    // The `ses` driver has no keys here on purpose: region and credentials
    // come from the AWS default chain — `AWS_REGION`, a profile, IMDS —
    // exactly as the other AWS drivers resolve theirs.
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_config::Config;

    #[test]
    fn a_driver_key_round_trips_as_its_wire_spelling() {
        let config = Config::new();
        config.set(CACHE_DRIVER, CacheDriver::RedisCluster).unwrap();

        // What a `.env` would hold, and what a config dump should show.
        assert_eq!(config.string("cache.driver").as_deref(), Some("redis-cluster"));
        assert_eq!(config.setting(CACHE_DRIVER).unwrap(), CacheDriver::RedisCluster);
    }

    #[test]
    fn every_key_is_under_the_section_its_name_says() {
        // A key filed under the wrong prefix reads fine and writes somewhere
        // nothing looks. Cheap to assert, and it has caught a paste already.
        let pairs: &[(&str, &str)] = &[
            ("APP", APP_NAME.path()),
            ("APP", APP_ENV.path()),
            ("APP", APP_DEBUG.path()),
            ("APP", APP_URL.path()),
            ("APP", APP_BASE_PATH.path()),
            ("SERVER", SERVER_HOST.path()),
            ("SERVER", SERVER_PORT.path()),
            ("SERVER", SERVER_MAX_BODY_BYTES.path()),
            ("SERVER", SERVER_REQUEST_TIMEOUT_SECS.path()),
            ("SERVER", SERVER_COMPRESSION.path()),
            ("DATABASE", DATABASE_URL.path()),
            ("DATABASES", DATABASE_DEFAULT.path()),
            ("FILESYSTEMS", FILESYSTEM_DEFAULT.path()),
            ("HASHING", HASH_DRIVER.path()),
            ("CACHE", CACHE_DRIVER.path()),
            ("CACHE", CACHE_REDIS_URL.path()),
            ("CACHE", CACHE_MEMCACHED_URL.path()),
            ("CACHE", CACHE_PREFIX.path()),
            ("CACHE", CACHE_STORES.path()),
            ("SESSION", SESSION_DRIVER.path()),
            ("SESSION", SESSION_LIFETIME.path()),
            ("SESSION", SESSION_COOKIE.path()),
            ("SESSION", SESSION_SECURE.path()),
            ("QUEUE", QUEUE_DRIVER.path()),
            ("QUEUE", QUEUE_DEFAULT.path()),
            ("QUEUES", QUEUE_DEFAULT_CONNECTION.path()),
            ("KAFKA", KAFKA_BROKERS.path()),
            ("KAFKA", KAFKA_GROUP.path()),
            ("KAFKA", KAFKA_TLS.path()),
            ("MAIL", MAIL_DRIVER.path()),
            ("MAIL", MAIL_FROM_ADDRESS.path()),
            ("MAIL", MAIL_FROM_NAME.path()),
            ("MAIL", MAIL_ALWAYS_TO.path()),
            ("MAIL", MAIL_FILE_PATH.path()),
            ("MAIL", MAIL_HOST.path()),
            ("MAIL", MAIL_PORT.path()),
            ("MAIL", MAIL_USERNAME.path()),
            ("MAIL", MAIL_PASSWORD.path()),
            ("MAIL", MAIL_ENCRYPTION.path()),
            ("MAIL", MAIL_TIMEOUT.path()),
            ("MAIL", MAIL_POSTMARK_TOKEN.path()),
            ("MAIL", MAIL_MAILGUN_DOMAIN.path()),
            ("MAIL", MAIL_MAILGUN_SECRET.path()),
            ("MAIL", MAIL_MAILGUN_ENDPOINT.path()),
            ("MAIL", MAIL_SENDGRID_KEY.path()),
            ("MAIL", MAIL_RESEND_KEY.path()),
            ("MAIL", MAIL_CLOUDFLARE_TOKEN.path()),
        ];

        for (section, path) in pairs {
            assert!(
                path.starts_with(&format!("{}.", section.to_lowercase())),
                "`{path}` is filed under the wrong section"
            );
        }
    }

    #[test]
    fn no_two_keys_name_the_same_path() {
        let paths = [
            APP_NAME.path(),
            APP_ENV.path(),
            APP_DEBUG.path(),
            APP_URL.path(),
            APP_BASE_PATH.path(),
            SERVER_HOST.path(),
            SERVER_PORT.path(),
            SERVER_MAX_BODY_BYTES.path(),
            SERVER_REQUEST_TIMEOUT_SECS.path(),
            SERVER_COMPRESSION.path(),
            DATABASE_URL.path(),
            DATABASES.path(),
            DATABASE_DEFAULT.path(),
            FILESYSTEMS.path(),
            FILESYSTEM_DEFAULT.path(),
            HASH_DRIVER.path(),
            CACHE_DRIVER.path(),
            CACHE_REDIS_URL.path(),
            CACHE_MEMCACHED_URL.path(),
            CACHE_PREFIX.path(),
            CACHE_STORES.path(),
            SESSION_DRIVER.path(),
            SESSION_LIFETIME.path(),
            SESSION_COOKIE.path(),
            SESSION_SECURE.path(),
            QUEUE_DRIVER.path(),
            QUEUE_DEFAULT.path(),
            QUEUES.path(),
            QUEUE_DEFAULT_CONNECTION.path(),
            KAFKA_BROKERS.path(),
            KAFKA_GROUP.path(),
            KAFKA_TOPIC_PREFIX.path(),
            KAFKA_BROADCAST_TOPIC.path(),
            KAFKA_TLS.path(),
            KAFKA_USERNAME.path(),
            KAFKA_PASSWORD.path(),
            KAFKA_SASL_MECHANISM.path(),
            MAIL_DRIVER.path(),
            MAIL_FROM_ADDRESS.path(),
            MAIL_FROM_NAME.path(),
            MAIL_ALWAYS_TO.path(),
            MAIL_FILE_PATH.path(),
            MAIL_HOST.path(),
            MAIL_PORT.path(),
            MAIL_USERNAME.path(),
            MAIL_PASSWORD.path(),
            MAIL_ENCRYPTION.path(),
            MAIL_TIMEOUT.path(),
            MAIL_POSTMARK_TOKEN.path(),
            MAIL_MAILGUN_DOMAIN.path(),
            MAIL_MAILGUN_SECRET.path(),
            MAIL_MAILGUN_ENDPOINT.path(),
            MAIL_SENDGRID_KEY.path(),
            MAIL_RESEND_KEY.path(),
        ];

        let unique: std::collections::BTreeSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "two keys share a path: {paths:?}");
    }
}
