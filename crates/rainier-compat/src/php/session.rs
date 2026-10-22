//! Sessions a PHP application started — read through, migrated on first
//! touch.
//!
//! ```ignore
//! let cookies = EncryptedCookies::new(keys.clone());
//! let source = Arc::new(CacheSessionSource::new(PhpCache::new(redis, "app_cache_")));
//!
//! let store = Arc::new(ForeignSessionStore::new(
//!     Arc::new(redis_session_store),  // where Rainier's own sessions live
//!     source,
//!     cookies.clone(),
//!     "app_session",                 // the PHP application's session cookie
//!     &keys,
//! )?);
//!
//! // Both applications stay signed in: Rainier gets its own cookie, the PHP
//! // one is left alone.
//! web.push(AdoptForeignSession::new("app_session", "rainier_session"))
//!    .push(StartSession::with_config(store, config.cookie("rainier_session")));
//! ```
//!
//! # Two ways to run beside a PHP application
//!
//! | | Rainier's cookie name | The PHP application afterwards |
//! |---|---|---|
//! | **Coexist** | its own, with [`AdoptForeignSession`] in front | still signed in — its cookie is untouched |
//! | **Take over** | the PHP application's | signed out on its next request: the cookie now holds a Rainier id |
//!
//! Coexisting is the default worth wanting: routes move one at a time and a
//! user crossing between them notices nothing. Taking over is for the day the
//! PHP application stops answering.
//!
//! # What happens on the first request
//!
//! 1. The PHP session cookie is decrypted and its name binding verified
//!    ([`EncryptedCookies`]), giving the PHP session id.
//! 2. The payload is loaded from wherever the PHP side keeps it
//!    ([`SessionSource`]) and unserialized ([`super::serialize`]) — data only,
//!    nothing instantiated.
//! 3. It is mapped ([`SessionMapping`]) into Rainier's shape: the anti-forgery
//!    token, the signed-in user, flash data, and every other value that has a
//!    lossless JSON form.
//! 4. It is written to Rainier's own store under an id **derived** from the
//!    PHP one with the application key, and the response sets Rainier's cookie
//!    to it.
//!
//! The derivation is what makes concurrent first requests safe: a page that
//! fires four requests at once migrates the session four times into the
//! **same** Rainier session rather than four different ones, and a request
//! that still carries only the PHP cookie after the migration finds the
//! migrated session rather than the stale original.
//!
//! # What does not come across
//!
//! A value with no lossless JSON form — an object, such as the validation
//! error bag a redirect-with-errors stores — is skipped and named in a debug
//! log line. Those are request-to-request leftovers, not state anybody loses
//! sleep over; the signed-in user and the token are plain values and always
//! come across.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use rainier_crypt::KeyRing;
use rainier_http::{Request, Response};
use rainier_middleware::{Middleware, Next};
use rainier_session::{SessionData, SessionStore, TOKEN_KEY};
use rainier_support::{BoxFuture, Error, Result};
use serde_json::Value as Json;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::cache::PhpCache;
use super::cookie::EncryptedCookies;
use super::serialize::{unserialize, Value};

type HmacSha256 = Hmac<Sha256>;

/// The session key Rainier's session guard reads the signed-in user from —
/// `rainier_auth::AUTH_KEY`, restated so this crate need not depend on the
/// whole of `rainier-auth` for one string. A test pins the two together.
pub const RAINIER_AUTH_KEY: &str = "_auth_id";

/// Marks an id that names a PHP session rather than a Rainier one.
const FOREIGN: &str = "php:";

