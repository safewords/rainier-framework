//! Broadcasting as configuration — [`BroadcasterConfig`], [`RedisBroadcast`],
//! [`KafkaBroadcast`].
//!
//! Every other backend in this framework is *declared*: the queue has
//! `ConnectionConfig`, the cache has `StoreConfig`, the filesystem has
//! `DiskConfig`. Each is a typed struct per driver, deserialisable from the
//! configuration tree, built by the framework. Broadcasting was the exception —
//! an application handed [`Broadcasting::new`] an `Arc<dyn Broadcaster>` it had
//! constructed itself.
//!
//! # What that cost
//!
//! Choosing a broadcaster is not one line. It means opening a connector,
//! awaiting a connection, handling the failure, deciding whether an unreachable
//! Redis should stop the boot, assembling the Pusher signature, and
//! feature-gating the whole block so it still compiles without the driver. In
//! the application this was extracted from it was a hundred lines of
//! `bootstrap.rs` — and every one of those decisions is the *framework's*
//! question, asked and answered identically by every application that
//! broadcasts at all.
//!
//! ```
//! # use rainier_broadcast::{BroadcasterConfig, RedisBroadcast};
//! let declared = BroadcasterConfig::Redis(RedisBroadcast {
//!     url: "redis://cache:6379".into(),
//!     prefix: Some("lewd-production".into()),
//!     key: Some("app-key".into()),
//!     secret: Some("app-secret".into()),
//! });
//!
//! assert_eq!(declared.driver_name(), "redis");
//! assert!(declared.can_authorise_private_channels());
//! ```
//!
//! A struct literal rather than a builder chain: every field is public and
//! [`Default`] is implemented, so a declaration names what it sets and nothing
//! else.
//!
//! ```
//! # use rainier_broadcast::{BroadcasterConfig, RedisBroadcast};
//! BroadcasterConfig::Redis(RedisBroadcast {
//!     url: "redis://localhost:6379".into(),
//!     ..Default::default()
//! });
//! ```
//!
//! # The same thing, from the configuration tree
//!
//! The struct's fields and the section's keys are deliberately the same names,
//! so reading one tells you the other:
//!
//! ```
//! # use rainier_broadcast::BroadcasterConfig;
//! # use serde_json::json;
//! let declared: BroadcasterConfig = serde_json::from_value(json!({
//!     "driver": "redis",
//!     "url": "redis://cache:6379",
//!     "prefix": "lewd-production",
//!     "key": "app-key",
//!     "secret": "app-secret",
//! })).unwrap();
//!
//! assert_eq!(declared.driver_name(), "redis");
//! ```
//!
//! # Why there is no `Broadcasters` set
//!
//! The queue, cache and filesystem each declare a *map* of named backends,
//! because each of those managers dispatches by name. [`Broadcasting`] holds
//! one driver and has no notion of a name, so a set here would be a concept the
//! rest of the crate does not have — a map whose every read is
//! `.get("default")`. One declaration, one broadcaster, until something
//! actually needs to publish two ways.
//!
//! # Falling back is a decision, so it has a name
//!
//! An unreachable Redis should cost realtime rather than the whole application
//! — but *silently* becoming a log broadcaster is worse than either, because
//! the symptom is "the site works and nothing updates live", which nobody
//! reports for hours.
//!
//! So [`build`](BroadcasterConfig::build) returns the error and
//! [`build_or_log`](BroadcasterConfig::build_or_log) is the fallback with the
//! diagnostics attached. Degrading is a choice an application makes on purpose,
//! in a place a reader can find.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use rainier_support::{Error, Result};

use crate::broadcaster::{Broadcaster, LogBroadcaster, MemoryBroadcaster};
use crate::manager::Broadcasting;
use crate::pusher::PusherAuth;

/// A declared broadcaster.
///
/// One variant per driver, each carrying its own settings and built from those
/// alone. Nothing is inherited between declarations, which is what stops a
/// second broadcaster picking up the first one's credentials and publishing
/// where nobody is subscribed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawBroadcaster", into = "RawBroadcaster")]
pub enum BroadcasterConfig {
    /// Write each broadcast to the log and publish nothing.
    ///
    /// The honest default for a deployment that has not configured
    /// broadcasting. **It cannot sign a private channel** — see
    /// [`can_authorise_private_channels`](Self::can_authorise_private_channels)
    /// for why that is worth saying out loud rather than discovering later.
    Log,

