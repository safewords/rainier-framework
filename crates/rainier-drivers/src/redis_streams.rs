//! Redis streams and sorted sets — the commands a queue needs.
//!
//! Split from [`redis`](crate::redis) because it is a different *use* of the
//! same connection: the cache needs `GET`/`SET`, a queue needs an
//! acknowledgement protocol, and mixing them makes one file nobody can hold in
//! their head.
//!
//! # Why streams and not lists
//!
//! `LPUSH`/`BRPOP` is the usual Redis queue, and it has no acknowledgement:
//! `BRPOP` removes the entry, so a worker that dies holding one has taken it
//! with it. Streams (Redis 5.0+) have consumer groups — a pending entry list,
//! `XACK`, and `XAUTOCLAIM` to redeliver what a dead consumer left — which is
//! the reserve-then-acknowledge protocol
//! [`Queue`](../rainier_queue/trait.Queue.html) requires. A list cannot honour
//! that contract; a stream can.

use std::time::Duration;

use redis::Value;

use rainier_support::Result;

use crate::redis::RedisClient;

/// The field every entry's body is stored under.
///
/// One field rather than a field per property: the body is already JSON, and
/// splitting it across stream fields would mean two serialisations that can
/// disagree.
pub const BODY: &str = "body";

/// One entry read out of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEntry {
    /// The stream's id for it — `1700000000000-0`. What `XACK` needs.
    pub id: String,
    /// The body, as stored.
    pub body: Vec<u8>,
}

/// Promote every due member of a sorted set into a stream, atomically.
///
/// Streams cannot hold a delayed entry, so a delayed job waits in a sorted set
/// scored by the millisecond it becomes available, and something has to move
/// it across. In Lua because the read and the two writes have to be one step:
/// two workers promoting at once would otherwise both see the same member and
/// add it twice.
const PROMOTE_DUE: &str = r"
    local due = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1])
    if #due == 0 then return 0 end
    for _, body in ipairs(due) do
        redis.call('ZREM', KEYS[1], body)
        redis.call('XADD', KEYS[2], '*', ARGV[2], body)
    end
    return #due
";

impl RedisClient {
    /// `XADD stream * body <entry>`. Returns the entry's id.
    pub async fn xadd(&self, stream: &str, body: &[u8]) -> Result<String> {
        self.connection().query(redis::cmd("XADD").arg(stream).arg("*").arg(BODY).arg(body)).await
    }