/// Whether `id` is shaped like a PHP session id: forty ASCII alphanumerics.
///
/// Checked before an id goes anywhere near a store or a path — it came out of
/// a cookie.
pub fn is_session_id(id: &str) -> bool {
    id.len() == 40 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

// --- the payload ---------------------------------------------------------------

/// How a PHP session payload becomes Rainier session data.
#[derive(Debug, Clone)]
pub struct SessionMapping {
    /// The guard whose signed-in user is carried across — `web` by default.
    pub guard: String,
    /// Where Rainier's guard looks for the user id.
    pub auth_key: String,
}

impl Default for SessionMapping {
    fn default() -> Self {
        Self { guard: "web".into(), auth_key: RAINIER_AUTH_KEY.into() }
    }
}

/// What a mapping produced.
#[derive(Debug, Clone, Default)]
pub struct Mapped {
    /// The session, in Rainier's shape.
    pub data: SessionData,
    /// The signed-in user's id, if there was one.
    pub user_id: Option<String>,
    /// Keys left behind because their values have no lossless JSON form.
    pub skipped: Vec<String>,
}

impl SessionMapping {
    /// Map `guard`'s user instead of `web`'s.
    pub fn guard(mut self, guard: impl Into<String>) -> Self {
        self.guard = guard.into();
        self
    }

    /// Whether `key` is the session guard key a PHP application writes for
    /// this guard: `login_{guard}_` and the forty-hex-digit SHA-1 of its guard
    /// class. Matched by shape, so the class name need not be configured.
    pub fn is_guard_key(&self, key: &str) -> bool {
        key.strip_prefix("login_")
            .and_then(|rest| rest.strip_prefix(self.guard.as_str()))
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    /// Map an unserialized payload.
    pub fn map(&self, payload: &Value) -> Result<Mapped> {
        let entries = payload
            .as_array()
            .ok_or_else(|| Error::bad_request("a session payload is an array"))?;
        let mut mapped = Mapped::default();

        for (key, value) in entries {
            let Some(name) = key.to_text() else {
                mapped.skipped.push(key.to_string());
                continue;
            };

            if self.is_guard_key(&name) {
                // An integer or string id. Rainier keeps it as text, which is
                // what `auth_identifier` produces.
                let id = match value {
                    Value::Int(n) => Some(n.to_string()),
                    Value::String(_) => value.as_str().map(str::to_string),
                    _ => None,
                };
                if let Some(id) = id {
                    mapped.data.values.insert(self.auth_key.clone(), Json::String(id.clone()));
                    mapped.user_id = Some(id);
                }
                continue;
            }

            if name == "_flash" {
                // The PHP side ages flash data when it saves: what the last
                // request flashed is under `old`, readable now and gone after.
                // Anything still under `new` was flashed and never aged, which
                // only a crashed request leaves; it is readable now too.
                for list in ["old", "new"] {
                    if let Some(keys) = value.get(list).and_then(Value::as_array) {
                        for (_, flashed) in keys {
                            if let Some(flashed) = flashed.as_str() {
                                if !mapped.data.flash.iter().any(|k| k == flashed) {
                                    mapped.data.flash.push(flashed.to_string());
                                }
                            }
                        }
                    }
                }
                continue;
            }

            match value.to_json() {
                Some(json) => {
                    // `_token` is the anti-forgery token under the same name
                    // in both; it is copied like any other string.
                    mapped.data.values.insert(name, json);
                }
                None => mapped.skipped.push(name),
            }
        }

        debug_assert_eq!(TOKEN_KEY, "_token");
        Ok(mapped)
    }

    /// Unserialize and map raw payload bytes.
    pub fn map_bytes(&self, payload: &[u8]) -> Result<Mapped> {
        self.map(&unserialize(payload)?)
    }
}

/// The payload of a session row in a database table, which the PHP side
/// stores base64-encoded.
pub fn database_payload(encoded: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| Error::bad_request("a session row's payload is base64"))
}

// --- where payloads live --------------------------------------------------------

/// Where the PHP application keeps session payloads.
///
/// Implement it over whatever the PHP side was configured with. Provided: the
/// cache ([`CacheSessionSource`], which is what a Redis or Memcached session
/// driver uses), files ([`FileSessionSource`]) and memory
/// ([`MemorySessionSource`]). For a database table, query the `payload`
/// column by `id` and hand it to [`database_payload`].
pub trait SessionSource: Send + Sync + 'static {
    /// The serialized attributes for session `id` — already a valid PHP
    /// session id — or `None`.
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>>;

    /// Delete session `id`. Only called when the store is told to release
    /// migrated sessions; the default refuses, so a source that cannot delete
    /// says so instead of pretending.
    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        let _ = id;
        Box::pin(async { Err(Error::internal("this session source cannot delete sessions")) })
    }
}

/// Sessions in the PHP application's cache.
///
/// The cache stores the payload — itself a serialized string — serialized
/// again, at the session id under the cache's prefixes.
#[derive(Debug, Clone)]
pub struct CacheSessionSource {
    cache: PhpCache,
}

