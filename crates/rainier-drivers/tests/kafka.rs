//! The Kafka client against a real broker.
//!
//! Everything interesting about this driver is a property of Kafka: that a key
//! decides a partition, that a fetch from an offset returns what is after it,
//! that reading before the start of a log is an error and not an empty answer.
//! None of it can be checked without a broker, so these tests skip unless one
//! is reachable and CI provides one.
//!
//! ```sh
//! docker run --rm -p 9092:9092 apache/kafka:3.9.0
//! KAFKA_BROKERS=localhost:9092 cargo test -p rainier-drivers --features kafka --test kafka
//! ```
#![cfg(feature = "kafka")]

use std::time::Duration;

use rainier_drivers::kafka::{
    partition_for_key, KafkaClient, KafkaConnector, KafkaOffset, KafkaRecord,
};

/// Where the tests look for a cluster.
fn brokers() -> String {
    std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".to_string())
}

/// A client, or `None` when nothing answers.
///
/// Skipping rather than failing, so a contributor with no Kafka gets a green
/// suite — **except** where `KAFKA_REQUIRED` is set, which CI does. A suite
/// that silently skipped in CI would be a driver nobody had ever run, reported
/// as passing.
async fn client() -> Option<KafkaClient> {
    KafkaClient::connect(
        &KafkaConnector::parse(&brokers())
            .with_client_id("rainier-tests")
            // Short, so a machine with no broker skips in seconds rather than
            // waiting out a production-shaped timeout nine times over.
            .with_timeout(Duration::from_secs(3)),
    )
    .await
    .ok()
}

macro_rules! kafka_or_skip {
    ($name:literal) => {
        match client().await {
            Some(client) => client,
            None if std::env::var("KAFKA_REQUIRED").is_ok_and(|required| !required.is_empty()) => {
                panic!("KAFKA_REQUIRED is set and no broker answered at {}", brokers())
            }
            None => {
                eprintln!("skipping `{}`: no Kafka at KAFKA_BROKERS", $name);
                return;
            }
        }
    };
}

/// A topic nothing else in the suite uses.
///
/// Named per test rather than shared: these run concurrently, and a topic they
/// all read would make every assertion about offsets depend on the others.
async fn topic(client: &KafkaClient, name: &str, partitions: i32) -> String {
    let topic = format!("rainier-test-{name}");

    client.create_topic(&topic, partitions, 1).await.expect("create the topic");

    topic
}

#[tokio::test]
async fn a_produced_record_comes_back_with_its_key_and_headers() {
    let client = kafka_or_skip!("roundtrip");
    let topic = topic(&client, "roundtrip", 1).await;

    let placed = client
        .produce(
            &topic,
            vec![KafkaRecord::new("the body").keyed("orders.7").header("event", "OrderShipped")],
        )
        .await
        .expect("produce");

    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].topic, topic);

    let fetch = client
        .fetch(&topic, placed[0].partition, placed[0].offset, 1024 * 1024, Duration::from_secs(5))
        .await
        .expect("fetch")
        .expect("the offset we were just given is in the log");

    let message = fetch.messages.first().expect("the record we produced");

    assert_eq!(message.value, b"the body");
    assert_eq!(message.key.as_deref(), Some(&b"orders.7"[..]));
    assert_eq!(message.header("event"), Some("OrderShipped"));
    assert_eq!(message.offset, placed[0].offset);
}

#[tokio::test]
async fn records_come_back_in_the_order_they_went_in() {
    let client = kafka_or_skip!("order");
    let topic = topic(&client, "order", 1).await;

    let records: Vec<_> =
        (0..5).map(|n| KafkaRecord::new(n.to_string()).keyed("one-key")).collect();

    let placed = client.produce(&topic, records).await.expect("produce");
    let first = placed.first().expect("five records").clone();

    let fetch = client
        .fetch(&topic, first.partition, first.offset, 1024 * 1024, Duration::from_secs(5))
        .await
        .expect("fetch")
        .expect("in the log");

    let bodies: Vec<String> = fetch
        .messages
        .iter()
        .take(5)
        .map(|message| String::from_utf8(message.value.clone()).unwrap())
        .collect();

    assert_eq!(bodies, ["0", "1", "2", "3", "4"]);
}