    /// Keep broadcasts in this process, for tests.
    Memory,

    /// Publish over Redis pub/sub — what soketi, Reverb and other
    /// Pusher-protocol servers subscribe to.
    Redis(RedisBroadcast),

    /// Publish to a Kafka topic, for a relay to fan out.
    Kafka(KafkaBroadcast),
}

impl Default for BroadcasterConfig {
    /// [`Log`](Self::Log): publishes nothing, loses nothing, and needs no
    /// backend to be reachable.
    fn default() -> Self {
        Self::Log
    }
}

/// Publishing over Redis pub/sub.
///
/// A *declaration*, not an open socket — [`crate::redis::RedisBroadcaster`] is
/// the connected thing, the way `S3Disk` is a declaration and a `Filesystem` is
/// the disk.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RedisBroadcast {
    /// `redis://host:port/db`, or `rediss://` for TLS.
    ///
    /// **Give it the URL that carries the credentials.** A URL rebuilt from a
    /// discrete host and port has no userinfo, and a Redis requiring auth then
    /// refuses every publish with `NOAUTH` — for the life of the process, on
    /// every channel, while boot logs the connection truthfully because the
    /// client connects lazily.
    ///
    /// A comma-separated seed list is accepted for a cluster; an ordinary
    /// `PUBLISH` reaches the whole cluster, so the first seed is used.
    pub url: String,

    /// Prefix every published channel name.
    ///
    /// Two applications sharing a Redis need it, and getting it wrong is
    /// silent: the publish succeeds and nobody is subscribed to what was
    /// published.
    pub prefix: Option<String>,

    /// The Pusher app key — public, and sent to the browser.
    ///
    /// Set both this and [`secret`](Self::secret) or neither; one alone is
    /// refused, because it cannot produce a signature.
    pub key: Option<String>,

    /// The Pusher app secret, which is what makes a signature mean anything.
    ///
    /// Without the pair, any Pusher-compatible relay refuses to let a browser
    /// join a **private** channel: the auth endpoint answers `200` with an
    /// empty body and the subscription is dropped client-side — a `200` that
    /// means "no".
    ///
    /// It belongs in the environment beside the encryption key. Neither
    /// [`Debug`] nor the boot diagnostics ever print it.
    pub secret: Option<String>,
}

/// Publishing to a Kafka topic.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KafkaBroadcast {
    /// Bootstrap brokers. At least one, or there is nowhere to connect.
    pub brokers: Vec<String>,

    /// The topic to publish to.
    ///
    /// `None` uses the driver's own default rather than a copy of it here, so
    /// the two cannot drift into this naming a topic the driver stopped using.
    pub topic: Option<String>,

    /// Prefix every channel name.
    ///
    /// Matters more here than on Redis: two applications sharing a topic is
    /// normal, and without a prefix each one's relay fans the other's
    /// broadcasts out to its own browsers.
    pub prefix: Option<String>,

    /// The Pusher app key. See [`RedisBroadcast::key`].
    pub key: Option<String>,

    /// The Pusher app secret. See [`RedisBroadcast::secret`].
    pub secret: Option<String>,
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, and it stays that way — the same rule the
/// queue's `RedisConnection` carries. A derived `Debug` prints the URL's
/// userinfo, which for a configuration dump at boot means the password is in
/// the log of every process that started.
impl std::fmt::Debug for RedisBroadcast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBroadcast")
            .field("url", &redacted_url(&self.url))
            .field("prefix", &self.prefix)
            .field("key", &self.key)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Names the brokers and never the secret. See [`RedisBroadcast`]'s.
impl std::fmt::Debug for KafkaBroadcast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaBroadcast")
            .field("brokers", &self.brokers)
            .field("topic", &self.topic)
            .field("prefix", &self.prefix)
            .field("key", &self.key)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A URL with any `user:password@` replaced.
///
/// String surgery rather than a URL parse, because this runs in `Debug` and a
/// malformed URL must still print *something* — a panic or an empty string
/// inside a diagnostic is worse than an imperfect redaction.
fn redacted_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.split_once('@') {
        Some((_, host)) => format!("{scheme}://<redacted>@{host}"),
        None => url.to_string(),
    }
}

