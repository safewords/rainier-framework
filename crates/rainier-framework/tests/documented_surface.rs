//! The surface `docs/` promises, named once so a rename cannot quietly make a
//! page wrong.
//!
//! Every path here is one a documentation page tells a reader to write. A doc
//! example is not compiled by anything — `cargo test --doc` only sees the ones
//! inside the crates — so this is what stops `docs/testing.md` describing a
//! method that was renamed two releases ago.
//!
//! It asserts nothing about behaviour. Failing to compile *is* the assertion.

use std::sync::Arc;
use std::time::Duration;

use rainier_framework::config::{Config, Env};
use rainier_framework::console_kernel::io;
use rainier_framework::container::{scope_facade_application, Application};
use rainier_framework::observability::TelemetrySettings;
use rainier_framework::prelude::*;
use rainier_framework::queue::{Job, JobContext, QueueManager};
use rainier_framework::testing::{TestApp, TestResponse};
use rainier_framework::{keys, DispatcherExt, FromEvent};
use rainier_middleware::{MiddlewareStack, ThrottleRequests};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// `docs/testing.md` — the harness.
#[allow(dead_code, reason = "compiling is the point")]
async fn the_test_harness(app: Arc<Application>) {
    let app = TestApp::new(app).unwrap().with_token("t").with_header("accept-language", "fr");

    app.get("/api/posts").await;
    app.post("/api/posts", &json!({ "title": "Hello" })).await;
    app.put("/api/posts/1", &json!({})).await;
    app.patch("/api/posts/1", &json!({})).await;
    app.delete("/api/posts/1").await;
    app.post_empty("/api/logout").await;

    let response: TestResponse = app
        .send(
            app.request(Method::POST, "/api/posts")
                .header("content-type", "application/x-www-form-urlencoded")
                .body("title=Hello")
                .build(),
        )
        .await;

    response
        .assert_ok()
        .assert_status(StatusCode::OK)
        .assert_created()
        .assert_no_content()
        .assert_not_found()
        .assert_unauthorized()
        .assert_forbidden()
        .assert_invalid()
        .assert_json_path("data.0.title", "Hello")
        .assert_json_missing("author.password")
        .assert_contains("Hello")
        .assert_header("content-type", "application/json")
        .assert_header_missing("x-secret");

    let _ = (response.status(), response.text(), response.header("etag"), response.json());
    let _: serde_json::Value = response.json_as();

    let _ = app.resolve::<Config>();
    let _ = app.app();
}

/// `docs/testing.md` — the facade scope and an isolated environment.
#[allow(dead_code, reason = "compiling is the point")]
fn scoping_and_isolation(app: Arc<Application>) {
    let _scope = scope_facade_application(app);

    let env = Env::from_map([("MAIL_DRIVER", "log")]);
    let _ = (env.get("PATH"), env.is_isolated());
    let _ = Env::parse("MAIL_DRIVER=log").isolated();
}

/// `docs/facades.md` — the three scopes, and carrying one into a spawn.
#[allow(dead_code, reason = "compiling is the point")]
async fn facade_scopes(app: Arc<Application>, kernel: Arc<rainier_server::Kernel>) {
    use rainier_framework::container::{
        set_facade_application, spawn_with_facades, task_facade_application,
        with_facade_application,
    };

    set_facade_application(Arc::clone(&app));
    with_facade_application(Arc::clone(&app), async { task_facade_application() }).await;
    let _ = spawn_with_facades(async {}).await;

    let _ = rainier_server::Server::from_arc(kernel).for_application(app);
}

/// `docs/middleware.md` — the three that arrived in 1.0.1.
#[allow(dead_code, reason = "compiling is the point")]
fn the_new_middleware() {
    let _ = Timeout::seconds(30);
    let _ = Timeout::millis(200);
    let _ = Timeout::new(Duration::from_secs(30)).limit();
    let _ = Compress::new().min_size(4096).level(9);
    let _ = MethodOverride::new().trusting_the_header();
}

