//! Lookout, end to end, offline — nothing to install, nothing to connect to.
//!
//! What it shows:
//!
//! 1. an application booted with Lookout on and the in-memory store — the
//!    framework builds the recorder, puts its layer in the subscriber, its
//!    middleware in the global stack and its dashboard on the router;
//! 2. four requests through the real kernel: a page, a page with a slow
//!    query in it, a route that fails, and one that writes models through a
//!    plain repository — each write a `ModelChanged` on the event bus, which
//!    Lookout hears and the Models page lists;
//! 3. what Lookout recorded for each, read back through the store the
//!    dashboard reads;
//! 4. where the dashboard is, and that browsing it records nothing.
//!
//! ```sh
//! cargo run -p rainier-framework --example lookout --features lookout
//! # …and keep serving, to click through the dashboard:
//! cargo run -p rainier-framework --example lookout --features lookout -- --serve
//! ```
//!
//! # In a deployment
//!
//! Build with the `lookout` feature and set the variables; no code is needed
//! beyond naming who may see the dashboard:
//!
//! ```env
//! LOOKOUT_ENABLED=true
//! LOOKOUT_STORE=database        # or cache, or memory
//! ```
//!
//! See `docs/lookout.md`.

use std::time::Duration;

use rainier_framework::database::testing::{fake_database, MemoryConnection};
use rainier_framework::database::{Criteria, EntityRepository, Repository};
use rainier_framework::lookout::config::WatcherConfig;
use rainier_framework::lookout::{BatchQuery, EntryType, Lookout, Store};
use rainier_framework::support::{Error, Result};
use rainier_framework::testing::TestApp;
use rainier_framework::Rainier;

/// A model, written through a repository built without `with_events`: no
/// typed hook fires, and the write is on the bus all the same.
#[derive(rainier_orm::Entity, Clone, Debug)]
#[orm(table = "posts")]
struct Post {
    #[orm(pk)]
    id: u64,
    title: String,
}

impl rainier_framework::database::Model for Post {}

#[tokio::main]
async fn main() -> Result<()> {
    // A stand-in database that accepts every write, so the demo needs no
    // server; a real connection announces its writes the same way.
    let (database, _) = fake_database(MemoryConnection::new(rainier_orm::Dialect::Sqlite));
    let posts = EntityRepository::<Post>::new(database.clone());

    // `with_lookout` adjusts what `LOOKOUT_*` set. Here it turns Lookout on
    // and marks the environment local — where every batch is kept and the
    // dashboard needs no login — and makes 20 ms a slow query, so the demo
    // does not have to wait 100.
    let app = Rainier::new(".")
        .with_lookout(|lookout| {
            lookout
                .enabled(true)
                .local(true)
                .watch(EntryType::Query, |w| WatcherConfig { slow_ms: Some(20), ..w })
        })
        .with_database(database)
        .with_routes(move |router| {
            router.get("/", || async { "home" }).name("home");
            router
                .post("/posts", move || {
                    let posts = posts.clone();
                    async move {
                        posts.create(Post { id: 7, title: "Hello".into() }).await?;
                        posts
                            .update_matching(
                                Criteria::new().where_eq("title", "Hello"),
                                vec![("title".into(), "Hello, world".into())],
                            )
                            .await?;
                        Ok::<_, Error>("saved")
                    }
                })
                .name("posts.store");
            router
                .get("/posts/{post}", || async {
                    // What the database layer's `db.query` span looks like;
                    // with a real connection the ORM opens it for you.
                    query("SELECT * FROM posts WHERE id = ?", 30).await;
                    query("SELECT * FROM comments WHERE post_id = ?", 1).await;
                    "a post"
                })
                .name("posts.show");
            router
                .get("/boom", || async {
                    Err::<&'static str, _>(Error::internal("the widget fell over"))
                })
                .name("boom");
        })
        .boot()
        .await?;

    let lookout = app.resolve::<Lookout>()?;
    let store = app.resolve::<std::sync::Arc<dyn Store>>()?;
    let path = lookout.config().dashboard.path.clone();
    let client = TestApp::new(app.clone())?;

    println!("\n== Four requests ==");
    for uri in ["/", "/posts/1", "/boom"] {
        let response = client.get(uri).await;
        println!("GET {uri} -> {}", response.status());
    }
    println!("POST /posts -> {}", client.post("/posts", &serde_json::json!({})).await.status());

    // Browsing the dashboard is not itself recorded.
    let dashboard = client.get(&path).await;
    println!("GET {path} -> {} (not recorded)", dashboard.status());

    // The writer flushes every second on its own; this does it now.
    lookout.flush().await;

    println!("\n== What Lookout recorded ({} store) ==", store.name());
    let mut batches = store.batches(BatchQuery::default()).await?;
    batches.reverse(); // oldest first, to read in order
    for summary in &batches {
        let mut flags = Vec::new();
        if summary.has_slow {
            flags.push("slow");
        }
        if summary.has_exception {
            flags.push("exception");
        }
        let counts: Vec<String> =
            summary.counts.iter().map(|(kind, n)| format!("{n} {kind}")).collect();
        println!(
            "{:<28} {:>7.1} ms  [{}]  {}",
            summary.summary,
            summary.duration_us as f64 / 1000.0,
            counts.join(", "),
            flags.join(" "),
        );

        if let Some(batch) = store.batch(&summary.id).await? {
            for entry in batch.entries.iter().filter(|e| e.entry_type != EntryType::Request) {
                let took = entry.duration_us.map(|us| format!("{:.1} ms", us as f64 / 1000.0));
                println!(
                    "    {:<10} {:<48} {}",
                    entry.entry_type.to_string(),
                    entry.summary,
                    took.unwrap_or_default()
                );
            }
        }
    }

    println!("\n== The dashboard ==");
    if std::env::args().any(|arg| arg == "--serve") {
        // The demo's batches are in the store; serve the same kernel so the
        // dashboard can be browsed, until Ctrl-C.
        let kernel = app.resolve::<rainier_framework::server::Kernel>()?;
        let options =
            rainier_framework::server::ServerOptions::default().bind_to("127.0.0.1", 8017)?;
        println!("Serving: open http://127.0.0.1:8017{path} (Ctrl-C to stop)");
        rainier_framework::server::Server::from_arc(kernel).with_options(options).run().await?;
    }
    println!("Run with `-- --serve`, or `serve` in an app, and open http://127.0.0.1:8000{path}");
    println!("Outside APP_ENV=local it is a 404 unless the viewer has `viewLookout`.");
    println!("Dropped batches (channel full): {}", lookout.dropped());

    rainier_framework::lookout::shutdown(&app).await;
    Ok(())
}

/// A `db.query` span lasting `ms` milliseconds.
async fn query(sql: &str, ms: u64) {
    use tracing::Instrument as _;
    let span = tracing::info_span!("db.query", db.system.name = "sqlite", db.query.text = sql);
    tokio::time::sleep(Duration::from_millis(ms)).instrument(span).await;
}
