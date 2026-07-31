//! The middleware groups Rainier ships — `web` and `api`, as
//! functions rather than as keys in a map.
//!
//! ```ignore
//! use rainier_framework::groups;
//!
//! router.group(GroupAttributes::new().middleware(groups::web()), |router| {
//!     router.get("/", home);
//! });
//! ```
//!
//! Three things follow from a group being a function.
//!
//! **You can find it.** "Go to definition" on `groups::web()` lands on the
//! list. A string key would send you grepping the kernel instead.
//!
//! **You cannot misspell it.** `groups::wev()` does not compile. `'wev'` boots
//! fine and serves every page in that group with no session and no security
//! headers.
//!
//! **Overriding is composition, not replacement.** Reassigning a
//! `'web'` key in a middleware map replaces the whole list, which is how
//! people accidentally drop `StartSession` while adding one thing. Here an
//! application writes its own function and says what it is starting from:
//!
//! ```ignore
//! pub fn web() -> MiddlewareStack {
//!     rainier_framework::groups::web().with(RequestId::new())
//! }
//! ```

use std::sync::Arc;

use rainier_middleware::{
    AddHeaders, ConvertEmptyStringsToNull, HandleCors, MiddlewareStack, ThrottleRequests,
    TrimStrings, TrustProxies,
};
use rainier_session::{SessionManager, StartSession};

/// Security headers plus a session — what a browser-facing page wants.
///
/// The session stage is [resolved from the
/// container](rainier_middleware::MiddlewareStack::resolved) when the router
/// compiles, because the store is chosen in `bootstrap.rs` and does not exist
/// while routes are being declared.
pub fn web() -> MiddlewareStack {
    MiddlewareStack::new().with(AddHeaders::security_defaults()).with_stack(session())
}

/// CORS plus a rate limit — what a JSON API wants.
///
/// No session on purpose: an API authenticates per request with a token, so a
/// session row and a `Set-Cookie` per call would be pure overhead.
pub fn api() -> MiddlewareStack {
    api_throttled(60)
}

/// [`api`] with an explicit per-minute limit.
///
/// The thing a name in a registry could not do: a group that takes an argument.
/// A string-keyed registry spells this `'throttle:60,1'` and parses the limit
/// back out of the name.
pub fn api_throttled(per_minute: u32) -> MiddlewareStack {
    MiddlewareStack::new()
        .with(HandleCors::any_origin())
        .with(ThrottleRequests::per_minute(per_minute))
}

/// Start a session, using whichever store the application bound.
pub fn session() -> MiddlewareStack {
    MiddlewareStack::new()
        .resolved(|manager: Arc<SessionManager>| StartSession::from_manager(&manager))
}

/// Normalise input — trim strings, and turn `""` into `null`.
///
/// Registered globally by the builder. Here as a value too, so an application
/// that wants it on some routes and not others can say so.
pub fn normalise_input() -> MiddlewareStack {
    MiddlewareStack::new().with(TrimStrings::new()).with(ConvertEmptyStringsToNull)
}

/// Trust `X-Forwarded-*` from the loopback and the private ranges.
///
/// The shape of "a proxy on this host or this network", which is where a proxy
/// nearly always is. For anything else name them:
/// `TrustProxies::these(["10.0.0.0/8"])`, and read
/// [the deployment notes](https://github.com/safewords/rainier-framework/blob/main/docs/deployment.md)
/// before reaching for `TrustProxies::all()`.
pub fn trust_local_proxies() -> MiddlewareStack {
    MiddlewareStack::new().with(TrustProxies::these([
        "127.0.0.0/8",
        "::1",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "fd00::/8",
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_container::Container;
    use rainier_session::MemorySessionStore;

    #[test]
    fn web_is_security_headers_and_a_session() {
        assert_eq!(web().labels(), vec!["AddHeaders", "StartSession"]);
    }

    #[test]
    fn api_has_no_session() {
        // The property worth pinning: a group that quietly started sessions
        // would put a row and a `Set-Cookie` on every API call.
        assert!(!api().labels().contains(&"StartSession"));
        assert_eq!(api().labels(), vec!["HandleCors", "ThrottleRequests"]);
    }

    #[test]
    fn a_group_can_take_an_argument() {
        // What a name in a registry cannot do without parsing itself back out
        // of a string.
        assert_eq!(api_throttled(10).len(), 2);
    }

    #[test]
    fn the_session_stage_is_built_from_the_container() {
        let container = Container::new();
        container.instance(SessionManager::new(Arc::new(MemorySessionStore::default())));

        let built = session().resolve(&container).expect("resolves");
        assert_eq!(built.len(), 1);
    }

    #[test]
    fn a_missing_session_manager_fails_with_a_readable_message() {
        let err = session().resolve(&Container::new()).err().expect("should fail");

        assert!(err.message().contains("StartSession"), "{}", err.message());
        assert!(err.message().contains("SessionManager"), "{}", err.message());
    }

    #[test]
    fn an_application_extends_a_group_rather_than_replacing_it() {
        // The mistake a string-keyed map invites: adding one thing to `web` by
        // rewriting the list, and dropping the session on the way.
        let extended = web().with(AddHeaders::new());

        assert_eq!(extended.labels(), vec!["AddHeaders", "StartSession", "AddHeaders"]);
    }
}
