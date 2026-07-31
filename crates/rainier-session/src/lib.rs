//! # rainier-session
//!
//! Sessions: the [`Session`] bag a handler reads and writes, the
//! [`SessionStore`] port it is persisted through, and the [`StartSession`]
//! middleware that ties them to a cookie.
//!
//! ```
//! use rainier_session::{MemorySessionStore, SessionRequestExt, StartSession, SessionStore};
//! use rainier_http::{Request, Response};
//! use rainier_middleware::{Middleware, Pipeline};
//! use std::sync::Arc;
//!
//! # #[tokio::main] async fn main() {
//! let store = Arc::new(MemorySessionStore::default());
//! let start: Arc<dyn Middleware> =
//!     Arc::new(StartSession::new(Arc::clone(&store) as Arc<dyn SessionStore>));
//!
//! let response = Pipeline::new()
//!     .through_arc(start)
//!     .then(|request: Request| async move {
//!         let session = request.session().expect("StartSession ran");
//!         session.put("user_id", 42u64).unwrap();
//!         Response::text("ok")
//!     })
//!     .run(Request::builder().build())
//!     .await;
//!
//! assert!(response.header("set-cookie").is_some());
//! # }
//! ```
//!
//! ## The session is on the request, not on a facade
//!
//! A session *facade* can work in PHP because the container rebinds a
//! request-scoped session per request. Rainier's facades resolve from a
//! **process-global** application, so there is nothing honest for
//! `Session::instance().get(…)` to return — with two requests in flight, which
//! one would it be?
//!
//! So the split is explicit:
//!
//! | | |
//! |---|---|
//! | [`request.session()`](SessionRequestExt::session) | this request's bag |
//! | [`SessionManager`] (the `Session` facade) | the store and its settings |
//!
//! The facade is for what genuinely is application-wide: reading or destroying
//! a session by id, and collecting expired ones.
//!
//! ## Flash data
//!
//! [`flash`](Session::flash) stores a value for exactly one further request —
//! the redirect-then-show-a-message pattern, with nothing to remember to clean
//! up:
//!
//! ```
//! # use rainier_session::{Session, SessionData};
//! let session = Session::new();
//! session.flash("status", "Saved.").unwrap();
//!
//! // …the request ends, and the next one can read it.
//! let next = Session::restore("id", session.age_and_take());
//! assert_eq!(next.string("status").as_deref(), Some("Saved."));
//!
//! // …and the one after that cannot.
//! let later = Session::restore("id", next.age_and_take());
//! assert!(!later.has("status"));
//! ```
//!
//! ## Drivers
//!
//! | Driver | Survives a restart | Shared | Revocable | Note |
//! |---|---|---|---|---|
//! | [`MemorySessionStore`] | no | no | yes | development |
//! | [`DatabaseSessionStore`] | yes | yes | yes | never evicts |
//! | [`CacheSessionStore`] | depends | yes | yes | expires itself; **can evict** |
//! | [`CookieSessionStore`] | yes | yes | **no** | no server state at all |
//!
//! A memory store behind a load balancer makes a user's session appear to
//! vanish and reappear as they are routed around, which looks like a bug in
//! your login code and is not.
//!
//! The cookie driver is the odd one: the whole session travels encrypted in the
//! cookie, so there is nothing to store and nothing to expire — and nothing to
//! revoke either. A session you cannot see is a session you cannot end.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod cache;
pub mod cookie;
pub mod database;
pub mod driver;
pub mod manager;
pub mod middleware;
pub mod session;
pub mod store;

