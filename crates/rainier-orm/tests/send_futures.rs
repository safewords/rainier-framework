//! Every public async API must produce a `Send` future.
//!
//! This is not a style preference — it is what makes the crate usable from a
//! multi-threaded server. A handler that awaits a `!Send` future cannot be
//! `tokio::spawn`ed, so a web framework built on this crate would be unable to
//! call `repo::` from a request handler at all.
//!
//! The hazard is easy to reintroduce: `sea_query`'s statement builders hold
//! `Rc<dyn Iden>`, so a statement that is merely *in scope* across an `.await`
//! (even if never used again) is captured in the generated future and makes it
//! `!Send`. The fix is always the same — build and render the statement inside
//! a scope that ends before the await.
//!
//! These assertions are compile-time: if a future stops being `Send`, this
//! test file fails to build.

use rainier_orm::sea_query::Value;
use rainier_orm::{repo, Dialect, Entity, ExecOutcome, Executor, Row};

#[derive(Entity, Clone)]
#[orm(table = "widgets")]
struct Widget {
    #[orm(pk, auto_increment)]
    id: u64,
    #[orm(unique)]
    name: String,
    active: bool,
}

/// A `Send + Sync` executor whose own futures are `Send`, so anything `!Send`
/// in an assertion below comes from the layer being tested rather than here.
struct SendExecutor;

impl Executor for SendExecutor {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn fetch_all(
        &self,
        _sql: &str,
        _params: Vec<Value>,
    ) -> rainier_orm::Result<Vec<Box<dyn Row>>> {
        Ok(Vec::new())
    }

    async fn execute(&self, _sql: &str, _params: Vec<Value>) -> rainier_orm::Result<ExecOutcome> {
        Ok(ExecOutcome::default())
    }
}

fn assert_send<T: Send>(_value: T) {}

#[test]
fn the_executor_used_by_these_assertions_is_itself_send() {
    let exec = SendExecutor;
    assert_send(exec.fetch_all("SELECT 1", Vec::new()));
    assert_send(SendExecutor.execute("SELECT 1", Vec::new()));
}

#[test]
fn repo_crud_futures_are_send() {
    let exec = SendExecutor;
    let widget = Widget { id: 0, name: "a".into(), active: true };

    assert_send(repo::insert(&exec, &widget));
    assert_send(repo::upsert(&exec, &widget, &["name"], &["active"]));
    assert_send(repo::find_by_pk::<Widget, _, _>(&exec, 1_i64));
    assert_send(repo::find_by::<Widget, _, _>(&exec, "name", "a"));
    assert_send(repo::find_one_by::<Widget, _, _>(&exec, "name", "a"));
    assert_send(repo::all::<Widget, _>(&exec));
    assert_send(repo::update(&exec, &widget));
    assert_send(repo::delete_by_pk::<Widget, _, _>(&exec, 1_i64));
}

#[test]
fn query_builder_terminal_futures_are_send() {
    let exec = SendExecutor;

    assert_send(repo::query::<Widget>().where_eq("active", true).all(&exec));
    assert_send(repo::query::<Widget>().where_eq("active", true).first(&exec));
    assert_send(repo::query::<Widget>().where_eq("active", true).count(&exec));
    assert_send(repo::query::<Widget>().where_eq("active", true).delete(&exec));
    assert_send(
        repo::query::<Widget>().where_eq("id", 1_i64).update(&exec, vec![("name", "b".into())]),
    );
    assert_send(repo::query::<Widget>().where_eq("id", 1_i64).increment(&exec, "id", 1));
    assert_send(
        repo::query::<Widget>()
            .where_eq("name", "a")
            .first_or_create(&exec, Widget { id: 0, name: "a".into(), active: true }),
    );
}

#[test]
fn query_builder_futures_are_send_with_joins_and_paging() {
    let exec = SendExecutor;

    assert_send(
        repo::query::<Widget>()
            .join("owners", "id", "widget_id")
            .left_join("tags", "id", "widget_id")
            .where_like("name", "a%")
            .where_in("id", vec![1_i64, 2])
            .where_not_null("name")
            .order_by_desc("id")
            .limit(10)
            .offset(20)
            .all(&exec),
    );
}

#[test]
fn cursor_futures_are_send() {
    let exec = SendExecutor;
    let mut cursor = repo::cursor::<Widget, _>(&exec, 100);
    assert_send(async move { cursor.next_page().await });
}

#[test]
fn change_tracking_futures_are_send() {
    use rainier_orm::active::Tracked;

    let exec = SendExecutor;
    assert_send(Tracked::<Widget>::load(&exec, 1_i64));
    assert_send(async move {
        let mut tracked = Tracked::new(Widget { id: 1, name: "a".into(), active: true });
        tracked.name = "b".into();
        tracked.save(&exec).await
    });
}

/// `Migrator::run` is the documented exception — see `migrate::StepFuture`.
///
/// It boxes each step's future behind `dyn`, which erases auto traits, and the
/// bound cannot be added: `CreateTable` implements `Migration<X>` for *every*
/// `X: Executor`, so it would have to produce a `Send` future for every
/// executor, and that is unknowable generically without return-type notation.
///
/// This test pins the shape of the workaround rather than asserting the
/// property, so the escape hatch keeps working: render synchronously, then
/// await plain `execute` calls. That is what a caller needing a `Send` future
/// — a service provider's `boot`, say — should do.
#[test]
fn migrations_can_be_rendered_synchronously_and_run_as_a_send_future() {
    use rainier_orm::ddl::{Column, Migration};
    use rainier_orm::schema;

    // Rendering is an ordinary function returning owned `String`s.
    let create: Vec<String> = schema::schema_ddl::<Widget>(Dialect::Sqlite);
    let alter: Vec<String> = Migration::new("0002_add_note")
        .add_column("widgets", Column::new("note", rainier_orm::ColumnType::Text))
        .create_index("idx_widgets_note", "widgets", ["note"])
        .render(Dialect::Sqlite);

    assert!(!create.is_empty());
    assert!(!alter.is_empty());
    assert_send(create.clone());

    // …so executing them holds nothing `!Send` across an await.
    let exec = SendExecutor;
    assert_send(async move {
        for sql in create.into_iter().chain(alter) {
            exec.execute(&sql, Vec::new()).await?;
        }
        Ok::<(), anyhow::Error>(())
    });
}
