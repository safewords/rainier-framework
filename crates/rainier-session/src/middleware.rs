//! [`StartSession`] — the middleware that loads a session and writes it back.

use std::sync::Arc;

use rainier_http::{Cookie, Request, Response, SameSite};
use rainier_middleware::{Middleware, Next};
use rainier_support::Result;

use crate::manager::{SessionConfig, SessionManager};
use crate::session::{Session, SessionData};
use crate::store::SessionStore;

/// A loaded session, and whether it is safe to write back.
struct Loaded {
    session: Session,
    /// The store could not be read, so this session stands in for one that
    /// very likely still exists. Writing it back would replace the real one
    /// with an empty session under a new id; see `load`.
    detached: bool,
}

impl Loaded {
    /// A session that may be persisted as usual.
    fn attached(session: Session) -> Self {
        Self { session, detached: false }
    }

    /// A session that must not be persisted.
    fn detached(session: Session) -> Self {
        Self { session, detached: true }
    }
}

/// Loads the session before the handler and persists it afterwards.
///
/// A route without this middleware has no session, and
/// [`request.session()`](crate::SessionRequestExt::session) is `None` there —
/// which is the honest answer, and better than handing out a bag that silently
/// fails to persist.
pub struct StartSession {
    store: Arc<dyn SessionStore>,
    config: SessionConfig,
}

impl StartSession {
    /// Start sessions in `store`, with the default cookie settings.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store, config: SessionConfig::default() }
    }

    /// Start sessions with explicit cookie settings.
    pub fn with_config(store: Arc<dyn SessionStore>, config: SessionConfig) -> Self {
        Self { store, config }
    }

    /// Take the settings and the store from a manager — the usual way, since
    /// the manager is what the container holds.
    pub fn from_manager(manager: &SessionManager) -> Self {
        Self::with_config(Arc::clone(manager.store()), manager.config().clone())
    }

    /// The cookie carrying this session.
    ///
    /// The **value** comes from the store, not from here: a server-side store
    /// returns the id, and a client-side one returns the whole encrypted
    /// session. Keeping that decision in the store is what lets the two kinds
    /// share this middleware.
    fn cookie(&self, value: &str) -> Cookie {
        let config = &self.config;
        let mut cookie = Cookie::new(&config.cookie, value)
            .path(&config.path)
            .http_only(true)
            .secure(config.secure)
            .same_site(config.same_site)
            .max_age(config.lifetime.num_seconds());

        if let Some(domain) = &config.domain {
            cookie = cookie.domain(domain);
        }
        cookie
    }

    /// Load the session the request's cookie names, or start a fresh one.
    async fn load(&self, request: &Request) -> Loaded {
        let Some(value) = request.cookie(&self.config.cookie) else {
            return Loaded::attached(Session::new());
        };

        // The store decides what a cookie value means, and whether it is one
        // this application could have issued. A value it rejects gets a fresh
        // session rather than an error: a client cannot be trusted to send a
        // good one, and a bad one is not worth failing a page over.
        let (id, carried) = match self.store.decode(value) {
            Ok(decoded) => decoded,
            Err(_) => return Loaded::attached(Session::new()),
        };

        // A client-side store has already produced the data; there is nothing
        // to read.
        if let Some(data) = carried {
            return Loaded::attached(Session::restore(id, data));
        }

        match self.store.read(&id).await {
            Ok(Some(data)) => Loaded::attached(Session::restore(id, data)),
            // No such session. A fresh one, which may take a new id: there is
            // nothing on the other end of the old one to lose.
            Ok(None) => Loaded::attached(Session::new()),
            Err(e) => {
                // The store being unreachable must not take the request with
                // it: the page still renders. Failing here would turn a cache
                // blip into a total outage.
                //
                // But it must not take the *session* with it either, which is
                // what starting a fresh one used to do. A new session gets a
                // new id, `save` then writes it and sets it as the cookie, and
                // the real session — intact in the store, merely unreadable
                // for this one request — is orphaned. Every signed-in visitor
                // is silently logged out by a blip, and a Redis Cluster shard
                // that lost its master logged out every visitor for three
                // hours and left four thousand abandoned sessions behind.
                //
                // So the session is *detached*: the id the cookie already
                // carries, no data, and `handle` neither writes it back nor
                // moves the cookie. The request sees an empty session and
                // anything written to it is lost, which is the honest
                // consequence of a store that cannot be reached — and when the
                // store returns, the visitor is still signed in.
                tracing::error!(
                    error = %e,
                    "could not read the session; serving this request without one"
                );
                Loaded::detached(Session::restore(id, SessionData::default()))
            }
        }
    }

    /// Write the session back and clean up any ids it superseded.
    ///
    /// Returns the cookie value to send, which for a client-side store *is* the
    /// session.
    async fn save(&self, session: &Session) -> Result<String> {
        if !self.store.is_client_side() {
            for id in session.superseded_ids() {
                self.store.destroy(&id).await?;
            }
        }

        let id = session.id();
        let data = session.age_and_take();

        if !self.store.is_client_side() {
            self.store.write(&id, &data).await?;
        }

        // Encoded after ageing, so the cookie carries what the next request
        // should see rather than what this one did.
        self.store.encode(&id, &data)
    }
}

