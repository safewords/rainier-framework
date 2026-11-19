//! What the kernel publishes on the event bus: [`RequestHandled`], the root
//! activity of every request's unit of work.
//!
//! Every request [`Kernel::handle_request`](crate::Kernel::handle_request)
//! serves runs inside a [`Unit`] of kind [`UnitKind::Request`], so anything
//! published while the middleware and the handler run — a query, a cache
//! call, a mail — names the request it belongs to through
//! [`Context::current`](rainier_events::Context::current). When the response
//! is ready, `RequestHandled` is published inside the same unit; once every
//! event raised in it has been delivered,
//! [`UnitCompleted`](rainier_events::UnitCompleted) follows.

use std::sync::Arc;
use std::time::Duration;

use rainier_events::{Activity, Context, Dispatcher, Fields, Unit, UnitKind};

/// A request was handled: published when its response is ready, inside the
/// request's unit.
///
/// Its [`kind`](Activity::kind) is `http.server.request` and its fields are
/// the ones the kernel's span of that name records, under the same names.
/// Published whether or not the `telemetry` feature is on.
#[derive(Debug, Clone)]
pub struct RequestHandled {
    /// The request method, `GET`.
    pub method: String,
    /// The path, never the query string.
    pub path: String,
    /// The matched route's pattern, `/posts/{post}`, when a route matched.
    pub route: Option<String>,
    /// The matched route's name, when it has one.
    pub route_name: Option<String>,
    /// The response's status code.
    pub status: u16,
    /// The `User-Agent` header, when the request carried one.
    pub user_agent: Option<String>,
    /// The request's trace id, when it is known.
    pub trace_id: Option<String>,
    /// From the kernel taking the request to the response being ready.
    pub duration: Duration,
}

impl Activity for RequestHandled {
    fn kind(&self) -> &'static str {
        "http.server.request"
    }

    fn fields(&self, fields: &mut Fields) {
        // The span's display name: the method, and the route pattern once the
        // router matched one — never the path.
        let name = match &self.route {
            Some(route) => format!("{} {route}", self.method),
            None => self.method.clone(),
        };
        fields
            .set("otel.name", name)
            .set("otel.kind", "server")
            .set("http.request.method", self.method.clone())
            .set("url.path", self.path.clone())
            .set("url.scheme", "http")
            .set("user_agent.original", self.user_agent.clone().unwrap_or_default())
            .maybe("http.route", self.route.clone())
            .maybe("rainier.route.name", self.route_name.clone())
            .set("http.response.status_code", self.status)
            .maybe("trace_id", self.trace_id.clone());
        // A 4xx is the client's mistake and leaves the span unmarked.
        if self.status >= 500 {
            fields.set("otel.status_code", "ERROR").set("error.type", self.status.to_string());
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.status >= 500
    }
}

/// Open the unit `kind` work runs in — on `events` when the caller has the
/// application's dispatcher in hand, else on whatever
/// [`Unit::open_or_join`] finds — or `None` when the work joins the unit
/// already current.
pub(crate) fn open_unit(
    events: Option<&Arc<Dispatcher>>,
    kind: UnitKind,
    name: Option<String>,
    path: Option<String>,
) -> Option<Arc<Unit>> {
    let Some(events) = events else { return Unit::open_or_join(kind, name, path) };
    // The same rule as `open_or_join`: a request is its own unit unless one
    // is already current.
    match Context::current_unit() {
        None => Some(Unit::on(events, kind, name, path)),
        Some(_) => None,
    }
}

/// Whether `header` is a well-formed W3C `traceparent` — what a unit may
/// record as the trace it continues.
pub(crate) fn is_traceparent(header: &str) -> bool {
    let mut parts = header.split('-');
    let (Some(version), Some(trace), Some(parent), Some(flags), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
    hex(version, 2)
        && hex(trace, 32)
        && hex(parent, 16)
        && hex(flags, 2)
        && trace.bytes().any(|b| b != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handled(status: u16, route: Option<&str>) -> RequestHandled {
        RequestHandled {
            method: "GET".into(),
            path: "/posts/7".into(),
            route: route.map(str::to_string),
            route_name: route.map(|_| "posts.show".to_string()),
            status,
            user_agent: Some("curl/8".into()),
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            duration: Duration::from_millis(3),
        }
    }

    #[test]
    fn its_fields_are_the_server_spans() {
        let fields = handled(200, Some("/posts/{post}")).collect_fields();
        assert_eq!(fields.text("otel.name"), Some("GET /posts/{post}"));
        assert_eq!(fields.text("otel.kind"), Some("server"));
        assert_eq!(fields.text("http.request.method"), Some("GET"));
        assert_eq!(fields.text("url.path"), Some("/posts/7"));
        assert_eq!(fields.text("url.scheme"), Some("http"));
        assert_eq!(fields.text("user_agent.original"), Some("curl/8"));
        assert_eq!(fields.text("http.route"), Some("/posts/{post}"));
        assert_eq!(fields.text("rainier.route.name"), Some("posts.show"));
        assert_eq!(fields.get("http.response.status_code"), Some(&rainier_events::Value::U64(200)));
        assert_eq!(fields.text("trace_id"), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(fields.get("otel.status_code"), None);
    }

    #[test]
    fn a_5xx_is_marked_failed_and_a_4xx_is_not() {
        let failed = handled(503, None);
        let fields = failed.collect_fields();
        assert!(failed.failed());
        assert_eq!(fields.text("otel.name"), Some("GET"), "no route, just the method");
        assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
        assert_eq!(fields.text("error.type"), Some("503"));

        let missing = handled(404, None);
        assert!(!missing.failed());
        assert_eq!(missing.collect_fields().get("otel.status_code"), None);
    }

    #[test]
    fn only_a_well_formed_traceparent_is_recorded() {
        assert!(is_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"));
        assert!(!is_traceparent("garbage"));
        assert!(!is_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"));
        assert!(!is_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7"));
    }
}
