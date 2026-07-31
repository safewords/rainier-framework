//! [`SessionManager`] — the store and its settings, as one container-storable
//! value.

use std::sync::Arc;

use chrono::Duration;
use rainier_http::SameSite;
use rainier_support::Result;

use crate::session::SessionData;
use crate::store::SessionStore;

/// How the session cookie is written.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The cookie's name.
    pub cookie: String,
    /// Its path.
    pub path: String,
    /// Its domain, or `None` for the host that set it.
    pub domain: Option<String>,
    /// Whether it is `Secure` — set this in production.
    pub secure: bool,
    /// Its `SameSite` policy.
    pub same_site: SameSite,
    /// How long a session lives without being touched.
    pub lifetime: Duration,
}

impl SessionConfig {
    /// Name the cookie.
    pub fn cookie(mut self, name: impl Into<String>) -> Self {
        self.cookie = name.into();
        self
    }

    /// Set the cookie's path.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Scope the cookie to a domain.
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Send the cookie only over HTTPS.
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Set the `SameSite` policy.
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = same_site;
        self
    }

    /// How long a session lives.
    pub fn lifetime(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }
}

/// The application's session store and settings.
///
/// This is what the container holds and what the `Session` facade resolves.
///
/// **It is not a request's session.** A facade is process-global and a session
/// belongs to one request, so there is nothing honest for
/// `Session::instance().get("user_id")` to return. Per-request access is
/// [`request.session()`](crate::SessionRequestExt::session); this type is for
/// the operations that genuinely are application-wide — reading or destroying
/// a session by id, and collecting expired ones.
#[derive(Clone)]
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    config: SessionConfig,
}

impl SessionManager {
    /// A manager over `store`, with the default cookie settings.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store, config: SessionConfig::default() }
    }

    /// A manager with explicit settings.
    pub fn with_config(store: Arc<dyn SessionStore>, config: SessionConfig) -> Self {
        Self { store, config }
    }

    /// Adjust the settings.
    pub fn configure(mut self, adjust: impl FnOnce(SessionConfig) -> SessionConfig) -> Self {
        self.config = adjust(self.config);
        self
    }

    /// The store.
    pub fn store(&self) -> &Arc<dyn SessionStore> {
        &self.store
    }

    /// The cookie settings.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The driver's name — `"memory"`, `"database"`.
    pub fn driver(&self) -> &str {
        self.store.name()
    }

    /// Read a session by id, without a request.
    ///
    /// For an administrative view: "what is in this user's session".
    pub async fn read(&self, id: &str) -> Result<Option<SessionData>> {
        self.store.read(id).await
    }

    /// Destroy a session by id.
    ///
    /// The other half of "log this device out", and the thing to call for
    /// every one of a user's sessions when their password changes.
    pub async fn destroy(&self, id: &str) -> Result<()> {
        self.store.destroy(id).await
    }

    /// Discard every expired session. Returns how many.
    pub async fn gc(&self) -> Result<u64> {
        self.store.gc().await
    }
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("driver", &self.store.name())
            .field("cookie", &self.config.cookie)
            .field("lifetime", &self.config.lifetime)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemorySessionStore;

    fn manager() -> SessionManager {
        SessionManager::new(Arc::new(MemorySessionStore::default()))
    }

    #[test]
    fn the_defaults_are_the_safe_ones() {
        let config = SessionConfig::default();

        assert_eq!(config.same_site, SameSite::Lax);
        assert_eq!(config.path, "/");
        assert!(!config.secure, "off by default so http://localhost works");
    }

    #[test]
    fn settings_are_configurable() {
        let manager = manager().configure(|config| {
            config
                .cookie("my_app_session")
                .secure(true)
                .same_site(SameSite::Strict)
                .lifetime(Duration::days(14))
                .domain("example.com")
        });

        assert_eq!(manager.config().cookie, "my_app_session");
        assert!(manager.config().secure);
        assert_eq!(manager.config().same_site, SameSite::Strict);
        assert_eq!(manager.config().lifetime, Duration::days(14));
        assert_eq!(manager.config().domain.as_deref(), Some("example.com"));
    }

    #[tokio::test]
    async fn a_session_can_be_read_and_destroyed_without_a_request() {
        let manager = manager();
        let mut values = serde_json::Map::new();
        values.insert("user_id".into(), 42.into());
        let data = SessionData { values, flash: Vec::new() };

        manager.store().write("abc", &data).await.unwrap();
        assert!(manager.read("abc").await.unwrap().is_some());

        manager.destroy("abc").await.unwrap();
        assert!(manager.read("abc").await.unwrap().is_none());
    }

    #[test]
    fn the_driver_is_reported() {
        assert_eq!(manager().driver(), "memory");
    }
}