/// `docs/console.md` — talking to whoever is running the command.
#[allow(dead_code, reason = "compiling is the point")]
fn console_io() -> Result<()> {
    io::table(&["ID", "Queue"], &[vec!["1".into(), "mail".into()]]);
    let _: String = io::table_to_string(&["ID"], &[]);

    if io::is_interactive() {
        let _ = io::ask("Name?")?;
        let _ = io::ask_with_default("Queue?", "default")?;
        let _ = io::secret("Password:")?;
        let _ = io::confirm("Send it?", true)?;
        let _ = io::confirm_by_typing("This drops every table.", "production")?;
    }
    Ok(())
}

/// `docs/errors.md` — the constructors 1.0.1 added.
#[allow(dead_code, reason = "compiling is the point")]
fn the_new_errors() -> Vec<Error> {
    vec![
        Error::request_timeout("too slow"),
        Error::too_many_requests("slow down"),
        Error::service_unavailable("the index is unreachable"),
    ]
}

/// `docs/helpers.md` — which build is running.
#[allow(dead_code, reason = "compiling is the point")]
fn the_build() {
    let info: BuildInfo = build_info!();
    let _ = (info.short_commit(), info.is_debug(), info.summary(), info.to_string());
}

/// `docs/observability.md` — the log format.
#[allow(dead_code, reason = "compiling is the point")]
fn logging(config: &Config) {
    let settings = TelemetrySettings::from_config(config);
    let _: bool = settings.install_logging("production", "info");
    let _ = LogFormat::Auto.resolve("production").is_structured();
    let _ = config.setting(keys::LOG_FORMAT);
}

/// `docs/configuration.md` — the keys 1.0.1 added.
#[allow(dead_code, reason = "compiling is the point")]
fn the_new_keys(config: &Config) -> Result<()> {
    config.set(keys::SERVER_REQUEST_TIMEOUT_SECS, 30u64)?;
    config.set(keys::SERVER_COMPRESSION, true)?;
    config.set(keys::LOG_FORMAT, LogFormat::Json)?;
    Ok(())
}

struct UserRegistered {
    user_id: u64,
}

#[derive(Serialize, Deserialize)]
struct SendWelcomeEmail {
    user_id: u64,
}

#[async_trait::async_trait]
impl Job for SendWelcomeEmail {
    const NAME: &'static str = "send-welcome-email";

    async fn handle(&self, _context: &JobContext) -> Result<()> {
        Ok(())
    }
}

impl FromEvent<UserRegistered> for SendWelcomeEmail {
    fn from_event(event: &UserRegistered) -> Self {
        Self { user_id: event.user_id }
    }
}

/// `docs/events.md` — a listener that goes on the queue.
#[allow(dead_code, reason = "compiling is the point")]
fn queued_listeners(events: &rainier_framework::events::Dispatcher, queue: Arc<QueueManager>) {
    events.listen_queued::<UserRegistered, SendWelcomeEmail>();
    events.listen_queued_on::<UserRegistered, SendWelcomeEmail>(queue);
}

/// `docs/scheduling.md` — the lock check.
#[allow(dead_code, reason = "compiling is the point")]
fn the_lock_check(app: &Application) -> Result<()> {
    rainier_framework::scheduling::warn_if_locks_are_not_shared(app);
    rainier_framework::scheduling::assert_locks_are_shared(app)?;

    let schedule = app.resolve::<rainier_framework::scheduler::Schedule>()?;
    let _: Vec<String> = schedule.tasks_needing_shared_locks();

    let locks = app.resolve::<rainier_framework::cache::LockManager>()?;
    let _ = locks.is_shared();
    Ok(())
}

/// `docs/routing.md` — the table, without compiling middleware.
#[allow(dead_code, reason = "compiling is the point")]
fn describing_routes(router: &Router) {
    for summary in router.describe() {
        let _ = (summary.methods, summary.uri, summary.name, summary.middleware);
    }
}

