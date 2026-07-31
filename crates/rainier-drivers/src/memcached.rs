//! Memcached transport — a small client for the text protocol.
//!
//! Written here rather than taken from a crate for one practical reason: the
//! obvious candidate does not compile on Windows (it imports `UnixStream`
//! unconditionally), and the protocol is small enough that carrying it is
//! cheaper than carrying a portability problem. What is below is the six
//! commands a cache needs, and nothing else.

use std::sync::Arc;
use std::time::Duration;

use rainier_support::{Error, ErrorKind, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// A reply to a storage command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// The value was written.
    Stored,
    /// `add` found the key already present.
    NotStored,
}

/// One connection to a Memcached server.
#[derive(Debug)]
pub struct MemcachedConnection {
    stream: BufReader<TcpStream>,
}

impl MemcachedConnection {
    /// Connect to `host:port`.
    pub async fn connect(address: &str) -> Result<Self> {
        let stream =
            TcpStream::connect(address).await.map_err(|e| unreachable_error("connect", e))?;
        // Nagle would add up to 40ms to every small command, and every command
        // here is small.
        let _ = stream.set_nodelay(true);
        Ok(Self { stream: BufReader::new(stream) })
    }

    /// `get` — the value at `key`, or `None`.
    pub async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        self.write_all(format!("get {key}\r\n").as_bytes()).await?;

        let header = self.read_line().await?;
        if header == "END" {
            return Ok(None);
        }

        // VALUE <key> <flags> <bytes>
        let mut fields = header.split(' ');
        if fields.next() != Some("VALUE") {
            return Err(protocol_error(&header));
        }
        let length: usize = fields
            .nth(2)
            .and_then(|bytes| bytes.trim().parse().ok())
            .ok_or_else(|| protocol_error(&header))?;

        // The body is exactly `length` bytes, then CRLF. Reading by length
        // rather than by line is what makes a value containing newlines work.
        let mut body = vec![0u8; length + 2];
        self.stream.read_exact(&mut body).await.map_err(|e| unreachable_error("read", e))?;
        body.truncate(length);

        // Then a bare END.
        let terminator = self.read_line().await?;
        if terminator != "END" {
            return Err(protocol_error(&terminator));
        }

