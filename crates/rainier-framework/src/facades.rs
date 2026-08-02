//! The facades — static proxies onto container-resolved services.
//!
//! ```ignore
//! use rainier_framework::facades::{Config, DB, Event, Queue};
//!
//! let name: String = Config::instance().get_or("app.name", "Rainier".into());
//! Event::instance().dispatch(UserRegistered { id }).await?;
//! Queue::instance().dispatch(SendWelcomeEmail { id }).await?;
//! let posts: Vec<Post> = DB::instance().fetch_all(prepared).await?;
//! ```
//!
//! Every call resolves through the container, so rebinding the accessor swaps
//! what every facade call sees — which is how a test installs a fake without a
//! separate `swap` mechanism.
//!
//! The convenience is real and so is the cost: a facade hides a dependency
//! that a constructor argument would have made visible. Reach for one in
//! application code and route closures; take the service as an argument in
//! anything you intend to unit-test.

use std::sync::Arc;

use rainier_container::facade;
use rainier_view::ViewEngine;

/// The application's view engine, as a container-storable value.
///
/// A newtype rather than binding the engine directly, so an application can
/// swap the default engine for something else without every `View::…` call
/// site naming a different type.
#[derive(Clone)]
pub struct Views(pub Arc<dyn ViewEngine>);

impl Views {
    /// Wrap an engine.
    pub fn new(engine: Arc<dyn ViewEngine>) -> Self {
        Self(engine)
    }

    /// The engine.
    pub fn engine(&self) -> &Arc<dyn ViewEngine> {
        &self.0
    }

    /// Render a named view.
    pub fn render(&self, name: &str, data: &serde_json::Value) -> rainier_support::Result<String> {
        self.0.render(name, data)
    }

    /// Render a [`View`](rainier_view::View).
    pub fn render_view(&self, view: &rainier_view::View) -> rainier_support::Result<String> {
        self.0.render_view(view)
    }
}

impl std::fmt::Debug for Views {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Views(..)")
    }
}

facade!(
    /// The configuration repository.
    Config => rainier_config::Config
);

facade!(
    /// The event dispatcher — Rainier's hook bus.
    Event => rainier_events::Dispatcher
);

facade!(
    /// The database handle.
    DB => rainier_database::Database
);

facade!(
    /// The queue dispatcher.
    Queue => rainier_queue::QueueManager
);

facade!(
    /// The mailer.
    Mail => rainier_mail::Mailer
);

facade!(
    /// The view engine.
    View => Views
);

facade!(
    /// URL generation for named routes.
    Url => rainier_routing::UrlGenerator
);

facade!(
    /// The middleware registry.
    Middleware => rainier_middleware::MiddlewareRegistry
);

facade!(
    /// Encryption and signing.
    Crypt => rainier_crypt::Encryption
);

facade!(
    /// Password hashing, with the algorithm behind a selection.
    ///
    /// `Hash::instance().hash(..)` writes with the driver `HASH_DRIVER`
    /// selected; `verify` dispatches on the stored hash's own prefix, so any
    /// registered algorithm's rows keep verifying whatever is selected. Bind
    /// a [`HashManager`](rainier_crypt::HashManager) in a provider first.
    Hash => rainier_crypt::HashManager
);

facade!(
    /// Notifications.
    Notify => rainier_notify::Notifier
);

facade!(
    /// Broadcasting to WebSocket channels.
    Broadcast => rainier_broadcast::Broadcasting
);

facade!(
    /// The cache.
    Cache => rainier_cache::CacheManager
);

facade!(
    /// File storage.
    Storage => rainier_filesystem::Storage
);

impl Storage {
    /// The disk registered under `name` — `Storage::disk("content")`.
    ///
    /// A forwarding method, so the call names the disk rather than the
    /// container step:
    ///
    /// ```ignore
    /// Storage::disk("content")               // this
    /// Storage::instance().disk("content")    // the same thing, spelled out
    /// ```
    ///
    /// Both work, and neither is doing anything the other is not. `instance()`
    /// resolves the service from the container; frameworks that let you write
    /// the short form resolve it too, behind a magic static call this language
    /// has no equivalent of. Writing the forwarder by hand is the whole of the
    /// difference.
    ///
    /// Returns `None` for a disk that was never registered, and **never** the
    /// default disk — see [`rainier_filesystem::Storage::disk`] for why that
    /// distinction is worth an `Option`.
    ///
    /// # Panics
    ///
    /// If storage is not bound in the container, as every facade call does. A
    /// disk lookup on an application with no storage bound at all is a
    /// configuration bug rather than a runtime condition. Reach for
    /// [`Facade::try_instance`](rainier_container::Facade::try_instance) where
    /// degrading is the right behaviour.
    pub fn disk(name: &str) -> Option<rainier_filesystem::Storage> {
        <Self as rainier_container::Facade>::instance().disk(name)
    }
}

facade!(
    /// The session **store** and its settings.
    ///
    /// Not a request's session — a facade is process-global and a session
    /// belongs to one request, so with two requests in flight there is
    /// nothing honest for `Session::instance().get(..)` to return. Reach a
    /// request's own bag with
    /// [`request.session()`](rainier_session::SessionRequestExt::session).
    ///
    /// This is for what genuinely is application-wide: reading or destroying
    /// a session by id, and collecting expired ones.
    Session => rainier_session::SessionManager
);

// There is deliberately **no** `Auth` facade here. `AuthManager<U>` is generic
// over the application's user model, and a facade is a concrete type — so the
// application declares its own in one line:
//
// ```ignore
// rainier_framework::facade!(Auth => rainier_framework::auth::AuthManager<app::models::User>);
// ```
//
// See the crate docs for why the manager is generic rather than erasing the
// user behind `dyn Authenticatable`.

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_container::{
        clear_facade_application, set_facade_application, Application, Facade,
    };
    use rainier_view::MemoryEngine;

    // The facade application is process-global, so these must not interleave.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn install(build: impl FnOnce(&Application)) -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let app = Application::new(".");
        build(&app);
        set_facade_application(Arc::new(app));
        guard
    }

    #[test]
    fn the_config_facade_reads_through_the_container() {
        let _guard = install(|app| {
            let config = rainier_config::Config::new();
            config.set("app.name", "Rainier").unwrap();
            app.instance(config);
        });

        assert_eq!(Config::instance().string("app.name").as_deref(), Some("Rainier"));
        clear_facade_application();
    }

    #[test]
    fn the_view_facade_renders() {
        let _guard = install(|app| {
            app.instance(Views::new(Arc::new(
                MemoryEngine::new().with("greeting", "Hi {{ name }}"),
            )));
        });

        let rendered =
            View::instance().render("greeting", &serde_json::json!({ "name": "Ada" })).unwrap();
        assert_eq!(rendered, "Hi Ada");
        clear_facade_application();
    }

    #[test]
    fn rebinding_swaps_what_every_call_sees() {
        let _guard = install(|app| {
            app.instance(Views::new(Arc::new(MemoryEngine::new().with("t", "first"))));
        });
        assert_eq!(View::instance().render("t", &serde_json::json!({})).unwrap(), "first");

        rainier_container::facade_application()
            .instance(Views::new(Arc::new(MemoryEngine::new().with("t", "second"))));
        assert_eq!(View::instance().render("t", &serde_json::json!({})).unwrap(), "second");

        clear_facade_application();
    }

    #[test]
    fn an_unbound_facade_reads_as_none_rather_than_panicking_through_try() {
        let _guard = install(|_| {});
        assert!(DB::try_instance().is_none());
        clear_facade_application();
    }
}