impl CacheSessionSource {
    /// Read sessions from `cache`.
    pub fn new(cache: PhpCache) -> Self {
        Self { cache }
    }
}

impl SessionSource for CacheSessionSource {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            match self.cache.get(id).await? {
                Some(Value::String(payload)) => Ok(Some(payload)),
                Some(_) => Err(Error::bad_request("a cached session is a serialized string")),
                None => Ok(None),
            }
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.cache.forget(id).await.map(|_| ()) })
    }
}

/// Sessions in files, one per id, as the file driver writes them.
#[derive(Debug, Clone)]
pub struct FileSessionSource {
    directory: PathBuf,
}

impl FileSessionSource {
    /// Read session files from `directory` — the PHP application's session
    /// storage path, mounted where this process can see it.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }
}

impl SessionSource for FileSessionSource {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            // The id is checked by the store before it gets here; checked
            // again because this one becomes a path.
            if !is_session_id(id) {
                return Ok(None);
            }
            match std::fs::read(self.directory.join(id)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(Error::internal(format!("could not read a session file: {e}"))),
            }
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !is_session_id(id) {
                return Ok(());
            }
            match std::fs::remove_file(self.directory.join(id)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::internal(format!("could not delete a session file: {e}"))),
            }
        })
    }
}

/// Sessions held in memory — for tests, and for payloads fetched some other
/// way and handed over.
#[derive(Debug, Default)]
pub struct MemorySessionSource {
    sessions: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySessionSource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold `payload` as session `id`.
    pub fn insert(&self, id: impl Into<String>, payload: impl Into<Vec<u8>>) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.into(), payload.into());
    }

    /// Whether session `id` is held.
    pub fn contains(&self, id: &str) -> bool {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).contains_key(id)
    }
}

impl SessionSource for MemorySessionSource {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            Ok(self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned())
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
            Ok(())
        })
    }
}

// --- the store -------------------------------------------------------------------

/// What happens to the PHP session once it has been migrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Leave it, so the PHP application keeps the user signed in. The default.
    Keep,
    /// Delete it when Rainier's copy is regenerated or destroyed — at login,
    /// logout, or invalidation — so the two cannot drift apart afterwards.
    Destroy,
}

/// A [`SessionStore`] that reads PHP sessions through and keeps Rainier's own
/// in `inner`.
pub struct ForeignSessionStore {
    inner: Arc<dyn SessionStore>,
    source: Arc<dyn SessionSource>,
    cookies: EncryptedCookies,
    cookie_name: String,
    derivation: [u8; 32],
    mapping: SessionMapping,
    release: Release,
}

impl ForeignSessionStore {
    /// Rainier sessions in `inner`; PHP sessions from `source`, named by the
    /// encrypted cookie `cookie_name` and sealed with `cookies`. `keys` derives
    /// the migrated sessions' ids.
    ///
    /// # Errors
    ///
    /// If `inner` keeps sessions in the cookie: a migrated session has to be
    /// **stored** under its derived id, or two concurrent first requests would
    /// produce two sessions.
    pub fn new(
        inner: Arc<dyn SessionStore>,
        source: Arc<dyn SessionSource>,
        cookies: EncryptedCookies,
        cookie_name: impl Into<String>,
        keys: &KeyRing,
    ) -> Result<Self> {
        if inner.is_client_side() {
            return Err(Error::internal(
                "a foreign session store needs a server-side store underneath; a cookie store \
                 cannot hold a migrated session under a derived id",
            ));
        }
        Ok(Self {
            inner,
            source,
            cookies,
            cookie_name: cookie_name.into(),
            derivation: *keys.current().bytes(),
            mapping: SessionMapping::default(),
            release: Release::Keep,
        })
    }

    /// Map with these settings.
    #[must_use]
    pub fn mapping(mut self, mapping: SessionMapping) -> Self {
        self.mapping = mapping;
        self
    }

    /// What to do with a PHP session after it is migrated.
    #[must_use]
    pub fn release(mut self, release: Release) -> Self {
        self.release = release;
        self
    }

    /// The Rainier id a PHP session migrates to: an HMAC of the PHP id under
    /// the application key, shaped like any id Rainier mints.
    pub fn derived_id(&self, php_id: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.derivation).expect("HMAC takes a key of any length");
        mac.update(b"rainier-compat|php-session|");
        mac.update(php_id.as_bytes());
        mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn foreign(id: &str) -> Option<&str> {
        id.strip_prefix(FOREIGN)
    }