        Ok(Some(body))
    }

    /// `set` — store unconditionally.
    pub async fn set(&mut self, key: &str, value: &[u8], expiry: u32) -> Result<()> {
        match self.store("set", key, value, expiry).await? {
            Stored::Stored => Ok(()),
            Stored::NotStored => Err(protocol_error("NOT_STORED in reply to set")),
        }
    }

    /// `add` — store only if the key is absent.
    pub async fn add(&mut self, key: &str, value: &[u8], expiry: u32) -> Result<Stored> {
        self.store("add", key, value, expiry).await
    }

    async fn store(&mut self, verb: &str, key: &str, value: &[u8], expiry: u32) -> Result<Stored> {
        let mut command = format!("{verb} {key} 0 {expiry} {}\r\n", value.len()).into_bytes();
        command.extend_from_slice(value);
        command.extend_from_slice(b"\r\n");
        self.write_all(&command).await?;

        match self.read_line().await?.as_str() {
            "STORED" => Ok(Stored::Stored),
            "NOT_STORED" | "EXISTS" => Ok(Stored::NotStored),
            other => Err(protocol_error(other)),
        }
    }

    /// `gets` — the value at `key` and its CAS token, or `None`.
    ///
    /// The CAS token is a version number the server bumps on every write. It is
    /// the only thing that makes "change this **if** nobody else has" possible
    /// over a protocol with no conditional delete.
    pub async fn gets(&mut self, key: &str) -> Result<Option<(Vec<u8>, u64)>> {
        self.write_all(format!("gets {key}\r\n").as_bytes()).await?;

        let header = self.read_line().await?;
        if header == "END" {
            return Ok(None);
        }

        // VALUE <key> <flags> <bytes> <cas>
        let fields: Vec<&str> = header.split(' ').collect();
        if fields.first() != Some(&"VALUE") || fields.len() < 5 {
            return Err(protocol_error(&header));
        }

        let length: usize = fields[3].trim().parse().map_err(|_| protocol_error(&header))?;
        let cas: u64 = fields[4].trim().parse().map_err(|_| protocol_error(&header))?;

        let mut body = vec![0u8; length + 2];
        self.stream.read_exact(&mut body).await.map_err(|e| unreachable_error("read", e))?;
        body.truncate(length);

        let terminator = self.read_line().await?;
        if terminator != "END" {
            return Err(protocol_error(&terminator));
        }

        Ok(Some((body, cas)))
    }

    /// `cas` — store only if nobody has written since `cas` was read.
    ///
    /// `Stored::NotStored` covers both of the server's refusals: `EXISTS`
    /// (somebody wrote in between) and `NOT_FOUND` (the key went away). Neither
    /// is an error, and to the one caller that matters — releasing a lock —
    /// they mean the same thing: it is not yours any more.
    ///
    /// A negative `expiry` expires the item immediately, which is how a
    /// conditional *delete* is spelled in a protocol that has no such command.
    pub async fn compare_and_swap(
        &mut self,
        key: &str,
        value: &[u8],
        expiry: i64,
        cas: u64,
    ) -> Result<Stored> {
        let mut command = format!("cas {key} 0 {expiry} {} {cas}\r\n", value.len()).into_bytes();
        command.extend_from_slice(value);
        command.extend_from_slice(b"\r\n");
        self.write_all(&command).await?;

        match self.read_line().await?.as_str() {
            "STORED" => Ok(Stored::Stored),
            "EXISTS" | "NOT_FOUND" => Ok(Stored::NotStored),
            other => Err(protocol_error(other)),
        }
    }

    /// Delete `key`, but only if it currently holds `expected`.
    ///
    /// Memcached has no conditional delete, so this is `gets` to read the value
    /// and its CAS token, then a `cas` that expires the item immediately. The
    /// CAS token is what makes it atomic: if anybody wrote between the two
    /// commands, the token has moved and the `cas` is refused.
    pub async fn delete_if(&mut self, key: &str, expected: &[u8]) -> Result<bool> {
        let Some((value, cas)) = self.gets(key).await? else {
            return Ok(false);
        };

        if value != expected {
            return Ok(false);
        }

        // `-1` rather than `0`: zero means "never expire".
        Ok(self.compare_and_swap(key, b"", -1, cas).await? == Stored::Stored)
    }

    /// `delete` — `false` if the key was not there.
    pub async fn delete(&mut self, key: &str) -> Result<bool> {
        self.write_all(format!("delete {key}\r\n").as_bytes()).await?;

        match self.read_line().await?.as_str() {
            "DELETED" => Ok(true),
            // Deleting an absent key is not a failure.
            "NOT_FOUND" => Ok(false),
            other => Err(protocol_error(other)),
        }
    }

    /// `incr` / `decr` — `None` if the key does not exist.
    ///
    /// Memcached's counters are **unsigned and saturating at zero**, and they
    /// refuse to create a key. Both are why the cache layer seeds the key first
    /// rather than relying on this alone.
    pub async fn increment(&mut self, key: &str, by: u64, up: bool) -> Result<Option<u64>> {
        let verb = if up { "incr" } else { "decr" };
        self.write_all(format!("{verb} {key} {by}\r\n").as_bytes()).await?;

        let reply = self.read_line().await?;
        match reply.as_str() {
            "NOT_FOUND" => Ok(None),
            value => value.trim().parse().map(Some).map_err(|_| protocol_error(value)),
        }
    }

    /// `flush_all` — invalidate everything on the server.
    pub async fn flush_all(&mut self) -> Result<()> {
        self.write_all(b"flush_all\r\n").await?;

        match self.read_line().await?.as_str() {
            "OK" => Ok(()),
            other => Err(protocol_error(other)),
        }
    }

    /// `version` — a liveness check.
    pub async fn version(&mut self) -> Result<String> {
        self.write_all(b"version\r\n").await?;

        let reply = self.read_line().await?;
        reply
            .strip_prefix("VERSION ")
            .map(|version| version.trim().to_string())
            .ok_or_else(|| protocol_error(&reply))
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.get_mut().write_all(bytes).await.map_err(|e| unreachable_error("write", e))?;
        self.stream.get_mut().flush().await.map_err(|e| unreachable_error("flush", e))
    }

    /// One CRLF-terminated line, with the terminator stripped.
    async fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let read =
            self.stream.read_line(&mut line).await.map_err(|e| unreachable_error("read", e))?;

        if read == 0 {
            return Err(unreachable_message("the server closed the connection"));
        }

        let line = line.trim_end_matches(['\r', '\n']).to_string();

        // The server's own error replies, surfaced as errors rather than being
        // mistaken for data.
        match line.as_str() {
            "ERROR" => Err(protocol_error("the server did not recognise the command")),
            other if other.starts_with("CLIENT_ERROR") || other.starts_with("SERVER_ERROR") => {
                Err(protocol_error(other))
            }
            _ => Ok(line),
        }
    }
}

