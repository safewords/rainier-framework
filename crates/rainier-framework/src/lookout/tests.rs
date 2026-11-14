use super::*;

use rainier_auth::{Authenticate, Credentials, TokenGuard, UserProvider};
use rainier_http::StatusCode;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::Layer as _;

use crate::observability::TelemetrySettings;
use crate::testing::TestApp;
use crate::Rainier;

struct User {
    id: u64,
    admin: bool,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.to_string()
    }

    fn auth_password_hash(&self) -> Option<&str> {
        None
    }
}

/// `admin-token` is user 7, an admin; `user-token` is user 8, who is not.
struct Tokens;

#[async_trait::async_trait]
impl UserProvider<User> for Tokens {
    async fn retrieve_by_id(&self, _: &str) -> Result<Option<User>> {
        Ok(None)
    }

    async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }

    async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
        Ok(false)
    }

    async fn retrieve_by_token(&self, token: &str) -> Result<Option<User>> {
        Ok(match token {
            "admin-token" => Some(User { id: 7, admin: true }),
            "user-token" => Some(User { id: 8, admin: false }),
            _ => None,
        })
    }
}

fn auth() -> Arc<AuthManager<User>> {
    Arc::new(
        AuthManager::new("token").register(Arc::new(TokenGuard::new("token", Arc::new(Tokens)))),
    )
}

/// On, with a writer that never ticks on its own during a test — the tests
/// [`Lookout::flush`] when they want to look.
fn on(local: bool) -> impl FnOnce(LookoutConfig) -> LookoutConfig {
    move |mut config| {
        config.ingest.flush_interval_ms = 3_600_000;
        config.capture.user_id = true;
        config.enabled(true).local(local)
    }
}

fn builder() -> Rainier {
    let auth = auth();
    Rainier::new(".").without_facades().without_tracing().with_routes(move |router| {
        router.get("/posts/{post}", || async { "post" }).name("posts.show");
        router
            .get("/slow", || async {
                let span = tracing::info_span!(
                    "db.query",
                    db.system.name = "sqlite",
                    db.query.text = "SELECT * FROM posts WHERE id = ?",
                );
                let _entered = span.enter();
                std::thread::sleep(std::time::Duration::from_millis(5));
                "slow"
            })
            .name("slow");
        router.get("/me", || async { "me" }).name("me").middleware(Authenticate::new(auth));
    })
}

async fn batches(app: &TestApp) -> Vec<BatchSummary> {
    app.resolve::<Lookout>().unwrap().flush().await;
    let store = app.resolve::<Arc<dyn Store>>().unwrap();
    store.batches(BatchQuery::default()).await.unwrap()
}

/// The booted recorder's layer, as the current thread's subscriber — the
/// tests boot `without_tracing`, so the global one is not theirs to set.
fn record(app: &TestApp) -> tracing::subscriber::DefaultGuard {
    let lookout = app.resolve::<Lookout>().unwrap();
    tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()))
}

#[tokio::test]
async fn booting_with_lookout_on_binds_the_recorder_and_its_store() {
    let app = builder().with_lookout(on(true)).boot().await.unwrap();

    let lookout = app.resolve::<Lookout>().unwrap();
    assert!(lookout.is_enabled());
    assert_eq!(lookout.store().name(), "memory", "the deferred store was filled in");
    assert_eq!(app.resolve::<Arc<dyn Store>>().unwrap().name(), "memory");
    assert!(app.resolve::<Writer>().is_ok(), "the writer is kept");
}

#[tokio::test]
async fn off_binds_nothing_and_mounts_nothing() {
    let app = TestApp::new(builder().boot().await.unwrap()).unwrap();

    assert!(app.resolve::<Lookout>().is_err());
    assert!(app.resolve::<Arc<dyn Store>>().is_err());
    app.get("/_lookout").await.assert_not_found();
    app.get("/_lookout/api/status").await.assert_not_found();
}

#[tokio::test]
async fn a_bad_setting_fails_the_boot_and_names_itself() {
    let error = builder()
        .with_lookout(|config| LookoutConfig { request_sample_rate: 2.0, ..config.enabled(true) })
        .boot()
        .await
        .unwrap_err();

    assert!(error.message().contains("lookout"), "{}", error.message());
    assert!(error.message().contains("request_sample_rate"), "{}", error.message());
}