#[tokio::test]
async fn one_key_always_lands_on_one_partition() {
    // The ordering guarantee, end to end: Kafka orders within a partition and
    // nowhere else, so "these are in order" means "these share a key".
    let client = kafka_or_skip!("keying");
    let topic = topic(&client, "keying", 4).await;

    let placed = client
        .produce(&topic, (0..6).map(|_| KafkaRecord::new("x").keyed("account-42")).collect())
        .await
        .expect("produce");

    let partitions: std::collections::HashSet<i32> =
        placed.iter().map(|position| position.partition).collect();

    assert_eq!(partitions.len(), 1, "one key must not spread across partitions");

    // And it is the partition the arithmetic said it would be, which is what
    // makes a JVM producer agree with this one.
    assert_eq!(
        placed[0].partition as usize,
        partition_for_key(b"account-42", 4),
        "the broker put it somewhere the partitioner did not predict"
    );
}

#[tokio::test]
async fn keyless_records_spread_across_the_partitions() {
    let client = kafka_or_skip!("spread");
    let topic = topic(&client, "spread", 4).await;

    let placed = client
        .produce(&topic, (0..24).map(|_| KafkaRecord::new("x")).collect())
        .await
        .expect("produce");

    let partitions: std::collections::HashSet<i32> =
        placed.iter().map(|position| position.partition).collect();

    assert!(partitions.len() > 1, "everything landed on {partitions:?}");
}

#[tokio::test]
async fn the_watermarks_move_with_what_was_written() {
    let client = kafka_or_skip!("watermarks");
    let topic = topic(&client, "watermarks", 1).await;

    let before = client.offset(&topic, 0, KafkaOffset::Latest).await.expect("latest");

    client
        .produce(&topic, vec![KafkaRecord::new("a"), KafkaRecord::new("b")])
        .await
        .expect("produce");

    let after = client.offset(&topic, 0, KafkaOffset::Latest).await.expect("latest");

    assert_eq!(after, before + 2, "two records moved the end by two");
    assert!(
        client.offset(&topic, 0, KafkaOffset::Earliest).await.expect("earliest") <= before,
        "the start cannot be past the end"
    );
}

#[tokio::test]
async fn reading_past_the_end_is_empty_rather_than_an_error() {
    let client = kafka_or_skip!("past-the-end");
    let topic = topic(&client, "past-the-end", 1).await;

    let end = client.offset(&topic, 0, KafkaOffset::Latest).await.expect("latest");

    let fetch = client
        .fetch(&topic, 0, end, 1024 * 1024, Duration::from_millis(200))
        .await
        .expect("fetch")
        .expect("the end of the log is a place you may wait at");

    assert!(fetch.is_empty(), "nothing has been written past the end");
    assert_eq!(fetch.high_watermark, end);
    assert_eq!(fetch.next_offset(), None, "there is nothing to advance past");
}

#[tokio::test]
async fn reading_before_the_start_says_so_rather_than_returning_nothing() {
    // The distinction a consumer has to act on: "nothing yet" means wait, and
    // "that offset is gone" means the records were retained away and the
    // cursor has to be reset. Returning an empty fetch for both would hang a
    // consumer forever on a topic whose head it had fallen behind.
    let client = kafka_or_skip!("before-the-start");
    let topic = topic(&client, "before-the-start", 1).await;

    client.produce(&topic, vec![KafkaRecord::new("a")]).await.expect("produce");

    let fetch = client
        .fetch(&topic, 0, -1, 1024 * 1024, Duration::from_millis(200))
        .await
        .expect("a negative offset is answered, not an error");

    assert!(fetch.is_none(), "an offset that is not in the log must report itself");
}

#[tokio::test]
async fn creating_a_topic_twice_is_not_a_failure() {
    // Two replicas booting together both try, and only one can win.
    let client = kafka_or_skip!("idempotent-create");
    let name = "rainier-test-idempotent-create";

    let first = client.create_topic(name, 1, 1).await.expect("create");
    let second = client.create_topic(name, 1, 1).await.expect("create again");

    assert!(!second, "the second call did not create it");
    let _ = first;

    assert!(
        client.topics().await.expect("list").iter().any(|topic| topic == name),
        "the topic should exist either way"
    );
}

#[tokio::test]
async fn a_topic_that_does_not_exist_has_no_partitions() {
    let client = kafka_or_skip!("unknown-topic");

    let partitions = client.partitions("rainier-test-no-such-topic-here").await.expect("ask");

    assert!(partitions.is_none(), "absent is not the same as having no partitions");
}
