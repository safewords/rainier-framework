//! Building the Kafka pieces from configuration.
//!
//! The step between `KAFKA_BROKERS` in an environment file and the objects the
//! [broadcaster](rainier_broadcast::kafka), the [relay](crate::relay) and the
//! [queue](rainier_queue::kafka) are built from — so an application writes a
//! provider rather than assembling a connector by hand.
//!
//! ```ignore
//! // app/providers/kafka.rs
//! pub fn register(app: &Application) -> Result<()> {
//!     let config = app.resolve::<Config>()?;
//!     let client = kafka::client(&config).await?;
//!
//!     app.instance(Broadcasting::new(Arc::new(kafka::broadcaster(&config, Arc::clone(&client))?)));
//!
//!     relay::spawn(
//!         kafka::relay(&config, client)?,
//!         SocketFanOut::new(app.resolve::<Rooms>()?),
//!     );
//!     Ok(())
//! }
//! ```

use std::sync::Arc;

use rainier_broadcast::kafka::{KafkaBroadcaster, KafkaRelay};
use rainier_cache::LockManager;
use rainier_config::Config;
use rainier_drivers::kafka::{
    KafkaClient, KafkaConnector, KafkaCredentials, KafkaRecord, SaslMechanism,
};
use rainier_queue::KafkaQueue;
use rainier_support::{Error, Result};
use serde::Serialize;

use crate::keys;

/// The connector `KAFKA_*` describes, or `None` when no brokers are named.
///
/// `None` rather than an error: an application that does not use Kafka should
/// not have to say so, and the caller decides whether its absence matters.
pub fn connector(config: &Config) -> Result<Option<KafkaConnector>> {
    let brokers = config.get(keys::KAFKA_BROKERS).unwrap_or_default();

    if brokers.trim().is_empty() {
        return Ok(None);
    }

    let mut connector = KafkaConnector::parse(&brokers)
        .with_client_id(config.get(keys::APP_NAME).unwrap_or_else(|| "rainier".into()));

    if config.get(keys::KAFKA_TLS).unwrap_or(false) {
        connector = connector.with_tls();
    }

    let username = config.get(keys::KAFKA_USERNAME).unwrap_or_default();
    if !username.is_empty() {
        let mechanism = mechanism(&config.get(keys::KAFKA_SASL_MECHANISM).unwrap_or_default())?;
        let password = config.get(keys::KAFKA_PASSWORD).unwrap_or_default();

        connector =
            connector.with_credentials(KafkaCredentials::new(mechanism, username, password));
    }

    Ok(Some(connector))
}

/// Connect to the configured cluster.
///
/// # Errors
///
/// When `KAFKA_BROKERS` is empty — a provider asking for a client has decided
/// this application needs one, and starting without it only moves the failure
/// to the first broadcast.
pub async fn client(config: &Config) -> Result<Arc<KafkaClient>> {
    let connector = connector(config)?.ok_or_else(|| {
        Error::internal(
            "Kafka is not configured. Set `KAFKA_BROKERS` to a comma-separated list of brokers.",
        )
    })?;

    Ok(Arc::new(KafkaClient::connect(&connector).await?))
}

/// The broadcaster `KAFKA_BROADCAST_TOPIC` and `KAFKA_TOPIC_PREFIX` describe.
pub fn broadcaster(config: &Config, client: Arc<KafkaClient>) -> KafkaBroadcaster {
    let topic = topic(config, keys::KAFKA_BROADCAST_TOPIC, "broadcasts");

    KafkaBroadcaster::new(client).on_topic(topic)
}

/// A relay for the same topic the broadcaster publishes to.
pub fn relay(config: &Config, client: Arc<KafkaClient>) -> KafkaRelay {
    KafkaRelay::new(client, topic(config, keys::KAFKA_BROADCAST_TOPIC, "broadcasts"))
}

/// The queue `KAFKA_GROUP` and `KAFKA_TOPIC_PREFIX` describe.
///
/// # Errors
///
/// When `locks` is not backed by a shared store — see
/// [`KafkaQueue::new`](rainier_queue::KafkaQueue::new).
pub fn queue(config: &Config, client: Arc<KafkaClient>, locks: LockManager) -> Result<KafkaQueue> {
    let queue = KafkaQueue::new(client, locks)?
        .in_group(config.get(keys::KAFKA_GROUP).unwrap_or_else(|| "rainier".into()));

    Ok(match config.get(keys::KAFKA_TOPIC_PREFIX) {
        Some(prefix) if !prefix.is_empty() => queue.with_topic_prefix(prefix),
        _ => queue,
    })
}