/// Opens connections to Memcached.
///
/// Unlike [`RedisConnection`](crate::RedisConnection), a Memcached connection
/// does **not** multiplex: the text protocol has no request ids, so replies are
/// matched to requests by order and one connection serves one command at a
/// time. This connector therefore keeps a small pool and lends one out per
/// operation.
#[derive(Clone)]
pub struct MemcachedConnector {
    address: String,
    pool: Arc<Pool>,
}

struct Pool {
    idle: Mutex<Vec<MemcachedConnection>>,
    limit: usize,
}

impl MemcachedConnector {
    /// Open a connector to `host:port`, or `tcp://host:port`.
    ///
    /// Nothing connects until the first operation, so this cannot fail on a
    /// server being down — deliberately: a cache that is briefly unreachable
    /// should not stop an application booting.
    pub fn open(url: impl Into<String>) -> Self {
        Self::with_pool_size(url, 8)
    }

    /// The same, with an explicit maximum number of pooled connections.
    ///
    /// The pool is a **cap on reuse, not on concurrency**: past the limit,
    /// connections are still opened and simply dropped rather than returned.
    /// That keeps a burst from queueing behind a lock, at the cost of some
    /// churn — the right trade for a cache.
    pub fn with_pool_size(url: impl Into<String>, limit: usize) -> Self {
        let url = url.into();
        let address = url
            .strip_prefix("tcp://")
            .or_else(|| url.strip_prefix("memcached://"))
            .unwrap_or(&url)
            .trim_end_matches('/')
            .to_string();

        Self { address, pool: Arc::new(Pool { idle: Mutex::new(Vec::new()), limit: limit.max(1) }) }
    }

    /// A label for diagnostics.
    pub fn description(&self) -> &str {
        "memcached"
    }

    /// The address, host and port only.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Take a connection, opening one if none is idle.
    pub async fn acquire(&self) -> Result<MemcachedGuard<'_>> {
        if let Some(connection) = self.pool.idle.lock().await.pop() {
            return Ok(MemcachedGuard { connection: Some(connection), connector: self });
        }

        let connection = MemcachedConnection::connect(&self.address).await?;
        Ok(MemcachedGuard { connection: Some(connection), connector: self })
    }

    /// Whether the server answers.
    pub async fn ping(&self) -> Result<bool> {
        let mut guard = self.acquire().await?;
        guard.connection().version().await?;
        Ok(true)
    }

    async fn release(&self, connection: MemcachedConnection) {
        let mut idle = self.pool.idle.lock().await;
        if idle.len() < self.pool.limit {
            idle.push(connection);
        }
        // Otherwise it drops and the socket closes, which is what keeps a burst
        // from leaving hundreds of idle connections behind.
    }
}

impl std::fmt::Debug for MemcachedConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the URL: it may carry credentials.
        f.debug_struct("MemcachedConnector").field("pool_limit", &self.pool.limit).finish()
    }
}

/// A borrowed connection, returned to the pool on drop.
#[derive(Debug)]
pub struct MemcachedGuard<'a> {
    connection: Option<MemcachedConnection>,
    connector: &'a MemcachedConnector,
}

impl MemcachedGuard<'_> {
    /// The connection.
    pub fn connection(&mut self) -> &mut MemcachedConnection {
        self.connection.as_mut().expect("the guard holds its connection until dropped")
    }

    /// Drop the connection instead of pooling it.
    ///
    /// **Call this after any error.** A connection that failed mid-command may
    /// have unread bytes waiting on it, and the next borrower would read them
    /// as its own reply — which is worse than a closed socket, because it
    /// silently returns one key's value for another.
    pub fn discard(mut self) {
        self.connection = None;
    }
}

impl Drop for MemcachedGuard<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let connector = self.connector.clone();
            // `Drop` cannot await, so the return is deferred. With no runtime
            // (shutdown) there is no handle and the connection simply closes,
            // which is correct.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { connector.release(connection).await });
            }
        }
    }
}