/// `docs/cache.md` — `remember`, and asking a store whether it is shared.
#[allow(dead_code, reason = "compiling is the point")]
async fn remembering(cache: &rainier_framework::cache::MemoryCache) -> Result<()> {
    let _: u64 = cache.remember("k", Some(Duration::from_secs(60)), || async { Ok(7) }).await?;
    let _: u64 = cache.remember_forever("k", || async { Ok(7) }).await?;

    let _ = rainier_framework::cache::Cache::is_shared(cache);
    let _ = rainier_framework::cache::LockManager::new(Arc::new(
        rainier_framework::cache::MemoryCache::new(),
    ))
    .declared_shared();
    Ok(())
}

/// `docs/hashing.md` — the timing branch and an account with no password.
#[allow(dead_code, reason = "compiling is the point")]
fn hashing(hasher: &rainier_framework::auth::Argon2Hasher) {
    use rainier_framework::auth::Hasher;

    hasher.dummy_verify("whatever they typed");
    let unusable = hasher.unusable();
    let _ = (hasher.is_unusable(&unusable), hasher.verify("x", &unusable));
}

/// `docs/responses.md` — reading a response outside the harness.
#[allow(dead_code, reason = "compiling is the point")]
async fn reading_a_response(response: Response) -> Result<()> {
    let mut response = response;
    let _ = response.take_body();
    let _: String = response.into_string().await?;
    Ok(())
}

/// `docs/middleware.md` — the throttle, keyed and shared.
#[allow(dead_code, reason = "compiling is the point")]
fn throttling() -> MiddlewareStack {
    let throttle = ThrottleRequests::per_minute(5)
        .named("login")
        .keyed_by(|request| request.input("email"))
        .stored_in(Arc::new(rainier_middleware::MemoryRateLimitStore::new()));

    let _ = (throttle.is_shared(), throttle.max_attempts(), throttle.limiter_name());
    let _ = ThrottleRequests::per_hour(1000);
    let _ = ThrottleRequests::per_day(10_000);

    rainier_framework::limits::shared(ThrottleRequests::per_minute(5))
}

/// `docs/urls.md` — signed links.
#[allow(dead_code, reason = "compiling is the point")]
fn signing(signed: &rainier_framework::SignedUrls) -> Result<()> {
    let _ = signed.route("unsubscribe", &[("user", "42")])?;
    let _ = signed.temporary_route("verify", 4_102_444_800, &[("user", "42")])?;
    let _ = signed.absolute_route("verify", &[("user", "42")])?;
    let _ = signed.temporary_absolute_route("verify", 4_102_444_800, &[])?;
    let _ = signed.sign("/verify?user=42")?;
    let _ = rainier_framework::ValidateSignature::resolved();
    Ok(())
}

/// `docs/authentication.md` — abilities, confirmation, challenges.
#[allow(dead_code, reason = "compiling is the point")]
async fn the_auth_surface(request: &Request, cache: Arc<dyn rainier_cache::Cache>) -> Result<()> {
    use rainier_auth::{
        Abilities, AbilitiesRequestExt, Challenges, ConfirmPassword, RequireAbility,
    };

    let abilities = Abilities::parse("posts:read,posts:*");
    let _ = (abilities.can("posts:write"), abilities.cannot("users:read"));
    let _ = (abilities.can_all(["a"]), abilities.can_any(["a"]), abilities.is_unrestricted());
    let _ = (Abilities::everything(), Abilities::none(), abilities.to_csv());
    let _ = (RequireAbility::any(["posts:read"]), RequireAbility::all(["posts:read"]));
    let _ = (request.token_abilities(), request.token_can("posts:read"));

    let _ = ConfirmPassword::within(Duration::from_secs(900));
    let _ = ConfirmPassword::recently().is_confirmed(request);
    let _ = ConfirmPassword::confirmed_ago(request);
    ConfirmPassword::forget(request);

    let challenges =
        Challenges::new(cache).lasting(Duration::from_secs(900)).max_attempts(5).digits(6);
    let code = challenges.issue(42, "email-change").await?;
    challenges.issue_code(42, "email-change", &code).await?;
    let _ = challenges.is_pending(42, "email-change").await?;
    let _ = challenges.attempts_remaining(42, "email-change").await?;
    challenges.consume(42, "email-change", &code).await?;
    challenges.cancel(42, "email-change").await?;
    Ok(())
}