#[tokio::test]
async fn a_request_through_the_kernel_becomes_a_batch_with_its_route_and_status() {
    let app = TestApp::new(builder().with_lookout(on(true)).boot().await.unwrap()).unwrap();
    let _recording = record(&app);

    app.get("/posts/1").await.assert_ok();
    app.get("/nowhere").await.assert_not_found();

    let found = batches(&app).await;
    let post = found.iter().find(|b| b.route_name.as_deref() == Some("posts.show"));
    let post = post.unwrap_or_else(|| panic!("no posts.show batch in {found:#?}"));
    assert_eq!(post.kind, BatchKind::Request);
    assert_eq!(post.method.as_deref(), Some("GET"));
    assert_eq!(post.status, Some(200));
    assert!(found.iter().any(|b| b.status == Some(404)), "a rejected request is recorded too");
}

#[tokio::test]
async fn an_authenticated_request_carries_its_guard_and_user() {
    let app = TestApp::new(builder().with_lookout(on(true)).boot().await.unwrap()).unwrap();
    let _recording = record(&app);

    app.send(
        rainier_http::Request::builder()
            .method(rainier_http::Method::GET)
            .uri("/me")
            .header("authorization", "Bearer user-token")
            .build(),
    )
    .await
    .assert_ok();

    let found = batches(&app).await;
    let me = found.iter().find(|b| b.route_name.as_deref() == Some("me")).expect("a batch");
    assert_eq!(me.guard.as_deref(), Some("token"));
    assert_eq!(me.user_id.as_deref(), Some("8"));
}

#[tokio::test]
async fn the_dashboard_is_open_locally() {
    let app = TestApp::new(builder().with_lookout(on(true)).boot().await.unwrap()).unwrap();
    let _recording = record(&app);

    app.get("/_lookout").await.assert_ok();
    app.get("/_lookout/api/status").await.assert_ok();

    assert!(batches(&app).await.is_empty(), "browsing the dashboard records nothing");
}

#[tokio::test]
async fn outside_local_the_dashboard_is_a_404_without_the_ability() {
    // No viewers named at all: nobody.
    let app = TestApp::new(builder().with_lookout(on(false)).boot().await.unwrap()).unwrap();
    app.with_header("authorization", "Bearer admin-token")
        .get("/_lookout")
        .await
        .assert_not_found();

    // Viewers named, but the ability is not defined on their gate: nobody.
    let app = builder()
        .with_lookout(on(false))
        .with_lookout_viewers::<User>()
        .with_instance_arc(auth())
        .with_instance(Gate::<User>::new())
        .boot()
        .await
        .unwrap();
    TestApp::new(app)
        .unwrap()
        .with_header("authorization", "Bearer admin-token")
        .get("/_lookout")
        .await
        .assert_not_found();
}

#[tokio::test]
async fn outside_local_a_viewer_with_the_ability_sees_the_dashboard() {
    let app = builder()
        .with_lookout(on(false))
        .with_lookout_viewers::<User>()
        .with_instance_arc(auth())
        .with_instance(Gate::<User>::new().define_simple("viewLookout", |user: &User| user.admin))
        .boot()
        .await
        .unwrap();
    let app = TestApp::new(app).unwrap();

    let page = app.get("/_lookout").await;
    assert_eq!(page.status(), StatusCode::NOT_FOUND, "anonymous");

    let user = TestApp::new(Arc::clone(app.app())).unwrap().with_token("user-token");
    user.get("/_lookout").await.assert_not_found();

    let admin = TestApp::new(Arc::clone(app.app())).unwrap().with_token("admin-token");
    admin.get("/_lookout").await.assert_ok();
    admin.get("/_lookout/api/batches").await.assert_ok();
}

