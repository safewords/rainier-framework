//! Booting installs the application's dispatcher as the process-wide bus,
//! so a unit of work opened with no dispatcher in hand publishes on it.
//!
//! Its own binary: it installs process-wide state.

use std::sync::Arc;

use rainier_events::{Dispatcher, Unit, UnitKind};
use rainier_framework::Rainier;

#[tokio::test]
async fn boot_installs_the_applications_dispatcher_as_the_bus() {
    let app = Rainier::new(".").without_tracing().boot().await.expect("boots");
    let events = app.resolve::<Dispatcher>().unwrap();

    let installed = rainier_events::installed().expect("a bus is installed");
    assert!(Arc::ptr_eq(&installed, &events), "the application's own dispatcher");

    // A unit opened without a dispatcher — a job the `sync` driver runs
    // outside a request — publishes on it.
    let unit = Unit::open_or_join(UnitKind::Job, Some("test".into()), None).expect("a unit");
    assert!(unit.events().is_some_and(|bus| Arc::ptr_eq(&bus, &events)));
}