impl BroadcasterConfig {
    /// Publish over Redis at `url`. The shorthand.
    pub fn redis(url: impl Into<String>) -> Self {
        Self::Redis(RedisBroadcast { url: url.into(), ..Default::default() })
    }

    /// The driver this declares, by the name the configuration tree uses.
    pub fn driver_name(&self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Memory => "memory",
            Self::Redis(_) => "redis",
            Self::Kafka(_) => "kafka",
        }
    }

    /// Whether this declaration can sign a private channel.
    ///
    /// Surfaced because the failure it predicts is invisible from the server:
    /// `POST /broadcasting/auth` answers `200` with an empty body, the browser
    /// subscribes to nothing, and every realtime feature is dead while the
    /// application looks entirely healthy.
    pub fn can_authorise_private_channels(&self) -> bool {
        self.auth().is_some()
    }

    /// The Pusher credentials, when the declaration carries a complete pair.
    ///
    /// A pair is all-or-nothing by the time a declaration exists — the
    /// conversion below refuses a lone key or secret — so this cannot see half
    /// of one.
    fn auth(&self) -> Option<PusherAuth> {
        let (key, secret) = match self {
            Self::Log | Self::Memory => (&None, &None),
            Self::Redis(redis) => (&redis.key, &redis.secret),
            Self::Kafka(kafka) => (&kafka.key, &kafka.secret),
        };

        match (key, secret) {
            (Some(key), Some(secret)) => Some(PusherAuth::new(key.clone(), secret.clone())),
            _ => None,
        }
    }

    /// Build this broadcaster, and only this one.
    ///
    /// # Errors
    ///
    /// When the driver's feature is off, or the backend refuses the connection.
    /// Both are real answers: see [`build_or_log`](Self::build_or_log) for the
    /// degrading alternative and why it is a separate, named choice.
    pub async fn build(&self) -> Result<Broadcasting> {
        Ok(Broadcasting::new(self.driver().await?))
    }

    async fn driver(&self) -> Result<Arc<dyn Broadcaster>> {
        let auth = self.auth();
        // Read only by the driver arms, and a build with neither feature has
        // none of them — so bind it rather than let the warning become noise
        // that hides a real one.
        let _ = &auth;

        match self {
            Self::Log => Ok(Arc::new(LogBroadcaster)),
            Self::Memory => Ok(Arc::new(MemoryBroadcaster::new())),

            #[cfg(feature = "redis")]
            Self::Redis(redis) => {
                use rainier_drivers::redis::RedisConnector;

                // A cluster is declared as a comma-separated seed list and this
                // wants one endpoint: an ordinary `PUBLISH` reaches the whole
                // cluster, so the first seed is enough.
                let url = redis.url.split(',').next().unwrap_or(&redis.url).trim();
                if url.is_empty() {
                    return Err(Error::internal(
                        "a redis broadcaster needs a url; an empty one has nowhere to publish.",
                    ));
                }

                let connector = RedisConnector::open(url)?;
                let mut broadcaster = crate::redis::RedisBroadcaster::connect(&connector).await?;
                if let Some(prefix) = &redis.prefix {
                    broadcaster = broadcaster.with_prefix(prefix.clone());
                }
                if let Some(auth) = auth {
                    broadcaster = broadcaster.with_pusher_auth(auth);
                }
                Ok(Arc::new(broadcaster))
            }
            #[cfg(not(feature = "redis"))]
            Self::Redis(_) => Err(Error::internal(
                "this build has no redis broadcaster; enable rainier-broadcast's `redis` feature.",
            )),

            #[cfg(feature = "kafka")]
            Self::Kafka(kafka) => {
                use rainier_drivers::{KafkaClient, KafkaConnector};

                let client = Arc::new(
                    KafkaClient::connect(&KafkaConnector::new(kafka.brokers.clone())).await?,
                );
                let mut broadcaster = crate::kafka::KafkaBroadcaster::new(client);
                if let Some(topic) = &kafka.topic {
                    broadcaster = broadcaster.on_topic(topic.clone());
                }
                if let Some(prefix) = &kafka.prefix {
                    broadcaster = broadcaster.with_prefix(prefix.clone());
                }
                if let Some(auth) = auth {
                    broadcaster = broadcaster.with_pusher_auth(auth);
                }
                Ok(Arc::new(broadcaster))
            }
            #[cfg(not(feature = "kafka"))]
            Self::Kafka(_) => Err(Error::internal(
                "this build has no kafka broadcaster; enable rainier-broadcast's `kafka` feature.",
            )),
        }
    }

    /// Build it, or fall back to the log broadcaster and say so loudly.
    ///
    /// For the deployment that would rather lose realtime than fail to boot.
    /// The diagnostics are the point: a log broadcaster is indistinguishable
    /// from a working one until somebody notices nothing updates live, and by
    /// then the boot output is long gone. Each line names the **symptom**
    /// rather than the cause, because the symptom is what the person reading
    /// logs an hour later actually has.
    ///
    /// # Why `eprintln!` and not `tracing`
    ///
    /// This runs during bootstrap, and in most applications the tracing
    /// subscriber is installed by the same `boot()` that has not finished. A
    /// `tracing::warn!` here reaches no subscriber and is dropped — a
    /// diagnostic that looks present and is not, which is worse than none,
    /// because it reads as "this was checked and was fine".
    pub async fn build_or_log(&self) -> Broadcasting {
        if !self.can_authorise_private_channels() {
            eprintln!(
                "warning: the {} broadcaster cannot sign private channels.",
                self.driver_name()
            );
            eprintln!("         Symptom: POST /broadcasting/auth answers 200 with an empty");
            eprintln!("         body instead of {{\"auth\":\"<key>:<signature>\"}}, the browser");
            eprintln!("         subscribes to nothing, and no feature updates live.");
        }

        match self.build().await {
            Ok(broadcasting) => broadcasting,
            Err(e) => {
                eprintln!(
                    "error: the {} broadcaster could not be built: {}",
                    self.driver_name(),
                    e.message()
                );
                eprintln!("       Falling back to the log broadcaster.");
                eprintln!("       Symptom: the application serves normally and nothing");
                eprintln!("       updates live — no feeds, no notifications, no chat.");
                Broadcasting::log()
            }
        }
    }
}