/// The record an event becomes on the way to a topic.
///
/// The event's own name is a header, so a consumer can route on it without
/// deserialising, and the body is the event as JSON. Keyed by whatever the
/// caller says identifies the subject — an account id, an order number — which
/// is what puts everything about one subject on one partition, in order.
fn record_for_event<E: Serialize>(name: &str, event: &E, key: String) -> Result<KafkaRecord> {
    let body = serde_json::to_vec(event)
        .map_err(|e| Error::internal(format!("an event published to Kafka must serialise: {e}")))?;

    Ok(KafkaRecord::new(body).keyed(key).header("event", name.to_string()))
}

/// Publish every event of one type to a topic, as it happens.
///
/// A broadcast pointed at a log rather than at a browser: the
/// same event that updates the page becomes a record other services can read.
///
/// ```ignore
/// kafka::publish_events::<OrderShipped>(
///     &events,
///     Arc::clone(&client),
///     "orders",
///     |event| event.order_id.to_string(),
/// );
/// ```
///
/// # A failed publish does not fail the request
///
/// It is logged and swallowed. A listener that returns `Err` stops the
/// listeners behind it, and "the analytics topic was unreachable" is not a
/// reason to abandon the rest of what an event set off. Where the publish
/// *must* happen, publish from a [job](rainier_queue) instead, so it retries.
pub fn publish_events<E>(
    events: &rainier_events::Dispatcher,
    client: Arc<KafkaClient>,
    topic: impl Into<String>,
    key_of: impl Fn(&E) -> String + Send + Sync + 'static,
) where
    E: rainier_events::Event + Serialize,
{
    let topic = topic.into();
    let name = E::event_name();

    events.listen::<E, _>(move |event: Arc<E>| {
        let client = Arc::clone(&client);
        let topic = topic.clone();
        let key = key_of(&event);

        Box::pin(async move {
            match record_for_event(name, &*event, key) {
                Ok(record) => {
                    if let Err(e) = client.produce(&topic, vec![record]).await {
                        tracing::error!(topic, event = name, error = %e, "could not publish an event");
                    }
                }
                Err(e) => tracing::error!(topic, event = name, error = %e, "could not serialise an event"),
            }
            Ok(())
        })
    });
}

/// A configured topic name, prefixed.
fn topic(config: &Config, key: rainier_config::Key<String>, fallback: &str) -> String {
    let prefix = config.get(keys::KAFKA_TOPIC_PREFIX).unwrap_or_default();

    let name = config
        .get(key)
        .filter(|configured| !configured.is_empty())
        .unwrap_or_else(|| fallback.to_string());

    format!("{prefix}{name}")
}