#[tokio::test]
async fn outside_local_a_viewer_predicate_decides_from_the_user_and_the_request() {
    // No `Gate` bound at all: the predicate stands in for it, and sees the
    // request — here a header a proxy would set — as well as the user.
    let app = builder()
        .with_lookout(on(false))
        .with_lookout_viewers_where(|user: &User, request: &rainier_http::Request| {
            user.admin && request.header("x-role") == Some("ops")
        })
        .with_instance_arc(auth())
        .boot()
        .await
        .unwrap();
    let app = TestApp::new(app).unwrap();
    let fresh = || TestApp::new(Arc::clone(app.app())).unwrap();

    // Unauthenticated: never asked, however the request looks.
    fresh().with_header("x-role", "ops").get("/_lookout").await.assert_not_found();

    let user = TestApp::new(Arc::clone(app.app())).unwrap().with_token("user-token");
    user.with_header("x-role", "ops").get("/_lookout").await.assert_not_found();

    let admin = TestApp::new(Arc::clone(app.app())).unwrap().with_token("admin-token");
    admin.get("/_lookout").await.assert_not_found();
    let admin = admin.with_header("x-role", "ops");
    admin.get("/_lookout").await.assert_ok();
    admin.get("/_lookout/api/batches").await.assert_ok();
}

#[tokio::test]
async fn the_framework_subscriber_lets_lookout_see_info_spans_under_a_warn_filter() {
    let app = TestApp::new(builder().with_lookout(on(true)).boot().await.unwrap()).unwrap();
    let lookout = app.resolve::<Lookout>().unwrap();

    // What boot installs globally, built with `RUST_LOG=warn` and set for
    // this thread only.
    let subscriber = TelemetrySettings::from_config(&Config::new()).subscriber(
        "production",
        "warn",
        None,
        vec![lookout.layer().boxed()],
    );
    let _recording = tracing::subscriber::set_default(subscriber);

    app.get("/slow").await.assert_ok();

    let found = batches(&app).await;
    let slow = found.iter().find(|b| b.route_name.as_deref() == Some("slow"));
    let slow = slow.unwrap_or_else(|| panic!("the info request span was filtered: {found:#?}"));
    assert_eq!(slow.counts.get(&EntryType::Query), Some(&1), "the info db.query span too");

    let store = app.resolve::<Arc<dyn Store>>().unwrap();
    let batch = store.batch(&slow.id).await.unwrap().expect("stored");
    let query = batch.entries.iter().find(|e| e.entry_type == EntryType::Query).expect("query");
    assert!(query.summary.contains("SELECT"), "{}", query.summary);
}

#[tokio::test]
async fn the_console_carries_lookouts_commands() {
    let console = crate::commands::console("app");
    for name in ["lookout:prune", "lookout:clear", "lookout:status"] {
        assert!(console.find(name).is_some(), "`{name}` should be registered");
    }

    let app = builder().with_lookout(on(true)).boot().await.unwrap();
    assert_eq!(console.run_argv(&app, ["lookout:status"]).await, rainier_console::exit::SUCCESS);
    assert_eq!(console.run_argv(&app, ["lookout:prune"]).await, rainier_console::exit::SUCCESS);

    let off = builder().boot().await.unwrap();
    assert_eq!(console.run_argv(&off, ["lookout:status"]).await, rainier_console::exit::FAILURE);
}

#[test]
fn the_gate_sees_local_exactly_when_the_configuration_says_so() {
    assert!(http::is_local(&gate_environment(true, "production")));
    assert!(!http::is_local(&gate_environment(false, "local")));
    assert_eq!(gate_environment(false, "staging"), "staging");
}

#[test]
fn the_environment_is_read_through_the_framework_and_adjusted_in_order() {
    let env = Env::from_map(
        [("LOOKOUT_ENABLED", "true"), ("LOOKOUT_STORE", "cache"), ("LOOKOUT_CAPACITY", "50")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string())),
    )
    .isolated();
    let config = Config::new();
    config.set(keys::APP_ENV, rainier_config::AppEnv::Local).unwrap();

    let lookout = configure(
        &env,
        &config,
        vec![
            Box::new(|c: LookoutConfig| c.disable(EntryType::Cache)),
            Box::new(|c: LookoutConfig| c.local(false)),
        ],
    )
    .unwrap();

    assert!(lookout.enabled);
    assert_eq!(lookout.store, config::StoreKind::Cache { store: None, capacity: 50 });
    assert!(!lookout.watcher(EntryType::Cache).enabled);
    assert!(!lookout.local, "the adjustment wins over APP_ENV");

    let bad = Env::from_map([("LOOKOUT_STORE".to_string(), "disk".to_string())]).isolated();
    let error = configure(&bad, &Config::new(), Vec::new()).unwrap_err();
    assert!(error.message().contains("LOOKOUT_STORE"), "{}", error.message());
}