/// `docs/hashing.md` — reading an inherited scheme.
#[allow(dead_code, reason = "compiling is the point")]
fn legacy_hashes() {
    struct Whatever;

    impl rainier_auth::LegacyVerifier for Whatever {
        fn name(&self) -> &'static str {
            "whatever"
        }
        fn recognises(&self, hashed: &str) -> bool {
            hashed.starts_with("x$")
        }
        fn verify(&self, _plain: &str, _hashed: &str) -> bool {
            false
        }
    }

    let hasher = rainier_auth::Argon2Hasher::new().with_legacy(Whatever);
    let _ = hasher.legacy_schemes();
}

/// `docs/observability.md` — health checks.
#[allow(dead_code, reason = "compiling is the point")]
async fn health_checks(app: Arc<Application>) {
    let health = rainier_framework::Health::new()
        .register("database", |_app| async { Ok(()) })
        .timeout(Some(Duration::from_secs(5)))
        .describing_build(build_info!());

    let _ = health.names();

    let report = health.run(Arc::clone(&app)).await;
    let _ = (report.is_healthy(), report.status_code(), report.failing());
    let _ = health.render(app).await;
    let _ = rainier_framework::health::endpoint().await;
}

/// `docs/testing.md` — factories.
#[allow(dead_code, reason = "compiling is the point")]
fn factories() {
    use rainier_database::HasFactory as _;

    #[derive(Clone, Default)]
    struct Row {
        name: String,
    }

    impl rainier_database::HasFactory for Row {
        fn factory() -> rainier_database::Factory<Self> {
            rainier_database::Factory::new(|_| Row::default())
        }
    }

    let factory = Row::factory().count(3).state(|row| row.name = "x".into());
    let _ = factory.clone().sequence(|row, i| row.name = format!("row-{i}")).make();
    let _ = factory.make_one();
}

/// `docs/http-client.md` — the client and its fake.
#[allow(dead_code, reason = "compiling is the point")]
async fn the_http_client() -> Result<()> {
    use rainier_framework::http_client::{Backoff, Http, HttpResponse};

    let fake = Http::fake();
    fake.responding(200, "{}").responding_with_header(201, "{}", "etag", "abc");

    let response: HttpResponse = Http::post("https://example.com/hook")
        .bearer("token")
        .accept_json()
        .header("x-signature", "abc")
        .json(&serde_json::json!({ "id": 1 }))?
        .timeout(Duration::from_secs(10))
        .retry(3, Backoff::exponential())
        .send()
        .await?;

    let _ = (response.status(), response.is_success(), response.header("etag"), response.text());
    let _ = response.error_for_status()?;

    let _ = Http::get("https://x").without_timeout().form(&[("a", "b")]).send().await;
    let _ = Http::put("https://x").body("x").send().await;
    let _ = Http::patch("https://x").send().await;
    let _ = Http::delete("https://x").send().await;
    let _ = Http::request("HEAD", "https://x").send().await;

    Http::assert_sent(|request| request.url_contains("/hook"));
    Http::assert_not_sent(|request| request.method() == "TRACE");
    fake.assert_sent_count(fake.count());
    let _ = fake.recorded().first().map(|request| (request.url(), request.body(), request.json()));
    fake.clear();
    Http::clear();
    Ok(())
}