    async fn read_foreign(&self, php_id: &str) -> Result<Option<SessionData>> {
        // Migrated already — by a concurrent request, or one whose response
        // never reached the browser — then that copy is the truth.
        if let Some(data) = self.inner.read(&self.derived_id(php_id)).await? {
            return Ok(Some(data));
        }

        let Some(payload) = self.source.load(php_id).await? else {
            return Ok(None);
        };
        let mapped = self.mapping.map_bytes(&payload)?;
        if !mapped.skipped.is_empty() {
            tracing::debug!(skipped = ?mapped.skipped, "session values with no JSON form were not migrated");
        }
        Ok(Some(mapped.data))
    }
}

impl SessionStore for ForeignSessionStore {
    fn name(&self) -> &str {
        "php-session"
    }

    fn read<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>>> {
        Box::pin(async move {
            match Self::foreign(id) {
                Some(php_id) => self.read_foreign(php_id).await,
                None => self.inner.read(id).await,
            }
        })
    }

    fn write<'a>(&'a self, id: &'a str, data: &'a SessionData) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match Self::foreign(id) {
                Some(php_id) => self.inner.write(&self.derived_id(php_id), data).await,
                None => self.inner.write(id, data).await,
            }
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match Self::foreign(id) {
                Some(php_id) => {
                    self.inner.destroy(&self.derived_id(php_id)).await?;
                    if self.release == Release::Destroy {
                        self.source.destroy(php_id).await?;
                    }
                    Ok(())
                }
                None => self.inner.destroy(id).await,
            }
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<u64>> {
        self.inner.gc()
    }

    fn encode(&self, id: &str, data: &SessionData) -> Result<String> {
        match Self::foreign(id) {
            // The cookie moves to the migrated session.
            Some(php_id) => Ok(self.derived_id(php_id)),
            None => self.inner.encode(id, data),
        }
    }

    fn decode(&self, value: &str) -> Result<(String, Option<SessionData>)> {
        if let Ok(decoded) = self.inner.decode(value) {
            return Ok(decoded);
        }
        let php_id = self.cookies.decrypt(&self.cookie_name, value)?;
        if !is_session_id(&php_id) {
            return Err(Error::bad_request("that is not a session id the PHP application issued"));
        }
        Ok((format!("{FOREIGN}{php_id}"), None))
    }
}

impl std::fmt::Debug for ForeignSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignSessionStore")
            .field("inner", &self.inner.name())
            .field("cookie", &self.cookie_name)
            .field("release", &self.release)
            .finish_non_exhaustive()
    }
}

// --- coexisting -------------------------------------------------------------------

/// Hands the PHP application's session cookie to Rainier's session middleware
/// when the request has no Rainier session yet.
///
/// Place it **immediately before** `StartSession`, which must be built over a
/// [`ForeignSessionStore`]. It adds `rainier_cookie=<the PHP cookie's value>`
/// to the request's `Cookie` header; the store recognises the PHP format and
/// migrates. The PHP cookie itself is never touched, so the PHP application
/// keeps its session.
///
/// It reads the header itself rather than the request's parsed cookies,
/// because parsed cookies are cached on first read — a middleware between
/// this one and `StartSession` that reads a cookie is harmless, one *before*
/// this one would freeze the cookie list without the addition.
#[derive(Debug, Clone)]
pub struct AdoptForeignSession {
    foreign_cookie: String,
    rainier_cookie: String,
}

impl AdoptForeignSession {
    /// Adopt the PHP cookie `foreign_cookie` as `rainier_cookie`.
    pub fn new(foreign_cookie: impl Into<String>, rainier_cookie: impl Into<String>) -> Self {
        Self { foreign_cookie: foreign_cookie.into(), rainier_cookie: rainier_cookie.into() }
    }
}

#[async_trait]
impl Middleware for AdoptForeignSession {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        let header = request.header("cookie").unwrap_or_default().to_string();
        let cookies = rainier_http::cookie::parse_cookie_header(&header);