#[async_trait::async_trait]
impl Middleware for StartSession {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        let Loaded { session, detached } = self.load(&request).await;
        let started_empty = session.is_empty();

        request.extensions_mut().insert(session.clone());
        let response = next.run(request).await;

        // The store could not be read. Saving now would write an empty session
        // over the real one and move the cookie to it, so the visitor would be
        // logged out by an outage they otherwise rode through. Leave both
        // alone; see `load`.
        if detached {
            return response;
        }

        // A session that was empty and stayed empty is not worth a row or a
        // cookie — otherwise every anonymous hit on a public page allocates
        // both, and the table fills with nothing.
        if started_empty && !session.is_dirty() {
            return response;
        }

        match self.save(&session).await {
            Ok(value) => response.with_cookie(&self.cookie(&value)),
            Err(e) => {
                // The response is already built and is very likely correct; the
                // cost of a failed save is a lost session, not a wrong page.
                // Sending no cookie is better than sending a stale one.
                tracing::error!(error = %e, "could not save the session");
                response
            }
        }
    }

    fn name(&self) -> &'static str {
        "StartSession"
    }
}

impl std::fmt::Debug for StartSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartSession")
            .field("store", &self.store.name())
            .field("cookie", &self.config.cookie)
            .finish()
    }
}

/// Reaching a request's session.
pub trait SessionRequestExt {
    /// The session, if [`StartSession`] ran for this route.
    fn session(&self) -> Option<&Session>;
}

impl SessionRequestExt for Request {
    fn session(&self) -> Option<&Session> {
        self.extension::<Session>()
    }
}