pub use cache::CacheSessionStore;
pub use cookie::CookieSessionStore;
pub use database::{DatabaseSessionStore, SessionRow};
pub use driver::SessionDriver;
pub use manager::{SessionConfig, SessionManager};
pub use middleware::{SessionRequestExt, StartSession};
pub use session::{generate_session_id, Session, SessionData, TOKEN_KEY};
pub use store::{is_well_formed, MemorySessionStore, SessionStore};

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::{Request, Response};
    use rainier_middleware::{Middleware, Pipeline};
    use std::sync::Arc;

    /// The login flow end to end: a session is regenerated on authentication,
    /// the old row goes, and the new one carries the user.
    #[tokio::test]
    async fn logging_in_rotates_the_session_and_keeps_the_user() {
        let store = Arc::new(MemorySessionStore::default());
        let start: Arc<dyn Middleware> =
            Arc::new(StartSession::new(Arc::clone(&store) as Arc<dyn SessionStore>));

        let run = |cookie: Option<String>, action: &'static str| {
            let start = Arc::clone(&start);
            async move {
                let mut builder = Request::builder();
                if let Some(cookie) = cookie {
                    builder = builder.header("cookie", &cookie);
                }
                Pipeline::new()
                    .through_arc(start)
                    .then(move |request: Request| async move {
                        let session = request.session().expect("a session");
                        match action {
                            "visit" => {
                                session.put("cart", vec![1u64, 2]).unwrap();
                            }
                            "login" => {
                                session.regenerate();
                                session.put("user_id", 7u64).unwrap();
                            }
                            _ => {}
                        }
                        Response::text("ok")
                    })
                    .run(builder.build())
                    .await
            }
        };

        let cookie_of = |response: &Response| {
            response
                .header("set-cookie")
                .and_then(|h| h.split(';').next())
                .and_then(|c| c.strip_prefix("rainier_session="))
                .map(str::to_string)
        };

        // An anonymous visit that puts something in the cart.
        let first = run(None, "visit").await;
        let anonymous = cookie_of(&first).expect("a cookie");

        // Then they log in.
        let second = run(Some(format!("rainier_session={anonymous}")), "login").await;
        let authenticated = cookie_of(&second).expect("a new cookie");

        assert_ne!(authenticated, anonymous, "the id must change on login");
        assert!(
            store.read(&anonymous).await.unwrap().is_none(),
            "the pre-login row is what a fixation attack holds"
        );

        let data = store.read(&authenticated).await.unwrap().expect("the new session");
        assert_eq!(data.values["user_id"], 7);
        assert_eq!(data.values["cart"], serde_json::json!([1, 2]), "the cart survives login");
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use rainier_crypt::{Encryption, Key, KeyRing};
    use rainier_http::{Request, Response};
    use rainier_middleware::{Middleware, Pipeline};
    use std::sync::Arc;

    /// Drive a store through the real middleware twice, following the cookie.
    ///
    /// Every driver has to behave the same from the outside, whatever it does
    /// underneath — which is the point of the port and is not obvious for the
    /// cookie one, where "underneath" is the cookie itself.
    async fn counts_across_requests(store: Arc<dyn SessionStore>) -> (u64, u64) {
        let start: Arc<dyn Middleware> = Arc::new(StartSession::new(store));

        let visit = |cookie: Option<String>| {
            let start = Arc::clone(&start);
            async move {
                let mut builder = Request::builder();
                if let Some(cookie) = cookie {
                    builder = builder.header("cookie", &cookie);
                }

                let response = Pipeline::new()
                    .through_arc(start)
                    .then(|request: Request| async move {
                        let session = request.session().expect("a session");
                        let seen: u64 = session.get("seen").unwrap_or(0);
                        session.put("seen", seen + 1).unwrap();
                        Response::text(seen.to_string())
                    })
                    .run(builder.build())
                    .await;

                let cookie = response
                    .header("set-cookie")
                    .and_then(|h| h.split(';').next())
                    .and_then(|c| c.strip_prefix("rainier_session="))
                    .map(str::to_string);

                let body = response.into_http().into_body().collect().await.unwrap();
                (String::from_utf8(body.to_vec()).unwrap().parse::<u64>().unwrap(), cookie)
            }
        };

        let (first, cookie) = visit(None).await;
        let cookie = cookie.expect("the session should be persisted");
        let (second, _) = visit(Some(format!("rainier_session={cookie}"))).await;

        (first, second)
    }

    fn crypt() -> Encryption {
        Encryption::from_keys(KeyRing::new(Key::generate()))
    }

    #[tokio::test]
    async fn every_driver_persists_a_session_across_requests() {
        let drivers: Vec<(&str, Arc<dyn SessionStore>)> = vec![
            ("memory", Arc::new(MemorySessionStore::default())),
            ("cache", Arc::new(CacheSessionStore::new(Arc::new(MemoryCache::new())))),
            ("cookie", Arc::new(CookieSessionStore::new(crypt()))),
        ];

        for (name, store) in drivers {
            assert_eq!(counts_across_requests(store).await, (0, 1), "{name}");
        }
    }

    #[tokio::test]
    async fn the_cookie_driver_carries_the_session_and_not_an_id() {
        // The distinguishing property: the cookie is long, because it *is* the
        // session rather than a pointer to one.
        let store: Arc<dyn SessionStore> = Arc::new(CookieSessionStore::new(crypt()));
        let start: Arc<dyn Middleware> = Arc::new(StartSession::new(Arc::clone(&store)));

        let response = Pipeline::new()
            .through_arc(start)
            .then(|request: Request| async move {
                request.session().unwrap().put("user_id", 42u64).unwrap();
                Response::text("ok")
            })
            .run(Request::builder().build())
            .await;

        let value = response
            .header("set-cookie")
            .and_then(|h| h.split(';').next())
            .and_then(|c| c.strip_prefix("rainier_session="))
            .expect("a cookie");

        assert!(value.len() > 64, "a cookie session is longer than an id: {}", value.len());
        assert!(!is_well_formed(value), "it is not an id at all");

        // The key name, not the value: a two-character value like "42" turns up
        // in base64 ciphertext by chance often enough to make that assertion
        // flaky. A seven-character one effectively does not.
        assert!(!value.contains("user_id"), "the client cannot read it: {value}");
    }

    #[tokio::test]
    async fn a_cookie_session_from_another_applications_key_is_discarded() {
        let store: Arc<dyn SessionStore> = Arc::new(CookieSessionStore::new(crypt()));
        let theirs = CookieSessionStore::new(crypt());

        let forged = theirs.encode(&generate_session_id(), &SessionData::default()).unwrap();
        let start: Arc<dyn Middleware> = Arc::new(StartSession::new(store));

        let response = Pipeline::new()
            .through_arc(start)
            .then(|request: Request| async move {
                let session = request.session().expect("a fresh session");
                Response::text(session.is_empty().to_string())
            })
            .run(Request::builder().header("cookie", &format!("rainier_session={forged}")).build())
            .await;

        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "true", "a session we did not write must not be trusted");
    }
}
