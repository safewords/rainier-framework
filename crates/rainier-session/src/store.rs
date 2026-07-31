//! Where sessions live — the [`SessionStore`] port and its implementations.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use rainier_support::{BoxFuture, Error, Result};

use crate::session::SessionData;

/// Where session data is kept between requests.
///
/// The four required methods are the server-side shape: an id in the cookie,
/// the data somewhere else. [`encode`](Self::encode) and
/// [`decode`](Self::decode) exist so a store can instead put the **whole
/// session in the cookie** — see
/// [`CookieSessionStore`](crate::CookieSessionStore) — without the middleware
/// needing to know which kind it has.
pub trait SessionStore: Send + Sync + 'static {
    /// A label for diagnostics — `"memory"`, `"database"`, `"cache"`,
    /// `"cookie"`.
    fn name(&self) -> &str;

    /// The data for this id, or `None` if it is unknown or expired.
    fn read<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>>>;

    /// Write the data back, refreshing its expiry.
    fn write<'a>(&'a self, id: &'a str, data: &'a SessionData) -> BoxFuture<'a, Result<()>>;

    /// Remove a session.
    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Discard every expired session. Called periodically, not per request.
    fn gc(&self) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    /// Whether the session travels in the cookie rather than being stored here.
    ///
    /// A client-side store's [`read`](Self::read) and [`write`](Self::write) are
    /// never called by the middleware.
    fn is_client_side(&self) -> bool {
        false
    }

    /// The cookie value carrying this session.
    ///
    /// Server-side stores return the id, which is the default.
    fn encode(&self, id: &str, data: &SessionData) -> Result<String> {
        let _ = data;
        Ok(id.to_string())
    }

    /// Recover a session id — and, for a client-side store, its data — from a
    /// cookie value.
    ///
    /// The default rejects anything that is not the shape
    /// [`generate_session_id`](crate::generate_session_id) produces. A cookie
    /// is client-supplied, so an id we did not mint is not worth a round trip,
    /// and letting a client choose its own id is how session fixation starts.
    fn decode(&self, value: &str) -> Result<(String, Option<SessionData>)> {
        if is_well_formed(value) {
            Ok((value.to_string(), None))
        } else {
            Err(Error::bad_request("that is not a session id this application issued"))
        }
    }
}

/// Whether an id looks like one [`generate_session_id`](crate::generate_session_id)
/// produced: 64 hexadecimal characters.
pub fn is_well_formed(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Sessions held in this process's memory.
///
/// Right for development, tests, and a single instance. Two instances behind a
/// load balancer will not see each other's sessions, so a user's session
/// appears to vanish and reappear as they are routed around — which looks like
/// a bug in your login code and is not.
pub struct MemorySessionStore {
    entries: Mutex<HashMap<String, Entry>>,
    lifetime: Duration,
}

#[derive(Debug, Clone)]
struct Entry {
    data: SessionData,
    expires_at: DateTime<Utc>,
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new(Duration::hours(2))
    }
}

impl MemorySessionStore {
    /// Sessions expiring after `lifetime`.
    pub fn new(lifetime: Duration) -> Self {
        Self { entries: Mutex::new(HashMap::new()), lifetime }
    }

    /// How many sessions are held, expired ones included.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether no sessions are held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SessionStore for MemorySessionStore {
    fn name(&self) -> &str {
        "memory"
    }

    fn read<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>>> {
        Box::pin(async move {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            match entries.get(id) {
                Some(entry) if entry.expires_at > Utc::now() => Ok(Some(entry.data.clone())),
                Some(_) => {
                    // Expired: drop it now rather than leave it to accumulate.
                    entries.remove(id);
                    Ok(None)
                }
                None => Ok(None),
            }
        })
    }