/// Memcached's largest expiry expressed as a relative number of seconds.
///
/// Beyond this, the field is read as an **absolute Unix timestamp** — so a
/// 40-day TTL sent as-is means "expired in 1970" and the value vanishes
/// immediately.
pub const MAX_RELATIVE_TTL: u64 = 60 * 60 * 24 * 30;

/// Convert a duration to Memcached's expiry field.
///
/// `0` means never. A sub-second duration becomes `1` rather than `0`, because
/// `0` would mean the opposite of what was asked for.
pub fn expiry_seconds(ttl: Option<Duration>) -> u32 {
    match ttl {
        None => 0,
        Some(ttl) => match ttl.as_secs() {
            0 => 1,
            seconds => seconds.min(MAX_RELATIVE_TTL) as u32,
        },
    }
}

/// Whether a key is one Memcached can store.
///
/// Checked here rather than left to fail at the protocol level, because a key
/// containing a space or a newline would **corrupt the connection** rather than
/// producing a clean error: the server would read the remainder as a second
/// command.
pub fn check_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::internal("a Memcached key must not be empty"));
    }
    if key.len() > 250 {
        return Err(Error::internal(format!(
            "Memcached keys are limited to 250 bytes; this one is {}",
            key.len()
        )));
    }
    if key.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
        return Err(Error::internal(
            "a Memcached key must not contain spaces or control characters",
        ));
    }
    Ok(())
}

fn unreachable_error(operation: &str, error: std::io::Error) -> Error {
    unreachable_message(&format!("{operation} failed: {error}"))
}

/// `ServiceUnavailable`, not `Internal`: an unreachable cache is a dependency
/// outage — retryable, somebody's to page about, and not a bug in the request.
fn unreachable_message(detail: &str) -> Error {
    Error::new(ErrorKind::ServiceUnavailable, format!("Memcached: {detail}"))
}