    /// `XGROUP CREATE stream group $ MKSTREAM`, tolerating one that exists.
    ///
    /// Creating the group is idempotent by being allowed to fail: `BUSYGROUP`
    /// means another worker got there first, which is the normal case on every
    /// start after the first.
    pub async fn xgroup_create(&self, stream: &str, group: &str) -> Result<()> {
        // `0` rather than `$`: `$` would deliver only entries added *after* the
        // group existed, so every job pushed before the first worker started
        // would sit in the stream forever.
        let outcome: Result<()> = self
            .connection()
            .run(redis::cmd("XGROUP").arg("CREATE").arg(stream).arg(group).arg("0").arg("MKSTREAM"))
            .await;

        match outcome {
            Ok(()) => Ok(()),
            // Matched on the text, because the error mapping keeps Redis's
            // message and drops its `BUSYGROUP` code. `XGROUP CREATE` has one
            // "already exists" to report, so this cannot swallow another.
            Err(e) if already_exists(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `XREADGROUP … COUNT 1 STREAMS stream >` — one entry never delivered
    /// before.
    pub async fn xreadgroup_one(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
    ) -> Result<Option<StreamEntry>> {
        let reply: Value = self
            .connection()
            .query(
                redis::cmd("XREADGROUP")
                    .arg("GROUP")
                    .arg(group)
                    .arg(consumer)
                    .arg("COUNT")
                    .arg(1)
                    .arg("STREAMS")
                    .arg(stream)
                    .arg(">"),
            )
            .await?;

        Ok(read_reply(&reply))
    }

    /// `XAUTOCLAIM` — one entry another consumer took and never acknowledged.
    ///
    /// This is redelivery: an entry idle longer than `min_idle` belongs to a
    /// worker that is not coming back, and claiming it is how the job survives
    /// that worker's death.
    pub async fn xautoclaim_one(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        min_idle: Duration,
    ) -> Result<Option<StreamEntry>> {
        let reply: Value = self
            .connection()
            .query(
                redis::cmd("XAUTOCLAIM")
                    .arg(stream)
                    .arg(group)
                    .arg(consumer)
                    .arg(min_idle.as_millis() as u64)
                    .arg("0-0")
                    .arg("COUNT")
                    .arg(1),
            )
            .await?;

        Ok(claim_reply(&reply))
    }

    /// `XACK` then `XDEL` — finished with this entry, and it can go.
    ///
    /// Both, because `XACK` alone only clears the pending list: the entry stays
    /// in the stream, and a queue that never deletes grows until it fills the
    /// memory Redis is holding everything else in.
    pub async fn xack_delete(&self, stream: &str, group: &str, id: &str) -> Result<()> {
        self.connection().run(redis::cmd("XACK").arg(stream).arg(group).arg(id)).await?;
        self.connection().run(redis::cmd("XDEL").arg(stream).arg(id)).await
    }

    /// `XCLAIM ... IDLE 0 JUSTID` — say the holder is still working.
    ///
    /// Resets the entry's idle time without moving it or re-delivering it, so
    /// a consumer that is genuinely mid-job keeps its reservation.
    ///
    /// This is what stops `XAUTOCLAIM` taking live work. A reservation is a
    /// claim about liveness, not a guess at how long the work takes: without a
    /// way to say "still here", the only lever is a timeout long enough for the
    /// slowest imaginable job, which is either far too long to reclaim genuinely
    /// dead work or far too short to survive a real one.
    pub async fn xclaim_touch(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        id: &str,
    ) -> Result<()> {
        self.connection()
            .run(
                redis::cmd("XCLAIM")
                    .arg(stream)
                    .arg(group)
                    .arg(consumer)
                    // Minimum idle time of 0: claim it regardless of how long
                    // it has been pending, since it is already ours.
                    .arg(0)
                    .arg(id)
                    .arg("IDLE")
                    .arg(0)
                    // No payload back; this is a keep-alive, not a delivery.
                    .arg("JUSTID"),
            )
            .await
    }

    /// `XLEN`.
    pub async fn xlen(&self, stream: &str) -> Result<u64> {
        self.connection().query(redis::cmd("XLEN").arg(stream)).await
    }

    /// `ZADD key score member`.
    pub async fn zadd(&self, key: &str, score: i64, member: &[u8]) -> Result<()> {
        self.connection().run(redis::cmd("ZADD").arg(key).arg(score).arg(member)).await
    }

    /// `ZCARD`.
    pub async fn zcard(&self, key: &str) -> Result<u64> {
        self.connection().query(redis::cmd("ZCARD").arg(key)).await
    }

    /// Move everything in `zset` scored at or before `now_ms` into `stream`.
    ///
    /// Returns how many moved. A Lua script, so the read and the two writes
    /// are one step: two workers promoting at once would otherwise both see the
    /// same member and add it twice.
    pub async fn promote_due(&self, zset: &str, stream: &str, now_ms: i64) -> Result<u64> {
        self.connection()
            .query(
                redis::cmd("EVAL")
                    .arg(PROMOTE_DUE)
                    .arg(2)
                    .arg(zset)
                    .arg(stream)
                    .arg(now_ms)
                    .arg(BODY),
            )
            .await
    }

    /// `LPUSH`. Returns the list's new length.
    pub async fn lpush(&self, key: &str, value: &[u8]) -> Result<u64> {
        self.connection().query(redis::cmd("LPUSH").arg(key).arg(value)).await
    }

    /// `LRANGE key 0 -1`.
    pub async fn lrange_all(&self, key: &str) -> Result<Vec<Vec<u8>>> {
        self.connection().query(redis::cmd("LRANGE").arg(key).arg(0).arg(-1)).await
    }

    /// `CONFIG GET name`, or `None` when the server will not say.
    ///
    /// A managed Redis often disables `CONFIG`, so "no answer" is a normal
    /// outcome and not an error — the caller wanted to check something, not to
    /// depend on it.
    pub async fn config_get(&self, name: &str) -> Result<Option<String>> {
        let reply: Value =
            match self.connection().query(redis::cmd("CONFIG").arg("GET").arg(name)).await {
                Ok(reply) => reply,
                Err(_) => return Ok(None),
            };

        Ok(config_value(&reply))
    }

    /// `DEL` over several keys. Returns how many existed.
    pub async fn delete_all(&self, keys: &[String]) -> Result<u64> {
        if keys.is_empty() {
            return Ok(0);
        }

        let mut command = redis::cmd("DEL");
        for key in keys {
            command.arg(key);
        }
        self.connection().query(&command).await
    }
}

/// Whether an error is Redis saying the consumer group is already there.
///
/// The normal case on every start after the first, so it has to be tolerated —
/// and it is checked by message because that is all the mapped error keeps.
fn already_exists(error: &rainier_support::Error) -> bool {
    let message = error.message();
    message.contains("BUSYGROUP") || message.contains("already exists")
}

/// The value out of a `CONFIG GET` reply — `[name, value]`, or a map on RESP3.
fn config_value(reply: &Value) -> Option<String> {
    match reply {
        Value::Array(pairs) => as_string(pairs.get(1)?),
        Value::Map(pairs) => pairs.first().and_then(|(_, value)| as_string(value)),
        _ => None,
    }
}

/// The first entry in an `XREADGROUP` reply.
///
/// `[[stream_name, [[id, [field, value, …]], …]], …]` — one level deeper than
/// `XAUTOCLAIM`, because the reply can cover several streams.
fn read_reply(reply: &Value) -> Option<StreamEntry> {
    let Value::Array(streams) = reply else { return None };

    streams.iter().find_map(|stream| {
        let Value::Array(parts) = stream else { return None };
        entries(parts.get(1)?)
    })
}

/// The first entry in an `XAUTOCLAIM` reply.
///
/// `[cursor, [[id, [field, value, …]], …], [deleted…]]`.
fn claim_reply(reply: &Value) -> Option<StreamEntry> {
    let Value::Array(parts) = reply else { return None };
    entries(parts.get(1)?)
}

/// The first `[id, [field, value, …]]` pair, as an entry.
fn entries(value: &Value) -> Option<StreamEntry> {
    let Value::Array(entries) = value else { return None };

    entries.iter().find_map(|entry| {
        let Value::Array(parts) = entry else { return None };
        let id = as_string(parts.first()?)?;
        let body = field(parts.get(1)?, BODY)?;

        Some(StreamEntry { id, body })
    })
}

/// The value of `name` in a flat `[field, value, field, value]` array.
fn field(value: &Value, name: &str) -> Option<Vec<u8>> {
    let Value::Array(pairs) = value else { return None };

    pairs.chunks(2).find_map(|pair| match pair {
        [key, value] if as_string(key).as_deref() == Some(name) => as_bytes(value),
        _ => None,
    })
}

fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => String::from_utf8(bytes.clone()).ok(),
        Value::SimpleString(text) => Some(text.clone()),
        _ => None,
    }
}

fn as_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(bytes) => Some(bytes.clone()),
        Value::SimpleString(text) => Some(text.clone().into_bytes()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    /// What `XREADGROUP GROUP g c COUNT 1 STREAMS s >` actually replies with.
    fn xreadgroup_reply() -> Value {
        Value::Array(vec![Value::Array(vec![
            bulk("queue:default"),
            Value::Array(vec![Value::Array(vec![
                bulk("1700000000000-0"),
                Value::Array(vec![bulk("body"), bulk(r#"{"id":"j1"}"#)]),
            ])]),
        ])])
    }

    /// What `XAUTOCLAIM s g c 30000 0-0 COUNT 1` replies with.
    fn xautoclaim_reply() -> Value {
        Value::Array(vec![
            bulk("0-0"),
            Value::Array(vec![Value::Array(vec![
                bulk("1700000000001-0"),
                Value::Array(vec![bulk("body"), bulk(r#"{"id":"j2"}"#)]),
            ])]),
            Value::Array(vec![]),
        ])
    }

    #[test]
    fn a_read_reply_yields_its_entry() {
        let entry = read_reply(&xreadgroup_reply()).expect("one entry");

        assert_eq!(entry.id, "1700000000000-0");
        assert_eq!(entry.body, br#"{"id":"j1"}"#.to_vec());
    }

    #[test]
    fn a_claim_reply_is_one_level_shallower_and_still_parses() {
        // The two commands nest differently, which is the whole reason these
        // are separate functions and separately tested.
        let entry = claim_reply(&xautoclaim_reply()).expect("one entry");

        assert_eq!(entry.id, "1700000000001-0");
        assert_eq!(entry.body, br#"{"id":"j2"}"#.to_vec());
    }

    #[test]
    fn an_empty_queue_yields_nothing_rather_than_an_error() {
        // Both commands answer this way when there is nothing to take, and it
        // is the most common reply a busy worker sees.
        assert_eq!(read_reply(&Value::Nil), None);
        assert_eq!(read_reply(&Value::Array(vec![])), None);

        let empty_claim =
            Value::Array(vec![bulk("0-0"), Value::Array(vec![]), Value::Array(vec![])]);
        assert_eq!(claim_reply(&empty_claim), None);
    }

    #[test]
    fn an_entry_missing_its_body_field_is_skipped() {
        // Something else wrote to this stream. Better to see nothing than to
        // hand the queue a job it will fail to parse on every redelivery.
        let reply = Value::Array(vec![Value::Array(vec![
            bulk("queue:default"),
            Value::Array(vec![Value::Array(vec![
                bulk("1-0"),
                Value::Array(vec![bulk("something-else"), bulk("{}")]),
            ])]),
        ])]);

        assert_eq!(read_reply(&reply), None);
    }

    #[test]
    fn a_reply_shape_we_do_not_know_is_none_not_a_panic() {
        for reply in [Value::Okay, Value::Int(1), bulk("nonsense")] {
            assert_eq!(read_reply(&reply), None);
            assert_eq!(claim_reply(&reply), None);
        }
    }

    #[test]
    fn a_config_reply_yields_its_value_in_either_protocol() {
        let resp2 = Value::Array(vec![bulk("maxmemory-policy"), bulk("noeviction")]);
        let resp3 = Value::Map(vec![(bulk("maxmemory-policy"), bulk("allkeys-lru"))]);

        assert_eq!(config_value(&resp2).as_deref(), Some("noeviction"));
        assert_eq!(config_value(&resp3).as_deref(), Some("allkeys-lru"));
        assert_eq!(config_value(&Value::Nil), None);
    }

    #[test]
    fn the_promotion_script_moves_and_removes_in_one_step() {
        // Read as a string, because the alternative is discovering at 3am that
        // it promoted without removing and every delayed job ran forever.
        assert!(PROMOTE_DUE.contains("ZRANGEBYSCORE"));
        assert!(PROMOTE_DUE.contains("ZREM"));
        assert!(PROMOTE_DUE.contains("XADD"));
    }
}
