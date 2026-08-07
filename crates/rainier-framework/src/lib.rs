//! # Rainier
//!
//! An MVC framework for Rust, built on the
//! [Rainier ORM](https://github.com/safewords/rainier-framework/tree/main/crates/rainier-orm) DBAL.
//!
//! Rainier takes the *structures* server-side MVC frameworks converge on — a
//! service container, service
//! providers, a router with named routes and middleware groups, form requests,
//! guards, jobs, mailables, facades, events — and gives each one a Rust shape
//! rather than a transliteration from another language. Where convention and
//! the language disagree, the
//! disagreement is resolved in Rust's favour and the reason is written down in
//! the module that made the call.
//!
//! ```no_run
//! use rainier_framework::prelude::*;
//!
//! async fn index() -> &'static str {
//!     "Hello from Rainier"
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let app = Rainier::new(".")
//!         .with_routes(|router| {
//!             router.get("/", index).name("home");
//!         })
//!         .boot()
//!         .await?;
//!
//!     rainier_framework::console("rainier").run_from_env(&app).await;
//!     Ok(())
//! }
//! ```
//!
//! ## The crates
//!
//! Each component is its own crate, and depends only on what it genuinely
//! needs — so an application that wants the queue but not HTTP pays for
//! neither the router nor hyper.
//!
//! | Crate | What it owns |
//! |---|---|
//! | [`support`] | the error type, futures, type-maps, string inflection |
//! | [`rainier_orm`] | the DBAL: entities, queries, DDL, sharding, the `Executor` port |
//! | [`container`] | the IoC container, service providers, lifecycle hooks, facades |
//! | [`config`] | the config repository and `.env` |
//! | [`events`] | the event dispatcher — the hook bus |
//! | [`http`] | requests, responses, cookies, uploads, extractors |
//! | [`middleware`] | the `handle(request, next)` pipeline and the built-in set |
//! | [`routing`] | route declaration, groups, resources, URL generation |
//! | [`validation`] | rules, the validator, and **request contracts** |
//! | [`view`] | the template engine |
//! | [`database`] | Rainier ORM integration, models, repositories |
//! | [`auth`] | guards, user providers, gates |
//! | [`queue`] | jobs, queue drivers, the worker |
//! | [`mail`] | mailables, the mailer, transports |
//! | [`server`] | the HTTP kernel and the hyper server |
//! | [`console`] | the console and its commands |
//! | [`cache`] | the cache port and its drivers |
//! | [`crypt`] | encryption, message signing, password hashing |
//! | [`drivers`] | shared Redis, Memcached and AWS transports |
//! | [`filesystem`] | file storage: local, memory, S3/R2 |
//! | [`notify`] | notifications: a message to a recipient, over their channels |
//! | [`scheduler`] | cron expressions, the schedule, and its atomic locks |
//! | [`session`] | the session bag, its store, and the middleware |
//!
//! ## Where Rainier departs from MVC convention
//!
//! - **Middleware is named, not typed, at the route.** `.middleware("auth")`
//!   keeps `routing` from depending on `auth` — see [`middleware::registry`].
//! - **The auth manager is generic over the user model** rather than erasing
//!   it behind `dyn Authenticatable`, because an application wants its own
//!   type back. See [`auth`].
//! - **Model hooks can veto but not mutate.** Several listeners mutating one
//!   row in registration order would make the outcome depend on wiring; see
//!   [`database::model`].
//! - **Request bodies are buffered.** That is what lets `request.input(..)` be
//!   synchronous, as `$request->input()` is; see [`http::body`].
//! - **Configuration is typed at both ends.** `config('cache.driver')` has a
//!   magic string for the path and another for the value. Rainier has
//!   [`keys::CACHE_DRIVER`] and a [`cache::CacheDriver`] enum, so a misspelled
//!   path does not compile and a misspelled driver fails the boot rather than
//!   silently caching in-process. See [`keys`].
//!
//! ## The Rainier ORM `Send` story
//!
//! Rainier ORM used to hold a `sea_query` statement across its awaits, which
//! made every `repo::` future `!Send` — unusable inside a handler the server
//! will `tokio::spawn`, since `sea_query` statements hold `Rc`. That is fixed
//! upstream: statements are built and dropped in a scope that ends before the
//! await, and `Query`'s terminals consume `self` outside their `async` block
//! (an `async fn` captures every argument whether or not the body moves it
//! out).
//!
//! So `repo::` and the query builder work directly against a
//! [`database::Database`]. Rainier still renders its repository SQL
//! synchronously through [`database::statement`] — now a design choice that
//! keeps shard routing and dialect rendering explicit at the framework seam,
//! rather than a workaround.
//!
//! One exception remains: `rainier_orm::migrate::Migrator::run` boxes its steps
//! behind `dyn`, so its future cannot be `Send` on stable.
//! [`database::Migrator`] is the `Send` alternative — it renders DDL
//! synchronously and executes plain strings, and its [`Migration`] contract
//! makes every step declare how to undo itself.
//!
//! [`Migration`]: database::Migration

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod binding;
pub mod bootstrap;
pub mod broadcasting;
pub mod commands;
pub mod cors;
pub mod facades;
pub mod groups;
pub mod health;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod keys;
pub mod limits;
pub mod mail;
pub mod notifications;
pub mod observability;
pub mod public;
pub mod queued_listeners;
#[cfg(feature = "kafka")]
pub mod relay;
pub mod scheduling;
pub mod session_delivery;
pub mod signed;
pub mod testing;