/// The flat shape a `broadcasting` section has on the wire.
///
/// Every field optional and `deny_unknown_fields`, so a misspelt setting is a
/// boot failure naming the key rather than a value read by nothing. The
/// conversion below is where a declaration is refused.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBroadcaster {
    driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    brokers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

impl TryFrom<RawBroadcaster> for BroadcasterConfig {
    type Error = String;

    fn try_from(raw: RawBroadcaster) -> std::result::Result<Self, Self::Error> {
        // Half a credential pair cannot produce a signature. Accepting it
        // would build a broadcaster that looks authorised and refuses every
        // private subscription, which is the failure this module is built to
        // make loud.
        match (&raw.key, &raw.secret) {
            (Some(_), None) => {
                return Err("a broadcaster with `key` needs `secret`; one alone cannot sign \
                            a private channel."
                    .into())
            }
            (None, Some(_)) => {
                return Err("a broadcaster with `secret` needs `key`; one alone cannot sign \
                            a private channel."
                    .into())
            }
            _ => {}
        }

        // Named individually rather than "unexpected field", because the
        // useful thing to say is which driver *would* have honoured it.
        let wrong_driver = |setting: &str, owner: &str| -> String {
            format!(
                "`{setting}` is a {owner} setting and the {} broadcaster does not read it; \
                 remove it or change the driver.",
                raw.driver
            )
        };

        match raw.driver.as_str() {
            "log" | "memory" => {
                // An ignored setting is worse than a rejected one: somebody has
                // configured a prefix and every channel would publish
                // unprefixed, with nothing to say so.
                for (name, present) in [
                    ("url", raw.url.is_some()),
                    ("brokers", raw.brokers.is_some()),
                    ("topic", raw.topic.is_some()),
                    ("prefix", raw.prefix.is_some()),
                    ("key", raw.key.is_some()),
                    ("secret", raw.secret.is_some()),
                ] {
                    if present {
                        return Err(format!(
                            "`{name}` is not read by the {} broadcaster, which publishes \
                             nowhere; remove it or change the driver.",
                            raw.driver
                        ));
                    }
                }
                Ok(if raw.driver == "log" { Self::Log } else { Self::Memory })
            }

            "redis" => {
                if raw.brokers.is_some() {
                    return Err(wrong_driver("brokers", "kafka"));
                }
                if raw.topic.is_some() {
                    return Err(wrong_driver("topic", "kafka"));
                }
                let url = raw.url.ok_or_else(|| {
                    "a redis broadcaster needs a `url`; an assumed one publishes to whatever \
                     happens to be on localhost."
                        .to_string()
                })?;
                Ok(Self::Redis(RedisBroadcast {
                    url,
                    prefix: raw.prefix,
                    key: raw.key,
                    secret: raw.secret,
                }))
            }

            "kafka" => {
                if raw.url.is_some() {
                    return Err(wrong_driver("url", "redis"));
                }
                let brokers = raw.brokers.unwrap_or_default();
                if brokers.is_empty() {
                    return Err("a kafka broadcaster needs at least one entry in `brokers`; a \
                                client with no bootstrap broker has nowhere to connect."
                        .into());
                }
                Ok(Self::Kafka(KafkaBroadcast {
                    brokers,
                    topic: raw.topic,
                    prefix: raw.prefix,
                    key: raw.key,
                    secret: raw.secret,
                }))
            }

            other => Err(format!(
                "unknown broadcast driver `{other}`; expected one of log, memory, redis, kafka."
            )),
        }
    }
}

