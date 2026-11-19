//! A deferred task runs inside the span that was current where it was
//! deferred — the request's, so its log lines carry the request's trace id.
//!
//! Its own test binary: tracing caches whether a callsite is enabled, and a
//! test elsewhere in the same process that hits the `deferred` span with no
//! subscriber installed would disable it for this one.

use std::sync::{Arc, Mutex};

use rainier_framework::deferred::Deferrer;
use tracing_subscriber::layer::SubscriberExt;

/// Each `deferred` span's parent, by name.
#[derive(Clone, Default)]
struct Parents(Arc<Mutex<Vec<Option<String>>>>);

impl<S> tracing_subscriber::Layer<S> for Parents
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if attrs.metadata().name() == "deferred" {
            let parent = ctx.span(id).and_then(|s| s.parent()).map(|p| p.name().to_string());
            self.0.lock().unwrap().push(parent);
        }
    }
}

#[tokio::test]
async fn the_task_runs_inside_the_callers_span() {
    let parents = Parents::default();
    let subscriber = tracing_subscriber::registry().with(parents.clone());
    let deferrer = Deferrer::new();

    tracing::subscriber::with_default(subscriber, || {
        let request = tracing::info_span!("http.server.request", trace_id = "4bf92f35");
        let _entered = request.enter();
        deferrer.spawn("traced", async {});
    });
    deferrer.settle().await;

    assert_eq!(parents.0.lock().unwrap().as_slice(), &[Some("http.server.request".to_string())]);
}