pub use bootstrap::Rainier;
pub use commands::{
    console, KeyGenerateCommand, MigrateCommand, MigrateRollbackCommand, QueueWorkCommand,
    RouteListCommand, ServeCommand,
};
pub use facades::Views;
pub use health::{Health, Report as HealthReport};
pub use limits::RateLimits;
pub use queued_listeners::{DispatcherExt, FromEvent};
pub use scheduling::{
    CommandTask, JobTask, ScheduleExt, ScheduleListCommand, ScheduleRunCommand, ScheduleWorkCommand,
};
pub use signed::{SignedUrls, ValidateSignature};
pub use testing::{TestApp, TestResponse};

// --- the components, re-exported under short names -------------------------

pub use rainier_auth as auth;
pub use rainier_broadcast as broadcast;
pub use rainier_cache as cache;
pub use rainier_config as config;
pub use rainier_console as console_kernel;
pub use rainier_container as container;
pub use rainier_crypt as crypt;
pub use rainier_database as database;
pub use rainier_drivers as drivers;
pub use rainier_events as events;
pub use rainier_filesystem as filesystem;
pub use rainier_http as http;
pub use rainier_http_client as http_client;
pub use rainier_metrics as metrics;
pub use rainier_middleware as middleware;
pub use rainier_notify as notify;
pub use rainier_openapi as openapi;
pub use rainier_queue as queue;
pub use rainier_routing as routing;
pub use rainier_scheduler as scheduler;
pub use rainier_server as server;
pub use rainier_session as session;
pub use rainier_support as support;
pub use rainier_telemetry as telemetry;
pub use rainier_validation as validation;
pub use rainier_view as view;
pub use rainier_websocket as websocket;

/// The ORM, so an application depends on one Rainier ORM version.
pub use rainier_orm;

// The macros are exported at the crate root by `#[macro_export]`, so they are
// already reachable as `rainier_framework::facade!` and `rainier_framework::bind_executor!`.
pub use rainier_container::{facade, Application, Facade, ServiceProvider};
pub use rainier_database::bind_executor;

/// Everything an application file usually wants in scope.
///
/// ```
/// use rainier_framework::prelude::*;
/// ```
pub mod prelude {
    pub use crate::bootstrap::Rainier;
    pub use crate::facades::{
        Broadcast, Cache, Config, Crypt, Event, Hash, Mail, Middleware, Notify, Queue, Session,
        Storage, Url, View, Views, DB,
    };

    pub use rainier_container::{Application, Container, Facade, ServiceProvider};
    pub use rainier_support::{build_info, BuildInfo, Context as _, Error, ErrorKind, Result};

    // Typed configuration: a `Key<T>` for the path, a `Setting` for the value.
    // The macros live at their crates' roots, so they are reachable as
    // `rainier_framework::config::config_keys!` without being imported.
    pub use rainier_config::{AppEnv, Key as ConfigurationKey, Setting as _};
    pub use rainier_telemetry::LogFormat;

    pub use crate::binding::{Bound, BoundAs};
    pub use rainier_http::extract::{Bearer, Form, Json, Path, Query, RawBody};
    pub use rainier_http::{
        Cookie, FromRequest, Html, IntoResponse, Method, Redirect, Request, Response, StatusCode,
    };

