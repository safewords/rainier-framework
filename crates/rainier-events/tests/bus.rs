//! `publish`: fire and forget, filtered subscriptions, context, units.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rainier_events::{Context, Dispatcher, Unit, UnitCompleted, UnitKind};
use rainier_support::Error;

#[derive(Debug)]
struct UserChanged {
    id: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_returns_before_a_slow_subscriber_runs() {
    let events = Arc::new(Dispatcher::new());
    let done = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&done);
    events.on::<UserChanged>().run(move |_e, _cx| {
        let seen = Arc::clone(&seen);
        async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });

    let started = Instant::now();
    events.publish(UserChanged { id: 1 });
    assert!(started.elapsed() < Duration::from_millis(20), "publish waited on a subscriber");
    assert_eq!(done.load(Ordering::SeqCst), 0);

    events.settled().await;
    assert_eq!(done.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_subscriber_does_not_hold_up_another() {
    let events = Arc::new(Dispatcher::new());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let tx = Mutex::new(Some(tx));
    events.on::<UserChanged>().run(|_e, _cx| async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(())
    });
    events.on::<UserChanged>().run_sync(move |_e, _cx| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        Ok(())
    });
    events.publish(UserChanged { id: 1 });
    tokio::time::timeout(Duration::from_secs(2), rx).await.expect("the fast one ran").unwrap();
}

#[tokio::test]
async fn a_declarative_filter_picks_the_events_it_wants() {
    let events = Arc::new(Dispatcher::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    events.on::<UserChanged>().when(|e| e.id == 42).run_sync(move |e, _cx| {
        sink.lock().unwrap().push(e.id);
        Ok(())
    });
    for id in [1, 42, 7, 42] {
        events.publish(UserChanged { id });
    }
    events.settled().await;
    assert_eq!(*seen.lock().unwrap(), vec![42, 42]);
}

#[tokio::test]
async fn the_publishers_unit_travels_with_the_event() {
    let events = Arc::new(Dispatcher::new());
    let seen = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);
    events.on::<UserChanged>().run_sync(move |_e, cx| {
        *sink.lock().unwrap() = cx.unit().map(|u| (u.id(), u.trace_id().map(str::to_string)));
        Ok(())
    });

    let unit = Unit::on(&events, UnitKind::Request, Some("GET /users/{id}".into()), None);
    unit.set_trace(Some("4bf92f3577b34da6a3ce929d0e0e4736"), None);
    let id = unit.id();
    let publisher = Arc::clone(&events);
    unit.scope(async move { publisher.publish(UserChanged { id: 1 }) }).await;
    drop(unit);
    events.settled().await;
    assert_eq!(
        *seen.lock().unwrap(),
        Some((id, Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string())))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_unit_completes_only_after_its_events_are_delivered() {
    let events = Arc::new(Dispatcher::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    events.on::<UserChanged>().run(move |e, _cx| {
        let sink = Arc::clone(&sink);
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sink.lock().unwrap().push(format!("user {}", e.id));
            Ok(())
        }
    });
    let sink = Arc::clone(&log);
    events.on::<UnitCompleted>().run_sync(move |e, _cx| {
        sink.lock().unwrap().push(format!("completed {:?}", e.kind));
        Ok(())
    });

    let unit = Unit::on(&events, UnitKind::Job, Some("SendInvoice".into()), None);
    let publisher = Arc::clone(&events);
    unit.scope(async move {
        publisher.publish(UserChanged { id: 1 });
        publisher.publish(UserChanged { id: 2 });
    })
    .await;
    drop(unit);
    // The completion is itself published when the last event lets go of the
    // unit, so settle twice: once for the events, once for the completion.
    events.settled().await;
    events.settled().await;
    let log = log.lock().unwrap().clone();
    assert_eq!(log.len(), 3, "{log:?}");
    assert_eq!(log[2], "completed Job");
}

#[tokio::test]
async fn a_dispatch_reaches_subscriptions_without_running_them_inline() {
    let events = Arc::new(Dispatcher::new());
    let inline = Arc::new(AtomicUsize::new(0));
    let deferred = Arc::new(AtomicUsize::new(0));
    let i = Arc::clone(&inline);
    events.listen_sync(move |_: Arc<UserChanged>| {
        i.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let d = Arc::clone(&deferred);
    events.on::<UserChanged>().run_sync(move |_, _| {
        d.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    events.dispatch(UserChanged { id: 1 }).await.unwrap();
    assert_eq!(inline.load(Ordering::SeqCst), 1, "a listener runs inside dispatch");
    events.settled().await;
    assert_eq!(deferred.load(Ordering::SeqCst), 1);
    assert_eq!(inline.load(Ordering::SeqCst), 1, "and is not run a second time");
}

#[tokio::test]
async fn a_published_event_reaches_ordinary_listeners_later() {
    let events = Arc::new(Dispatcher::new());
    let count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&count);
    events.listen(move |_: Arc<UserChanged>| {
        let c = Arc::clone(&c);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            Err(Error::internal("cannot reach the publisher"))
        }
    });
    events.publish(UserChanged { id: 1 });
    events.settled().await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(events.subscriber_failures(), 1);
}

#[tokio::test]
async fn a_fake_records_published_events() {
    let events = Dispatcher::fake();
    events.publish(UserChanged { id: 3 });
    events.assert_dispatched_times::<UserChanged>(1);
    assert_eq!(events.dispatched::<UserChanged>()[0].id, 3);
}

#[test]
fn nothing_subscribed_builds_nothing() {
    let events = Dispatcher::new();
    events.publish_with(|| -> UserChanged { panic!("built for nobody") });
    assert!(!events.has_subscribers::<UserChanged>());
}

#[test]
fn publishing_from_a_plain_thread_delivers_on_the_bus_runtime() {
    let events = Arc::new(Dispatcher::new());
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    events.on::<UserChanged>().run_sync(move |e, _| {
        tx.lock().unwrap().send(e.id).unwrap();
        Ok(())
    });
    events.publish(UserChanged { id: 9 });
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), 9);
}

#[tokio::test]
async fn the_bus_resolves_to_the_current_units_dispatcher() {
    let events = Arc::new(Dispatcher::new());
    let unit = Unit::on(&events, UnitKind::Command, None, None);
    let found = unit.scope(async { rainier_events::bus() }).await;
    assert!(found.is_some_and(|b| Arc::ptr_eq(&b, &events)));
    let _ = Context::none();
}
