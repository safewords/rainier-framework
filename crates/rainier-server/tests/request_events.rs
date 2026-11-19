//! A request the kernel handles is a unit of work on the bus: the handler
//! runs inside it, `RequestHandled` is published in it, and `UnitCompleted`
//! follows once everything published in it has been delivered.

use std::sync::{Arc, Mutex};

use rainier_container::Application;
use rainier_events::{Activity, Context, Dispatcher, UnitCompleted, UnitKind};
use rainier_http::{Request, Response};
use rainier_routing::Router;
use rainier_server::{Kernel, RequestHandled};

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn kernel(events: &Arc<Dispatcher>) -> Kernel {
    let mut router = Router::new();
    router
        .get("/posts/{post}", || async {
            // The handler sees the request's unit.
            let unit = Context::current_unit().expect("a unit is current in the handler");
            assert_eq!(unit.kind(), UnitKind::Request);
            Response::ok(unit.path().unwrap_or_default().to_string())
        })
        .name("posts.show");
    router.get("/boom", || async {
        Err::<Response, _>(rainier_support::Error::internal("the database is down"))
    });
    Kernel::new(router.compile(&Application::new(".")).expect("compiles"))
        .with_events(Arc::clone(events))
}

#[derive(Debug)]
enum Seen {
    Handled(Box<RequestHandled>, Option<u64>),
    Completed(UnitCompleted),
}

fn watch(events: &Arc<Dispatcher>) -> Arc<Mutex<Vec<Seen>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (a, b) = (Arc::clone(&seen), Arc::clone(&seen));
    events
        .subscriber("test")
        .on(move |e: &RequestHandled, cx: &Context| {
            a.lock().unwrap().push(Seen::Handled(Box::new(e.clone()), cx.unit().map(|u| u.id())));
            Ok(())
        })
        .on(move |e: &UnitCompleted, _| {
            b.lock().unwrap().push(Seen::Completed(e.clone()));
            Ok(())
        })
        .start();
    seen
}

async fn completed(events: &Arc<Dispatcher>, seen: &Arc<Mutex<Vec<Seen>>>, count: usize) {
    for _ in 0..2000 {
        events.settled().await;
        let done = seen.lock().unwrap().iter().filter(|s| matches!(s, Seen::Completed(_))).count();
        if done >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!("no UnitCompleted arrived");
}

#[tokio::test]
async fn a_handled_request_publishes_request_handled_in_its_unit_then_unit_completed() {
    let events = Arc::new(Dispatcher::new());
    let seen = watch(&events);

    let request = Request::builder()
        .uri("/posts/7")
        .header("traceparent", TRACEPARENT)
        .header("user-agent", "curl/8")
        .build();
    let response = kernel(&events).handle_request(request).await;
    assert_eq!(response.status().as_u16(), 200);
    assert!(Context::current_unit().is_none(), "the unit is current only inside the request");

    completed(&events, &seen, 1).await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");

    let Seen::Handled(handled, unit) = &seen[0] else { panic!("RequestHandled first: {seen:?}") };
    let Seen::Completed(done) = &seen[1] else { panic!("UnitCompleted last: {seen:?}") };
    assert_eq!(*unit, Some(done.id), "published in the request's own unit");
    assert_eq!(done.kind, UnitKind::Request);
    assert_eq!(done.path.as_deref(), Some("/posts/7"));
    assert_eq!(done.trace_id.as_deref(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));

    assert_eq!(handled.kind(), "http.server.request");
    let fields = handled.collect_fields();
    assert_eq!(fields.text("otel.name"), Some("GET /posts/{post}"));
    assert_eq!(fields.text("http.request.method"), Some("GET"));
    assert_eq!(fields.text("url.path"), Some("/posts/7"));
    assert_eq!(fields.text("http.route"), Some("/posts/{post}"));
    assert_eq!(fields.text("rainier.route.name"), Some("posts.show"));
    assert_eq!(fields.text("user_agent.original"), Some("curl/8"));
    assert_eq!(fields.text("trace_id"), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
    assert_eq!(fields.get("http.response.status_code"), Some(&rainier_events::Value::U64(200)));
    assert!(!handled.failed());
}

#[tokio::test]
async fn a_server_error_is_published_failed_with_its_route() {
    let events = Arc::new(Dispatcher::new());
    let seen = watch(&events);

    let response = kernel(&events).handle_request(Request::builder().uri("/boom").build()).await;
    assert_eq!(response.status().as_u16(), 500);

    completed(&events, &seen, 1).await;
    let seen = seen.lock().unwrap();
    let handled = seen
        .iter()
        .find_map(|s| match s {
            Seen::Handled(handled, _) => Some(handled),
            _ => None,
        })
        .expect("RequestHandled");
    assert!(handled.failed());
    let fields = handled.collect_fields();
    assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
    assert_eq!(fields.text("error.type"), Some("500"));
    assert_eq!(fields.text("http.route"), Some("/boom"), "the route survives the re-render");
}

#[tokio::test]
async fn with_nobody_subscribed_nothing_is_published() {
    let events = Arc::new(Dispatcher::fake());
    let response = kernel(&events).handle_request(Request::builder().uri("/posts/1").build()).await;
    assert_eq!(response.status().as_u16(), 200);
    // A fake records what is published; a real dispatcher with nobody
    // subscribed builds nothing. Both are the same `publish_activity_with`.
    events.assert_dispatched_times::<RequestHandled>(1);

    let quiet = Arc::new(Dispatcher::new());
    assert!(!quiet.has_activity_subscribers::<RequestHandled>());
    kernel(&quiet).handle_request(Request::builder().uri("/posts/1").build()).await;
}