        if !cookies.contains_key(&self.rainier_cookie) {
            if let Some(value) = cookies.get(&self.foreign_cookie) {
                // Percent-encoded the way a browser would send it; the cookie
                // parser decodes it again on the other side.
                let encoded: String = value
                    .bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                            (b as char).to_string()
                        }
                        other => format!("%{other:02X}"),
                    })
                    .collect();
                let joined = if header.trim().is_empty() {
                    format!("{}={encoded}", self.rainier_cookie)
                } else {
                    format!("{header}; {}={encoded}", self.rainier_cookie)
                };
                if let Ok(value) = joined.parse() {
                    request.headers_mut().insert("cookie", value);
                }
            }
        }

        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "AdoptForeignSession"
    }
}

// --- remember-me -----------------------------------------------------------------

/// What a remember-me cookie claims: `id|token|hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recaller {
    /// The user's id.
    pub id: String,
    /// The remember token, compared against the user's stored one.
    pub token: String,
    /// A MAC of the user's password hash, so changing the password kills the
    /// cookie.
    pub hash: String,
}

impl Recaller {
    /// Split a decrypted value. `None` unless all three parts are present and
    /// non-empty; anything after a fourth `|` is ignored, as the PHP side
    /// ignores it.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split('|');
        let (id, token, hash) = (parts.next()?, parts.next()?, parts.next()?);
        if id.is_empty() || token.is_empty() || hash.is_empty() {
            return None;
        }
        Some(Self { id: id.into(), token: token.into(), hash: hash.into() })
    }
}

/// Looks up the user a remember-me cookie names.
#[async_trait]
pub trait RememberedUsers: Send + Sync + 'static {
    /// The password hash of the user with this `id` **and** this remember
    /// token — `None` if either does not match. Compare the token in constant
    /// time, as the PHP side does.
    async fn password_hash_for(&self, id: &str, token: &str) -> Result<Option<String>>;
}

/// Reads remember-me cookies a PHP application set.
pub struct RememberCookies {
    cookies: EncryptedCookies,
    guard: String,
    hash_key: Vec<u8>,
}

impl RememberCookies {
    /// Read the `guard` remember cookie, sealed with `cookies`.
    ///
    /// `app_key` is the application key **exactly as configured**, `base64:`
    /// prefix and all: the password MAC is keyed with the configuration
    /// string, not the decoded key.
    pub fn new(cookies: EncryptedCookies, guard: impl Into<String>, app_key: &str) -> Self {
        Self { cookies, guard: guard.into(), hash_key: app_key.as_bytes().to_vec() }
    }

    /// Whether `name` is this guard's remember cookie — `remember_{guard}_`
    /// and the SHA-1 of the guard class, matched by shape.
    pub fn is_cookie_name(&self, name: &str) -> bool {
        name.strip_prefix("remember_")
            .and_then(|rest| rest.strip_prefix(self.guard.as_str()))
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    /// The MAC a remember cookie carries for `password_hash`.
    pub fn password_mac(&self, password_hash: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.hash_key).expect("HMAC takes a key of any length");
        mac.update(password_hash.as_bytes());
        mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The decrypted, parsed remember cookie on `request`, if it has one.
    pub fn recaller(&self, request: &Request) -> Option<Recaller> {
        request
            .cookies()
            .iter()
            .filter(|(name, _)| self.is_cookie_name(name))
            .find_map(|(name, value)| Recaller::parse(&self.cookies.decrypt(name, value).ok()?))
    }

    /// The id of the user `request`'s remember cookie signs in, after checking
    /// the token and the password MAC.
    pub async fn user_id(
        &self,
        request: &Request,
        users: &dyn RememberedUsers,
    ) -> Result<Option<String>> {
        let Some(recaller) = self.recaller(request) else {
            return Ok(None);
        };
        let Some(password_hash) = users.password_hash_for(&recaller.id, &recaller.token).await?
        else {
            return Ok(None);
        };

        let mac = self.password_mac(&password_hash);
        // The MAC, or — from cookies set before the MAC was introduced — the
        // hash itself.
        let valid = bool::from(mac.as_bytes().ct_eq(recaller.hash.as_bytes()))
            || bool::from(password_hash.as_bytes().ct_eq(recaller.hash.as_bytes()));
        Ok(valid.then_some(recaller.id))
    }
}

impl std::fmt::Debug for RememberCookies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RememberCookies").field("guard", &self.guard).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