/// The default cookie settings, exposed for tests and for a custom config.
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cookie: "rainier_session".to_string(),
            path: "/".to_string(),
            domain: None,
            secure: false,
            same_site: SameSite::Lax,
            lifetime: chrono::Duration::hours(2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemorySessionStore;
    use rainier_middleware::Pipeline;
    use rainier_support::BoxedFuture;
    use std::sync::Mutex;

    fn middleware() -> (Arc<StartSession>, Arc<MemorySessionStore>) {
        let store = Arc::new(MemorySessionStore::default());
        (Arc::new(StartSession::new(Arc::clone(&store) as Arc<dyn SessionStore>)), store)
    }

    async fn run<F>(start: &Arc<StartSession>, request: Request, handler: F) -> Response
    where
        F: Fn(Request) -> BoxedFuture<Response> + Send + Sync + 'static,
    {
        Pipeline::new()
            .through_arc(Arc::clone(start) as Arc<dyn Middleware>)
            .then(handler)
            .run(request)
            .await
    }

    fn session_cookie(response: &Response) -> Option<String> {
        let header = response.header("set-cookie")?;
        let value = header.split(';').next()?.strip_prefix("rainier_session=")?;
        Some(value.to_string())
    }

    #[tokio::test]
    async fn a_handler_that_writes_gets_a_cookie_and_a_stored_row() {
        let (start, store) = middleware();

        let response = run(&start, Request::builder().build(), |request| {
            Box::pin(async move {
                request.session().expect("a session").put("user_id", 42u64).unwrap();
                Response::text("ok")
            })
        })
        .await;

        let id = session_cookie(&response).expect("a session cookie");
        assert_eq!(store.read(&id).await.unwrap().unwrap().values["user_id"], 42);
    }

    #[tokio::test]
    async fn an_anonymous_request_allocates_nothing() {
        let (start, store) = middleware();

        let response =
            run(&start, Request::builder().build(), |_| Box::pin(async { Response::text("ok") }))
                .await;

        assert!(response.header("set-cookie").is_none(), "no cookie for a read-only visit");
        assert!(store.is_empty(), "and no row");
    }

    #[tokio::test]
    async fn a_session_survives_a_second_request() {
        let (start, _store) = middleware();

        let first = run(&start, Request::builder().build(), |request| {
            Box::pin(async move {
                request.session().unwrap().put("user_id", 7u64).unwrap();
                Response::text("ok")
            })
        })
        .await;
        let id = session_cookie(&first).unwrap();

        let second = run(
            &start,
            Request::builder().header("cookie", &format!("rainier_session={id}")).build(),
            |request| {
                let value = request.session().unwrap().get::<u64>("user_id");
                Box::pin(async move { Response::text(format!("{value:?}")) })
            },
        )
        .await;

        let body = second.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "Some(7)");
    }

    #[tokio::test]
    async fn an_unknown_or_malformed_cookie_starts_a_fresh_session() {
        let (start, store) = middleware();

        for hostile in ["nonsense", "../../etc/passwd", &"f".repeat(64), &"z".repeat(64)] {
            let response = run(
                &start,
                Request::builder().header("cookie", &format!("rainier_session={hostile}")).build(),
                |request| {
                    Box::pin(async move {
                        request.session().unwrap().put("x", 1).unwrap();
                        Response::text("ok")
                    })
                },
            )
            .await;

            let id = session_cookie(&response).expect("a fresh cookie");
            assert_ne!(id, hostile, "a client must not choose its own session id");
        }
        assert!(!store.is_empty());
    }

    #[tokio::test]
    async fn regenerating_deletes_the_old_row() {
        let (start, store) = middleware();

        let first = run(&start, Request::builder().build(), |request| {
            Box::pin(async move {
                request.session().unwrap().put("user_id", 7u64).unwrap();
                Response::text("ok")
            })
        })
        .await;
        let before = session_cookie(&first).unwrap();

        let second = run(
            &start,
            Request::builder().header("cookie", &format!("rainier_session={before}")).build(),
            |request| {
                // What a login does.
                request.session().unwrap().regenerate();
                Box::pin(async { Response::text("ok") })
            },
        )
        .await;
        let after = session_cookie(&second).unwrap();

        assert_ne!(after, before);
        assert!(store.read(&before).await.unwrap().is_none(), "the old row must not linger");
        assert_eq!(store.read(&after).await.unwrap().unwrap().values["user_id"], 7);
    }

    #[tokio::test]
    async fn the_cookie_is_http_only_and_same_site() {
        let (start, _) = middleware();

        let response = run(&start, Request::builder().build(), |request| {
            Box::pin(async move {
                request.session().unwrap().put("x", 1).unwrap();
                Response::text("ok")
            })
        })
        .await;

        let header = response.header("set-cookie").unwrap();
        assert!(header.contains("HttpOnly"), "{header}");
        assert!(header.contains("SameSite=Lax"), "{header}");
        assert!(header.contains("Path=/"), "{header}");
    }

    #[tokio::test]
    async fn flash_data_is_readable_next_request_and_gone_after() {
        let (start, _) = middleware();

        let first = run(&start, Request::builder().build(), |request| {
            Box::pin(async move {
                request.session().unwrap().flash("status", "Saved.").unwrap();
                Response::text("ok")
            })
        })
        .await;
        let id = session_cookie(&first).unwrap();
        let cookie = format!("rainier_session={id}");

        let read = |cookie: String| {
            let start = Arc::clone(&start);
            async move {
                let response =
                    run(&start, Request::builder().header("cookie", &cookie).build(), |request| {
                        let status = request.session().unwrap().string("status");
                        Box::pin(async move { Response::text(status.unwrap_or_default()) })
                    })
                    .await;
                String::from_utf8(
                    response.into_http().into_body().collect().await.unwrap().to_vec(),
                )
                .unwrap()
            }
        };

        assert_eq!(read(cookie.clone()).await, "Saved.");
        assert_eq!(read(cookie).await, "", "flash data must not survive twice");
    }

    #[tokio::test]
    async fn a_route_without_the_middleware_has_no_session() {
        let response = Pipeline::new()
            .then(|request: Request| async move {
                Response::text(format!("{}", request.session().is_some()))
            })
            .run(Request::builder().build())
            .await;

        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "false");
    }

    #[tokio::test]
    async fn a_store_that_fails_does_not_fail_the_request() {
        struct Broken;
        impl SessionStore for Broken {
            fn name(&self) -> &str {
                "broken"
            }
            fn read<'a>(
                &'a self,
                _: &'a str,
            ) -> rainier_support::BoxFuture<'a, Result<Option<SessionData>>> {
                Box::pin(async { Err(rainier_support::Error::internal("down")) })
            }
            fn write<'a>(
                &'a self,
                _: &'a str,
                _: &'a SessionData,
            ) -> rainier_support::BoxFuture<'a, Result<()>> {
                Box::pin(async { Err(rainier_support::Error::internal("down")) })
            }
            fn destroy<'a>(&'a self, _: &'a str) -> rainier_support::BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }

        let start = Arc::new(StartSession::new(Arc::new(Broken)));
        let response = run(&start, Request::builder().build(), |request| {
            Box::pin(async move {
                request.session().unwrap().put("x", 1).unwrap();
                Response::text("the page still renders")
            })
        })
        .await;

        assert_eq!(response.status(), rainier_http::StatusCode::OK);
    }

    /// A store that holds one session and refuses every read, so a request
    /// arrives with a cookie naming a session that is really there and really
    /// unreadable — a shard mid-failover, which is the case that matters.
    struct UnreadableStore {
        writes: Arc<Mutex<Vec<(String, SessionData)>>>,
        destroys: Arc<Mutex<Vec<String>>>,
    }

    impl UnreadableStore {
        fn new() -> Self {
            Self {
                writes: Arc::new(Mutex::new(Vec::new())),
                destroys: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl SessionStore for UnreadableStore {
        fn name(&self) -> &str {
            "unreadable"
        }

        fn read<'a>(
            &'a self,
            _: &'a str,
        ) -> rainier_support::BoxFuture<'a, Result<Option<SessionData>>> {
            Box::pin(async {
                Err(rainier_support::Error::service_unavailable(
                    "Redis: slot 15558 has no reachable node: its master is gone",
                ))
            })
        }

        fn write<'a>(
            &'a self,
            id: &'a str,
            data: &'a SessionData,
        ) -> rainier_support::BoxFuture<'a, Result<()>> {
            let writes = Arc::clone(&self.writes);
            let entry = (id.to_string(), data.clone());
            Box::pin(async move {
                writes.lock().unwrap().push(entry);
                Ok(())
            })
        }

        fn destroy<'a>(&'a self, id: &'a str) -> rainier_support::BoxFuture<'a, Result<()>> {
            let destroys = Arc::clone(&self.destroys);
            let id = id.to_string();
            Box::pin(async move {
                destroys.lock().unwrap().push(id);
                Ok(())
            })
        }
    }

    /// A request carrying a session cookie, the way a signed-in visitor's is.
    ///
    /// The id has to be one this application could have minted, or `decode`
    /// rejects the cookie before the store is read at all — a different path
    /// than the one these tests are about.
    fn with_session_cookie(config: &SessionConfig) -> (Request, String) {
        let id = crate::session::generate_session_id();
        let request =
            Request::builder().header("cookie", &format!("{}={}", config.cookie, id)).build();
        (request, id)
    }

    #[tokio::test]
    async fn an_unreadable_session_is_not_overwritten_with_an_empty_one() {
        // The production failure. The store was reachable enough to hold the
        // session and not reachable enough to return it; starting a fresh one
        // wrote an empty session under a new id and set that as the cookie, so
        // the real one was orphaned and the visitor was signed out by a blip.
        let store = Arc::new(UnreadableStore::new());
        let writes = Arc::clone(&store.writes);
        let destroys = Arc::clone(&store.destroys);

        let start = Arc::new(StartSession::new(Arc::clone(&store) as Arc<dyn SessionStore>));
        let (request, _real_id) = with_session_cookie(&SessionConfig::default());

        let response = run(&start, request, |request| {
            Box::pin(async move {
                // A handler that writes, which is what used to force the save.
                request.session().unwrap().put("x", 1).unwrap();
                Response::text("the page still renders")
            })
        })
        .await;

        assert_eq!(response.status(), rainier_http::StatusCode::OK);
        assert!(writes.lock().unwrap().is_empty(), "nothing may be written over the real session");
        assert!(destroys.lock().unwrap().is_empty(), "and nothing may be destroyed");
    }

    #[tokio::test]
    async fn an_unreadable_session_does_not_move_the_cookie() {
        // The other half of being signed out: even without a write, a response
        // that sets a cookie for a new id points the next request at the new
        // session and abandons the real one.
        let store = Arc::new(UnreadableStore::new());
        let start = Arc::new(StartSession::new(store as Arc<dyn SessionStore>));
        let config = SessionConfig::default();
        let (request, _real_id) = with_session_cookie(&config);

        let response = run(&start, request, |request| {
            Box::pin(async move {
                request.session().unwrap().put("x", 1).unwrap();
                Response::text("ok")
            })
        })
        .await;

        let set_cookie: Vec<_> = response
            .into_http()
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|v| v.starts_with(&config.cookie))
            .map(|v| v.to_string())
            .collect();

        assert!(set_cookie.is_empty(), "the session cookie must not move: {set_cookie:?}");
    }

    #[tokio::test]
    async fn a_session_that_is_merely_absent_still_starts_a_fresh_one() {
        // The distinction the fix rests on. `Ok(None)` is a session that is
        // genuinely gone — expired, or never existed — and there is nothing on
        // the other end of that id to protect, so it must still behave as it
        // always did. Only a read that *failed* is held back.
        struct Empty {
            writes: Arc<Mutex<usize>>,
        }

        impl SessionStore for Empty {
            fn name(&self) -> &str {
                "empty"
            }
            fn read<'a>(
                &'a self,
                _: &'a str,
            ) -> rainier_support::BoxFuture<'a, Result<Option<SessionData>>> {
                Box::pin(async { Ok(None) })
            }
            fn write<'a>(
                &'a self,
                _: &'a str,
                _: &'a SessionData,
            ) -> rainier_support::BoxFuture<'a, Result<()>> {
                let writes = Arc::clone(&self.writes);
                Box::pin(async move {
                    *writes.lock().unwrap() += 1;
                    Ok(())
                })
            }
            fn destroy<'a>(&'a self, _: &'a str) -> rainier_support::BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }

        let writes = Arc::new(Mutex::new(0usize));
        let store = Arc::new(Empty { writes: Arc::clone(&writes) });
        let start = Arc::new(StartSession::new(store as Arc<dyn SessionStore>));
        let (request, _) = with_session_cookie(&SessionConfig::default());

        let response = run(&start, request, |request| {
            Box::pin(async move {
                request.session().unwrap().put("x", 1).unwrap();
                Response::text("ok")
            })
        })
        .await;

        assert_eq!(response.status(), rainier_http::StatusCode::OK);
        assert_eq!(*writes.lock().unwrap(), 1, "an absent session is still replaced and saved");
    }
}