/// `docs/encryption.md` — JWTs.
#[cfg(feature = "jwt")]
#[allow(dead_code, reason = "compiling is the point")]
fn tokens() -> Result<()> {
    use rainier_crypt::jwt::{Jwt, JwtAlgorithm, JwtKey, JwtKeyRing};

    let key = JwtKey::generate_es256("current")?;
    let _ = (key.kid(), key.algorithm(), key.to_jwk(), JwtAlgorithm::Rs256.name());

    let ring = JwtKeyRing::new(key).with_previous(JwtKey::generate_es256("previous")?);
    let _ = (ring.ids(), ring.jwks(), ring.current().is_some(), ring.find("previous").is_some());

    let jwt = Jwt::new(ring).issued_by("https://id.example.com").for_audience("api").leeway(60);
    let token = jwt.sign(&serde_json::json!({ "sub": "42", "exp": 4_102_444_800i64 }))?;
    let _: serde_json::Value = jwt.verify(&token)?;
    let _ = (Jwt::kid_of(&token), jwt.jwks(), jwt.ring().ids());
    Ok(())
}

/// `docs/encryption.md`'s PHP envelope, and `docs/cache.md`'s KV.
#[allow(dead_code, reason = "compiling is the point")]
fn envelopes() {
    use rainier_crypt::{CryptScheme, PhpEncrypter};

    let keys = rainier_crypt::KeyRing::new(rainier_crypt::Key::generate());
    let _ = PhpEncrypter::new(keys.clone());
    let _ = (CryptScheme::Native, CryptScheme::Php);
    let _ = keys.all().count();

    let _ = rainier_cache::CacheDriver::Kv.can_hold_a_lock();
    let _ = rainier_cache::Cache::supports_atomic_add(&rainier_cache::MemoryCache::new());
}

/// `docs/kafka.md` — the connector and the client.
#[cfg(feature = "kafka")]
#[allow(dead_code, reason = "compiling is the point")]
async fn the_kafka_client() -> Result<()> {
    use rainier_drivers::kafka::{
        partition_for_key, KafkaClient, KafkaConnector, KafkaCredentials, KafkaOffset, KafkaRecord,
        SaslMechanism, DEFAULT_PORT, DEFAULT_TIMEOUT,
    };

    let connector = KafkaConnector::parse("kafka-1:9092,kafka-2")
        .with_client_id("checkout")
        .with_timeout(Duration::from_secs(10))
        .with_max_message_size(1024 * 1024)
        .with_tls()
        .with_credentials(KafkaCredentials::new(SaslMechanism::ScramSha512, "svc", "secret"));

    let _ = (connector.brokers(), connector.is_tls(), connector.timeout());
    let _ =
        (connector.credentials().map(KafkaCredentials::username), DEFAULT_PORT, DEFAULT_TIMEOUT);
    let _ = (SaslMechanism::Plain.name(), partition_for_key(b"user-7", 12));

    let client = Arc::new(KafkaClient::connect(&connector).await?);

    client.create_topic("broadcasts", 6, 3).await?;
    let _ = (client.topics().await?, client.partitions("broadcasts").await?);

    let placed = client
        .produce("broadcasts", vec![KafkaRecord::new("body").keyed("k").header("event", "E")])
        .await?;
    let _ = client.produce_to("broadcasts", 0, vec![KafkaRecord::new("body")]).await?;
    let _ = placed.first().map(|position| (position.topic.clone(), position.offset));

    let fetched = client.fetch("broadcasts", 0, 0, 1024 * 1024, Duration::from_secs(1)).await?;
    if let Some(fetch) = fetched {
        let _ = (fetch.is_empty(), fetch.next_offset(), fetch.high_watermark);
        let _ = fetch.messages.first().map(|m| (m.header("event"), m.position(), m.offset));
    }

    let _ = client.offset("broadcasts", 0, KafkaOffset::Earliest).await?;
    let _ = client.offset("broadcasts", 0, KafkaOffset::Latest).await?;
    Ok(())
}

