//! What the view layer publishes about its own work on the event bus.
//!
//! [`ViewRendered`] is the bus's view of a `view.render` span: published for
//! every template the [`TemplateEngine`](crate::TemplateEngine) renders,
//! whether or not the crate's `telemetry` feature is on, with the same fields
//! the span carries.

use std::time::Duration;

use rainier_events::{Activity, Fields};

/// A template was rendered — or failed to.
///
/// Carries the template's name, never the data it was given, which is
/// whatever the page is about. Published in the context of the code that
/// rendered it, so a view rendered for a mail names the mail's
/// [`Operation`](rainier_events::Operation) as its parent.
#[derive(Debug, Clone)]
pub struct ViewRendered {
    /// The view's name, as it was asked for: `mail.welcome`, `posts.index`.
    pub name: String,
    /// How long the render took, includes and layouts with it.
    pub duration: Duration,
    /// The failure's kind, in `snake_case`, when the render failed.
    pub error_type: Option<&'static str>,
}

impl Activity for ViewRendered {
    fn kind(&self) -> &'static str {
        "view.render"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("render {}", self.name))
            .set("rainier.view.name", self.name.clone());
        if let Some(error_type) = self.error_type {
            fields.set("otel.status_code", "ERROR").set("error.type", error_type);
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error_type.is_some()
    }
}
