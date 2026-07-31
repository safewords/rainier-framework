//! Where a facade scope reaches — asserted, because the answer is not obvious
//! and the failure is silent.
//!
//! # Why nothing here asserts on *which* global it found
//!
//! The process binding is exactly that — one slot per process — so two of
//! these tests running at once each install their own and then race to read it
//! back. Every assertion below is instead about an application this test
//! created and nobody else can see: it either reached the spawned task or it
//! did not, which is the property under test anyway.
//!
//! Asserting "found == my global" passed locally and failed about one run in
//! three, which is the least useful kind of test there is.

use std::sync::Arc;

use rainier_container::{
    scope_facade_application, set_facade_application, spawn_with_facades, try_facade_application,
    with_facade_application, Application,
};

/// Something for the process binding to hold, so "nothing is installed" is
/// never what a test is accidentally measuring.
fn install_a_global() {
    set_facade_application(Arc::new(Application::new("a global")));
}

/// Whether what was found is the application *this test* made.
fn is_mine(found: &Option<Arc<Application>>, mine: &Arc<Application>) -> bool {
    found.as_ref().is_some_and(|found| Arc::ptr_eq(found, mine))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thread_scope_survives_an_await_on_a_multi_threaded_runtime() {
    // `block_on` drives the body on the calling thread, so this holds however
    // many workers the runtime has. Worth asserting: "multi-threaded runtime"
    // is widely read as "the body moves", and it does not.
    install_a_global();
    let mine = Arc::new(Application::new("mine"));
    let _scope = scope_facade_application(Arc::clone(&mine));

    for _ in 0..50 {
        tokio::task::yield_now().await;
        assert!(is_mine(&try_facade_application(), &mine));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thread_scope_does_not_reach_a_spawned_task() {
    // The hole. A spawned task is a new task on some other thread; it inherits
    // no scope and falls back to the process binding, with nothing to notice.
    install_a_global();
    let mine = Arc::new(Application::new("mine"));
    let _scope = scope_facade_application(Arc::clone(&mine));

    let seen = tokio::spawn(async { try_facade_application() }).await.unwrap();

    assert!(!is_mine(&seen, &mine), "this is the documented gap, not a bug in the test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_with_facades_closes_it() {
    install_a_global();
    let mine = Arc::new(Application::new("mine"));
    let _scope = scope_facade_application(Arc::clone(&mine));

    let seen = spawn_with_facades(async {
        // Yield first, so the task is rescheduled and stands a real chance of
        // resuming on a different worker than it started on.
        tokio::task::yield_now().await;
        try_facade_application()
    })
    .await
    .unwrap();

    assert!(is_mine(&seen, &mine));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_scope_follows_the_future_across_threads() {
    install_a_global();
    let mine = Arc::new(Application::new("mine"));

    let seen = tokio::spawn(with_facade_application(Arc::clone(&mine), async {
        let mut all = Vec::new();
        for _ in 0..50 {
            tokio::task::yield_now().await;
            all.push(try_facade_application());
        }
        all
    }))
    .await
    .unwrap();

    assert!(seen.iter().all(|found| is_mine(found, &mine)), "one poll resolved elsewhere");
}

#[tokio::test(flavor = "multi_thread")]
async fn task_scopes_nest_and_the_outer_one_resumes() {
    let outer = Arc::new(Application::new("outer"));
    let inner = Arc::new(Application::new("inner"));

    with_facade_application(Arc::clone(&outer), async {
        assert!(is_mine(&try_facade_application(), &outer));

        with_facade_application(Arc::clone(&inner), async {
            assert!(is_mine(&try_facade_application(), &inner));
        })
        .await;

        assert!(is_mine(&try_facade_application(), &outer), "the outer scope did not resume");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_scope_beats_a_thread_scope() {
    // Nearest wins: the task scope is the more specific statement, and it is
    // the one a spawn carried deliberately.
    let thread = Arc::new(Application::new("thread"));
    let task = Arc::new(Application::new("task"));

    let _scope = scope_facade_application(Arc::clone(&thread));

    with_facade_application(Arc::clone(&task), async {
        assert!(is_mine(&try_facade_application(), &task));
    })
    .await;

    assert!(is_mine(&try_facade_application(), &thread), "the thread scope did not resume");
}