impl From<BroadcasterConfig> for RawBroadcaster {
    fn from(config: BroadcasterConfig) -> Self {
        let bare = |driver: &str| RawBroadcaster {
            driver: driver.to_string(),
            url: None,
            brokers: None,
            topic: None,
            prefix: None,
            key: None,
            secret: None,
        };

        match config {
            BroadcasterConfig::Log => bare("log"),
            BroadcasterConfig::Memory => bare("memory"),
            BroadcasterConfig::Redis(redis) => RawBroadcaster {
                url: Some(redis.url),
                prefix: redis.prefix,
                key: redis.key,
                secret: redis.secret,
                ..bare("redis")
            },
            BroadcasterConfig::Kafka(kafka) => RawBroadcaster {
                brokers: Some(kafka.brokers),
                topic: kafka.topic,
                prefix: kafka.prefix,
                key: kafka.key,
                secret: kafka.secret,
                ..bare("kafka")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> std::result::Result<BroadcasterConfig, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    fn signed() -> BroadcasterConfig {
        BroadcasterConfig::Redis(RedisBroadcast {
            url: "redis://cache:6379".into(),
            prefix: Some("app".into()),
            key: Some("k".into()),
            secret: Some("s".into()),
        })
    }

    #[test]
    fn a_declaration_is_a_struct_literal() {
        // The shape this module exists for: named fields, not a builder chain,
        // with `..Default::default()` for everything it does not set.
        let declared = BroadcasterConfig::Redis(RedisBroadcast {
            url: "redis://cache:6379".into(),
            ..Default::default()
        });

        assert_eq!(declared.driver_name(), "redis");
        assert!(!declared.can_authorise_private_channels());
    }

    #[test]
    fn it_round_trips_through_the_configuration_tree() {
        let json = serde_json::to_value(signed()).expect("serialises");
        let back: BroadcasterConfig = serde_json::from_value(json).expect("deserialises");

        assert_eq!(back, signed());
    }

    #[test]
    fn the_struct_fields_and_the_section_keys_are_the_same_names() {
        // Deliberate, and worth a test: reading either one tells you the
        // other, so nobody has to keep a mapping in their head.
        let json = serde_json::to_value(signed()).expect("serialises");
        let object = json.as_object().expect("an object");

        for key in ["driver", "url", "prefix", "key", "secret"] {
            assert!(object.contains_key(key), "missing {key} in {json}");
        }
    }

    #[test]
    fn debug_never_prints_the_secret_or_the_password() {
        // The rule the queue's RedisConnection already carries: a derived
        // Debug puts the URL's userinfo into the log of every process that
        // starts.
        let declared = RedisBroadcast {
            url: "redis://someone:hunter2@cache:6379".into(),
            prefix: None,
            key: Some("k".into()),
            secret: Some("hunter2".into()),
        };

        let shown = format!("{declared:?}");

        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        // The host is still there, because a diagnostic that names nothing is
        // not a diagnostic.
        assert!(shown.contains("cache:6379"), "{shown}");
    }

    #[test]
    fn a_url_without_credentials_is_shown_whole() {
        assert_eq!(redacted_url("redis://cache:6379"), "redis://cache:6379");
        // Malformed input still prints something rather than panicking.
        assert_eq!(redacted_url("not a url"), "not a url");
    }

    #[test]
    fn a_key_without_a_secret_is_refused() {
        let err =
            parse(json!({"driver": "redis", "url": "redis://x", "key": "k"})).expect_err("refused");

        assert!(err.contains("needs `secret`"), "{err}");
    }

    #[test]
    fn a_secret_without_a_key_is_refused() {
        let err = parse(json!({"driver": "redis", "url": "redis://x", "secret": "s"}))
            .expect_err("refused");

        assert!(err.contains("needs `key`"), "{err}");
    }

    #[test]
    fn a_redis_declaration_needs_a_url() {
        // An assumed URL publishes to whatever is on localhost, which in
        // production is nothing, silently.
        let err = parse(json!({"driver": "redis"})).expect_err("refused");

        assert!(err.contains("needs a `url`"), "{err}");
    }

    #[test]
    fn a_kafka_setting_on_a_redis_declaration_is_refused_by_name() {
        // Somebody believes these broadcasts reach Kafka. They reach Redis.
        let err = parse(json!({"driver": "redis", "url": "redis://x", "brokers": ["a:9092"]}))
            .expect_err("refused");

        assert!(err.contains("brokers") && err.contains("kafka"), "{err}");
    }

    #[test]
    fn a_redis_setting_on_a_kafka_declaration_is_refused_by_name() {
        let err = parse(json!({"driver": "kafka", "brokers": ["a:9092"], "url": "redis://x"}))
            .expect_err("refused");

        assert!(err.contains("url") && err.contains("redis"), "{err}");
    }

    #[test]
    fn kafka_needs_a_broker() {
        let err = parse(json!({"driver": "kafka", "brokers": []})).expect_err("refused");

        assert!(err.contains("bootstrap broker"), "{err}");
    }

    #[test]
    fn a_setting_the_log_driver_cannot_honour_is_refused_rather_than_dropped() {
        for setting in ["prefix", "url"] {
            let err = parse(json!({"driver": "log", setting: "anything"})).expect_err("refused");
            assert!(err.contains(setting), "{err}");
        }
    }

    #[test]
    fn credentials_on_a_driver_that_cannot_sign_are_refused() {
        // Otherwise somebody has configured private channels and every
        // subscription is refused, with a 200 and no explanation.
        let err = parse(json!({"driver": "log", "key": "k", "secret": "s"})).expect_err("refused");

        assert!(err.contains("publishes") || err.contains("key"), "{err}");
    }

    #[test]
    fn an_unknown_driver_names_the_ones_that_exist() {
        let err = parse(json!({"driver": "redys", "url": "redis://x"})).expect_err("refused");

        assert!(err.contains("redis"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_refused_naming_itself() {
        let err = parse(json!({"driver": "redis", "url": "redis://x", "prefx": "app"}))
            .expect_err("refused");

        assert!(err.contains("prefx"), "{err}");
    }

    #[test]
    fn only_a_complete_pair_can_authorise_a_private_channel() {
        assert!(!BroadcasterConfig::Log.can_authorise_private_channels());
        assert!(!BroadcasterConfig::Memory.can_authorise_private_channels());
        assert!(!BroadcasterConfig::redis("redis://x").can_authorise_private_channels());
        assert!(signed().can_authorise_private_channels());
    }

    #[tokio::test]
    async fn the_log_and_memory_drivers_build_without_a_backend() {
        assert_eq!(BroadcasterConfig::Log.build().await.expect("builds").driver_name(), "log");
        assert_eq!(
            BroadcasterConfig::Memory.build().await.expect("builds").driver_name(),
            "memory"
        );
    }

    #[tokio::test]
    async fn build_or_log_degrades_instead_of_failing() {
        // Why the fallback is a separate method: this is a deployment choosing
        // to lose realtime rather than not boot, and the choice should be
        // visible in the code that made it.
        let unreachable = BroadcasterConfig::Redis(RedisBroadcast {
            url: "redis://127.0.0.1:1".into(),
            ..Default::default()
        });

        assert_eq!(unreachable.build_or_log().await.driver_name(), "log");
    }
}
