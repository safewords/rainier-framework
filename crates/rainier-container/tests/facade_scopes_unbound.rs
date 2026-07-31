//! What the scope helpers do when **nothing** is installed.
//!
//! Its own binary, and that is the whole point: this clears the process-wide
//! application, so a test in `facade_scopes.rs` running beside it would find
//! the global it was relying on gone — which is exactly the failure mode those
//! tests exist to describe, arriving from the wrong direction.

use rainier_container::{clear_facade_application, spawn_with_facades, try_facade_application};

#[tokio::test(flavor = "multi_thread")]
async fn spawning_with_nothing_installed_is_just_a_spawn() {
    clear_facade_application();

    let seen = spawn_with_facades(async { try_facade_application() }).await.unwrap();

    assert!(seen.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn resolving_with_nothing_installed_is_none_rather_than_a_panic() {
    clear_facade_application();

    assert!(try_facade_application().is_none());
}