    fn write<'a>(&'a self, id: &'a str, data: &'a SessionData) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.entries.lock().unwrap_or_else(|e| e.into_inner()).insert(
                id.to_string(),
                Entry { data: data.clone(), expires_at: Utc::now() + self.lifetime },
            );
            Ok(())
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.entries.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
            Ok(())
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let now = Utc::now();
            let before = entries.len();
            entries.retain(|_, entry| entry.expires_at > now);
            Ok((before - entries.len()) as u64)
        })
    }
}

impl std::fmt::Debug for MemorySessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySessionStore").field("sessions", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(user: u64) -> SessionData {
        let mut values = serde_json::Map::new();
        values.insert("user_id".into(), user.into());
        SessionData { values, flash: Vec::new() }
    }

    #[tokio::test]
    async fn a_written_session_reads_back() {
        let store = MemorySessionStore::default();
        store.write("abc", &data(42)).await.unwrap();

        let read = store.read("abc").await.unwrap().expect("present");
        assert_eq!(read.values["user_id"], 42);
    }

    #[tokio::test]
    async fn an_unknown_session_reads_as_none() {
        assert!(MemorySessionStore::default().read("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn destroying_a_session_removes_it() {
        let store = MemorySessionStore::default();
        store.write("abc", &data(42)).await.unwrap();

        store.destroy("abc").await.unwrap();
        assert!(store.read("abc").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_session_reads_as_none_and_is_dropped() {
        let store = MemorySessionStore::new(Duration::seconds(-1));
        store.write("abc", &data(42)).await.unwrap();

        assert!(store.read("abc").await.unwrap().is_none());
        assert!(store.is_empty(), "reading an expired session should clean it up");
    }

    #[tokio::test]
    async fn writing_again_refreshes_the_expiry() {
        let store = MemorySessionStore::new(Duration::seconds(60));
        store.write("abc", &data(42)).await.unwrap();
        store.write("abc", &data(43)).await.unwrap();

        assert_eq!(store.read("abc").await.unwrap().unwrap().values["user_id"], 43);
    }

    #[tokio::test]
    async fn gc_removes_only_the_expired() {
        let store = MemorySessionStore::new(Duration::seconds(-1));
        store.write("a", &data(1)).await.unwrap();
        store.write("b", &data(2)).await.unwrap();

        assert_eq!(store.gc().await.unwrap(), 2);
        assert!(store.is_empty());

        let live = MemorySessionStore::default();
        live.write("a", &data(1)).await.unwrap();
        assert_eq!(live.gc().await.unwrap(), 0);
        assert_eq!(live.len(), 1);
    }

    #[tokio::test]
    async fn two_sessions_are_independent() {
        let store = MemorySessionStore::default();
        store.write("first", &data(42)).await.unwrap();
        store.write("second", &data(42)).await.unwrap();

        store.destroy("first").await.unwrap();

        assert!(store.read("first").await.unwrap().is_none());
        assert!(
            store.read("second").await.unwrap().is_some(),
            "logging out one device must not log out the others"
        );
    }
}

#[cfg(test)]
mod port_tests {
    use super::*;

    #[test]
    fn a_well_formed_id_is_sixty_four_hex_characters() {
        assert!(is_well_formed(&crate::session::generate_session_id()));
        assert!(is_well_formed(&"a".repeat(64)));
        assert!(is_well_formed(&"F".repeat(64)));
    }

    #[test]
    fn anything_else_is_not() {
        for bad in ["", "short", &"z".repeat(64), &"a".repeat(63), &"a".repeat(65), "../../etc"] {
            assert!(!is_well_formed(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_default_encoding_is_the_id_itself() {
        let store = MemorySessionStore::default();
        let id = crate::session::generate_session_id();

        assert_eq!(store.encode(&id, &SessionData::default()).unwrap(), id);
        assert!(!store.is_client_side());
    }

    #[test]
    fn the_default_decoding_refuses_a_client_chosen_id() {
        // Session fixation begins with a client picking its own id.
        let store = MemorySessionStore::default();

        assert!(store.decode("chosen-by-the-client").is_err());

        let id = crate::session::generate_session_id();
        assert_eq!(store.decode(&id).unwrap(), (id, None));
    }
}