fn protocol_error(reply: &str) -> Error {
    Error::new(
        ErrorKind::ServiceUnavailable,
        format!("Memcached returned an unexpected reply: {reply}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_scheme_is_optional_and_stripped() {
        for url in ["127.0.0.1:11211", "tcp://127.0.0.1:11211", "memcached://127.0.0.1:11211/"] {
            assert_eq!(MemcachedConnector::open(url).address(), "127.0.0.1:11211", "{url}");
        }
    }

    #[test]
    fn a_connector_opens_without_touching_the_network() {
        // A cache being down must not stop a boot.
        assert_eq!(MemcachedConnector::open("127.0.0.1:1").description(), "memcached");
    }

    #[test]
    fn the_pool_size_is_at_least_one() {
        let connector = MemcachedConnector::with_pool_size("127.0.0.1:1", 0);
        assert_eq!(connector.pool.limit, 1, "a pool of zero would never lend a connection");
    }

    #[test]
    fn debug_does_not_disclose_the_url() {
        let connector = MemcachedConnector::open("tcp://user:hunter2@127.0.0.1:11211");
        assert!(!format!("{connector:?}").contains("hunter2"));
    }

    #[test]
    fn no_ttl_means_never_expire() {
        assert_eq!(expiry_seconds(None), 0);
    }

    #[test]
    fn a_sub_second_ttl_becomes_one_second_not_never() {
        assert_eq!(expiry_seconds(Some(Duration::from_millis(500))), 1);
    }

    #[test]
    fn a_long_ttl_is_clamped_below_the_timestamp_threshold() {
        let forty_days = Duration::from_secs(60 * 60 * 24 * 40);
        assert_eq!(expiry_seconds(Some(forty_days)), MAX_RELATIVE_TTL as u32);
    }

    #[test]
    fn an_ordinary_ttl_passes_through_in_seconds() {
        assert_eq!(expiry_seconds(Some(Duration::from_secs(300))), 300);
    }

    #[test]
    fn a_key_that_would_corrupt_the_connection_is_refused() {
        for bad in ["", "has space", "has\nnewline", "has\ttab", "has\rreturn"] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
        assert!(check_key("app:user:1").is_ok());
    }

    #[test]
    fn an_over_long_key_is_refused_with_its_length() {
        let err = check_key(&"k".repeat(251)).unwrap_err();

        assert!(err.message().contains("250"), "{}", err.message());
        assert!(err.message().contains("251"), "{}", err.message());
    }

    #[tokio::test]
    async fn connecting_to_nothing_is_a_503() {
        let err = MemcachedConnection::connect("127.0.0.1:1").await.unwrap_err();
        assert_eq!(err.status(), 503, "{}", err.message());
    }

    #[tokio::test]
    async fn acquiring_from_nothing_is_a_503() {
        let err = MemcachedConnector::open("127.0.0.1:1").acquire().await.unwrap_err();
        assert_eq!(err.status(), 503);
    }

    // --- the protocol, against a stub server -------------------------------

    /// A one-shot server that replies with `script` and records what it read.
    async fn stub(script: &'static [u8]) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read whatever the client sends first, then reply.
            let mut buffer = vec![0u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap();
            buffer.truncate(read);
            socket.write_all(script).await.unwrap();
            socket.flush().await.unwrap();
            buffer
        });

        (address, handle)
    }

    #[tokio::test]
    async fn get_reads_a_value_by_length() {
        // A value containing a CRLF: reading by line would truncate it, which
        // is why the body is read by its declared length.
        // "ab\r\ncd" is six bytes; the declared length must match or the reader
        // eats into the terminator.
        let (address, server) = stub(b"VALUE k 0 6\r\nab\r\ncd\r\nEND\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert_eq!(connection.get("k").await.unwrap(), Some(b"ab\r\ncd".to_vec()));
        assert_eq!(server.await.unwrap(), b"get k\r\n");
    }

    #[tokio::test]
    async fn get_reads_a_miss_as_none() {
        let (address, _server) = stub(b"END\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert_eq!(connection.get("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_sends_the_length_and_the_body() {
        let (address, server) = stub(b"STORED\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        connection.set("k", b"hello", 300).await.unwrap();

        assert_eq!(server.await.unwrap(), b"set k 0 300 5\r\nhello\r\n");
    }

    #[tokio::test]
    async fn add_reports_a_collision_rather_than_failing() {
        let (address, _server) = stub(b"NOT_STORED\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert_eq!(connection.add("k", b"v", 0).await.unwrap(), Stored::NotStored);
    }

    #[tokio::test]
    async fn delete_reads_not_found_as_false() {
        let (address, _server) = stub(b"NOT_FOUND\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert!(!connection.delete("k").await.unwrap());
    }

    #[tokio::test]
    async fn increment_reads_the_new_value() {
        let (address, server) = stub(b"7\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert_eq!(connection.increment("k", 3, true).await.unwrap(), Some(7));
        assert_eq!(server.await.unwrap(), b"incr k 3\r\n");
    }

    #[tokio::test]
    async fn decrement_uses_the_decr_verb() {
        let (address, server) = stub(b"1\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        connection.increment("k", 2, false).await.unwrap();
        assert_eq!(server.await.unwrap(), b"decr k 2\r\n");
    }

    #[tokio::test]
    async fn increment_reads_a_missing_key_as_none() {
        let (address, _server) = stub(b"NOT_FOUND\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert_eq!(connection.increment("k", 1, true).await.unwrap(), None);
    }

    #[tokio::test]
    async fn version_strips_its_prefix() {
        let (address, _server) = stub(b"VERSION 1.6.21\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert_eq!(connection.version().await.unwrap(), "1.6.21");
    }

    #[tokio::test]
    async fn a_server_error_is_surfaced_rather_than_read_as_data() {
        let (address, _server) = stub(b"SERVER_ERROR out of memory\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        let err = connection.get("k").await.unwrap_err();
        assert!(err.message().contains("out of memory"), "{}", err.message());
    }

    #[tokio::test]
    async fn an_unrecognised_command_is_an_error() {
        let (address, _server) = stub(b"ERROR\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        assert!(connection.flush_all().await.is_err());
    }

    #[tokio::test]
    async fn a_closed_connection_says_so() {
        let (address, _server) = stub(b"").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        let err = connection.get("k").await.unwrap_err();
        assert!(err.message().contains("closed"), "{}", err.message());
    }

    #[tokio::test]
    async fn flush_all_expects_ok() {
        let (address, server) = stub(b"OK\r\n").await;
        let mut connection = MemcachedConnection::connect(&address).await.unwrap();

        connection.flush_all().await.unwrap();
        assert_eq!(server.await.unwrap(), b"flush_all\r\n");
    }
}