/// The mechanism a name refers to.
///
/// A closed set, and unknown is an error rather than a default: silently
/// falling back to `PLAIN` would send a password in the clear because somebody
/// misspelled `scram-sha-512`.
fn mechanism(name: &str) -> Result<SaslMechanism> {
    match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "" | "plain" => Ok(SaslMechanism::Plain),
        "scram-sha-256" => Ok(SaslMechanism::ScramSha256),
        "scram-sha-512" => Ok(SaslMechanism::ScramSha512),
        other => Err(Error::internal(format!(
            "`KAFKA_SASL_MECHANISM={other}` is not a mechanism. Use `plain`, `scram-sha-256` or \
             `scram-sha-512`."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_config::Env;

    fn config(env: &str) -> Config {
        let config = Config::new();
        let env = Env::parse(env).isolated();

        config.set(keys::KAFKA_BROKERS, env.string("KAFKA_BROKERS", "")).unwrap();
        config.set(keys::KAFKA_TLS, env.bool("KAFKA_TLS", false)).unwrap();
        config.set(keys::KAFKA_USERNAME, env.string("KAFKA_USERNAME", "")).unwrap();
        config.set(keys::KAFKA_PASSWORD, env.string("KAFKA_PASSWORD", "")).unwrap();
        config
            .set(keys::KAFKA_SASL_MECHANISM, env.string("KAFKA_SASL_MECHANISM", "plain"))
            .unwrap();
        config.set(keys::KAFKA_TOPIC_PREFIX, env.string("KAFKA_TOPIC_PREFIX", "")).unwrap();
        config
            .set(keys::KAFKA_BROADCAST_TOPIC, env.string("KAFKA_BROADCAST_TOPIC", "broadcasts"))
            .unwrap();
        config.set(keys::APP_NAME, env.string("APP_NAME", "rainier")).unwrap();
        config
    }

    #[test]
    fn no_brokers_is_not_an_error() {
        // An application that does not use Kafka should not have to say so.
        assert!(connector(&config("APP_ENV=local")).unwrap().is_none());
    }

    #[test]
    fn the_broker_list_becomes_a_connector() {
        let connector = connector(&config("KAFKA_BROKERS=kafka-1:9092,kafka-2\nAPP_NAME=checkout"))
            .unwrap()
            .expect("brokers were configured");

        assert_eq!(connector.brokers(), ["kafka-1:9092", "kafka-2:9092"]);
        assert!(!connector.is_tls());
        assert!(connector.credentials().is_none());
    }

    #[test]
    fn credentials_are_only_used_when_a_username_is_set() {
        let with = connector(&config(
            "KAFKA_BROKERS=kafka:9092\nKAFKA_USERNAME=svc\nKAFKA_PASSWORD=secret\n\
             KAFKA_SASL_MECHANISM=scram-sha-512",
        ))
        .unwrap()
        .unwrap();

        let credentials = with.credentials().expect("a username was configured");
        assert_eq!(credentials.mechanism(), SaslMechanism::ScramSha512);
        assert_eq!(credentials.username(), "svc");

        let without =
            connector(&config("KAFKA_BROKERS=kafka:9092\nKAFKA_PASSWORD=secret")).unwrap().unwrap();
        assert!(without.credentials().is_none(), "a password alone authenticates nobody");
    }

    #[test]
    fn a_misspelled_mechanism_stops_the_boot() {
        // The alternative is falling back to PLAIN and sending the password in
        // the clear because somebody typed `scram-512`.
        let error = connector(&config(
            "KAFKA_BROKERS=kafka:9092\nKAFKA_USERNAME=svc\nKAFKA_SASL_MECHANISM=scram-512",
        ))
        .unwrap_err();

        assert!(error.message().contains("scram-sha-512"), "{}", error.message());
    }

    #[test]
    fn mechanism_names_are_forgiving_about_case_and_underscores() {
        assert_eq!(mechanism("SCRAM_SHA_256").unwrap(), SaslMechanism::ScramSha256);
        assert_eq!(mechanism(" plain ").unwrap(), SaslMechanism::Plain);
        assert_eq!(mechanism("").unwrap(), SaslMechanism::Plain);
    }

    #[test]
    fn the_prefix_reaches_the_topic_names() {
        let config = config("KAFKA_BROKERS=kafka:9092\nKAFKA_TOPIC_PREFIX=checkout.");

        assert_eq!(
            topic(&config, keys::KAFKA_BROADCAST_TOPIC, "broadcasts"),
            "checkout.broadcasts"
        );
    }

    #[test]
    fn a_named_broadcast_topic_wins_over_the_default() {
        let config = config("KAFKA_BROKERS=kafka:9092\nKAFKA_BROADCAST_TOPIC=events");

        assert_eq!(topic(&config, keys::KAFKA_BROADCAST_TOPIC, "broadcasts"), "events");
    }

    #[test]
    fn an_event_becomes_a_keyed_record_carrying_its_name() {
        #[derive(serde::Serialize)]
        struct OrderShipped {
            order_id: u64,
        }

        let record =
            record_for_event("OrderShipped", &OrderShipped { order_id: 7 }, "7".into()).unwrap();

        assert_eq!(record.key.as_deref(), Some(&b"7"[..]));
        assert_eq!(record.headers.get("event").map(Vec::as_slice), Some(&b"OrderShipped"[..]));

        // The header exists so a consumer can route without parsing the body,
        // and the body is still the whole event.
        let body: serde_json::Value = serde_json::from_slice(&record.value).unwrap();
        assert_eq!(body["order_id"], 7);
    }

    #[test]
    fn tls_is_off_unless_it_is_asked_for() {
        let plain = connector(&config("KAFKA_BROKERS=kafka:9092")).unwrap().unwrap();
        assert!(!plain.is_tls());

        let secured =
            connector(&config("KAFKA_BROKERS=kafka:9092\nKAFKA_TLS=true")).unwrap().unwrap();
        assert!(secured.is_tls());
    }
}