/// `docs/kafka.md` — broadcasting over a topic, and relaying it back.
#[cfg(feature = "kafka")]
#[allow(dead_code, reason = "compiling is the point")]
async fn kafka_broadcasting(client: Arc<rainier_drivers::kafka::KafkaClient>) -> Result<()> {
    use rainier_broadcast::kafka::{KafkaBroadcaster, KafkaRelay, DEFAULT_TOPIC};
    use rainier_framework::relay::{self, SocketFanOut};
    use rainier_websocket::Rooms;

    let broadcaster =
        KafkaBroadcaster::new(Arc::clone(&client)).on_topic(DEFAULT_TOPIC).with_prefix("checkout_");
    let _ = broadcaster.topic();

    let relay = KafkaRelay::new(Arc::clone(&client), "broadcasts")
        .from_earliest()
        .with_max_wait(Duration::from_secs(1));
    let _ = relay.topic();

    let mut cursor = relay.subscribe().await?;
    let _ = (cursor.len(), cursor.is_empty(), cursor.positions().to_vec());

    let rooms = Arc::new(Rooms::new());
    let fan_out = SocketFanOut::new(Arc::clone(&rooms))
        .naming_rooms(|channel| Some(channel.trim_start_matches("private-").to_string()));

    for broadcast in relay.poll(&mut cursor).await? {
        let _ = (broadcast.should_reach("1.7"), broadcast.wire_payload(), &broadcast.channel);
        let _ = fan_out.deliver(&broadcast);
    }

    let _ = fan_out.rooms();
    let _ = relay::run(&relay, &fan_out).await;

    // A `JoinHandle`, which is a future — `let _` on one would drop it here.
    let running = relay::spawn(relay, fan_out);
    running.abort();
    Ok(())
}

/// `docs/kafka.md` — a socket identity that survives more than one replica.
#[allow(dead_code, reason = "compiling is the point")]
fn socket_identities(socket: &rainier_websocket::Socket) {
    let _ = (socket.identity(), socket.id());
    let _ = rainier_websocket::socket_from_identity(&socket.identity());
    let _ = rainier_websocket::instance_id();
}

/// `docs/kafka.md` — jobs on a log.
#[cfg(feature = "kafka")]
#[allow(dead_code, reason = "compiling is the point")]
async fn kafka_jobs(
    client: Arc<rainier_drivers::kafka::KafkaClient>,
    locks: rainier_framework::cache::LockManager,
) -> Result<()> {
    use rainier_queue::kafka::FAILED_SUFFIX;
    use rainier_queue::KafkaQueue;

    let queue = KafkaQueue::new(client, locks)?
        .in_group("checkout-workers")
        .with_topic_prefix("jobs.")
        .with_lease(Duration::from_secs(300))
        .with_max_wait(Duration::from_millis(100));

    let _ = (queue.group(), queue.topic_for("default"), queue.failed_topic_for("default"));
    let _ = (queue.client(), FAILED_SUFFIX);
    queue.release_partitions().await?;
    Ok(())
}

/// `docs/kafka.md` — building it all from configuration.
#[cfg(feature = "kafka")]
#[allow(dead_code, reason = "compiling is the point")]
async fn kafka_from_config(
    config: &Config,
    locks: rainier_framework::cache::LockManager,
    events: &rainier_framework::events::Dispatcher,
) -> Result<()> {
    use rainier_framework::kafka;

    let _ = kafka::connector(config)?;

    let client = kafka::client(config).await?;
    let _ = kafka::broadcaster(config, Arc::clone(&client));
    let _ = kafka::relay(config, Arc::clone(&client));
    let _ = kafka::queue(config, Arc::clone(&client), locks)?;

    #[derive(Serialize)]
    struct OrderShipped {
        order_id: u64,
    }

    kafka::publish_events::<OrderShipped>(events, client, "orders", |event| {
        event.order_id.to_string()
    });

    let _ = (
        config.get(keys::KAFKA_BROKERS),
        config.get(keys::KAFKA_GROUP),
        config.get(keys::KAFKA_TOPIC_PREFIX),
        config.get(keys::KAFKA_BROADCAST_TOPIC),
        config.get(keys::KAFKA_TLS),
        config.get(keys::KAFKA_USERNAME),
        config.get(keys::KAFKA_PASSWORD),
        config.get(keys::KAFKA_SASL_MECHANISM),
    );
    Ok(())
}

#[test]
fn the_documented_surface_exists() {
    // Every function above had to compile for this to run at all.
}