    pub use rainier_cache::{Cache as CacheContract, CacheDriver, CacheExt as _, CacheManager};
    pub use rainier_crypt::{EncrypterExt as _, Encryption, SignerExt as _};
    pub use rainier_filesystem::{
        DiskConfig, Disks, Filesystem, FilesystemDriver, FilesystemExt as _, LocalDisk,
        LocalFilesystem, S3Disk, Storage as StorageManager,
    };
    pub use rainier_http_client::{Backoff, Http};
    pub use rainier_middleware::{
        Compress, MethodOverride, Middleware as MiddlewareContract, Next, Timeout, TrustProxies,
    };
    pub use rainier_routing::{
        GroupAttributes, ParamConstraint, Req, ResourceController, Route, Router,
    };
    pub use rainier_session::{
        Session as SessionBag, SessionDriver, SessionManager, SessionRequestExt as _, StartSession,
    };
    pub use rainier_validation::{field, FormRequest, Rule, RuleSet, Validated, Validator};

    pub use rainier_database::{
        BelongsTo, BelongsToMany, Criteria, Database, DatePart, EntityRepository, HasMany, HasOne,
        JoinKind, Model, Paginated, Projection, Related, RelatedCounts, Relation, Repository,
    };
    pub use rainier_orm::Entity;

    pub use rainier_auth::{
        AuthManager, Authenticatable, AuthenticatedUser, Credentials, Gate, GuardExt as _,
    };
    pub use rainier_broadcast::{Broadcastable, Broadcasting, Channel as BroadcastChannelName};
    pub use rainier_events::Dispatcher;
    pub use rainier_mail::{Content, Envelope, MailDriver, MailEncryption, Mailable};
    pub use rainier_metrics::{Metrics, RecordMetrics};
    pub use rainier_notify::{Channels, Notifiable, Notification, Notifier};
    pub use rainier_queue::{Job, JobContext, QueueDriver, QueueManager};
    pub use rainier_telemetry::Trace;
    pub use rainier_view::View as ViewData;
    pub use rainier_websocket::{Rooms, Socket, WebSocketHandler, WebSocketRoutes};

    pub use async_trait::async_trait;
    pub use std::sync::Arc;
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use rainier_http::Method;

    #[tokio::test]
    async fn an_application_boots_and_serves() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.get("/", || async { "hello" }).name("home");
            })
            .boot()
            .await
            .expect("boots");

        let kernel = app.resolve::<rainier_server::Kernel>().unwrap();
        let response =
            kernel.handle_request(Request::builder().method(Method::GET).uri("/").build()).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_prelude_covers_a_realistic_controller() {
        // A compile-level assertion that the prelude is actually sufficient:
        // this uses an extractor, a form request, a response and the error
        // type without importing anything else.
        #[derive(serde::Deserialize)]
        struct StorePost {
            title: String,
        }

        #[async_trait]
        impl FormRequest for StorePost {
            fn rules() -> RuleSet {
                vec![field("title", [Rule::Required, Rule::String])]
            }
        }

        async fn store(Validated(post): Validated<StorePost>) -> Result<Response> {
            Ok(Response::json(&serde_json::json!({ "title": post.title })))
        }

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.post("/posts", store);
            })
            .boot()
            .await
            .expect("boots");

        let response = app
            .resolve::<rainier_server::Kernel>()
            .unwrap()
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/posts")
                    .json(&serde_json::json!({ "title": "Hello" }))
                    .build(),
            )
            .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_failed_contract_is_a_422() {
        #[derive(serde::Deserialize)]
        struct StorePost {
            #[allow(dead_code)]
            title: String,
        }

        #[async_trait]
        impl FormRequest for StorePost {
            fn rules() -> RuleSet {
                vec![field("title", [Rule::Required])]
            }
        }

        async fn store(Validated(_): Validated<StorePost>) -> &'static str {
            "created"
        }

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.post("/posts", store);
            })
            .boot()
            .await
            .unwrap();

        let response = app
            .resolve::<rainier_server::Kernel>()
            .unwrap()
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/posts")
                    .header("accept", "application/json")
                    .json(&serde_json::json!({}))
                    .build(),
            )
            .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
