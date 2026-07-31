//! The [`Mailable`] contract — an email as an object.

use rainier_support::Result;
use rainier_view::ViewEngine;

use crate::message::{Attachment, Content, Envelope, Message};

/// An email, described rather than assembled.
///
/// A mailable says *what* the email is — its envelope, its content, its
/// attachments — and the [`Mailer`](crate::Mailer) works out how to render and
/// deliver it. That separation is why a mailable is testable on its own: it
/// produces a value, and needs no SMTP server to do it.
///
/// ```
/// use rainier_mail::{Content, Envelope, Mailable};
/// use rainier_support::Result;
///
/// struct WelcomeEmail {
///     name: String,
///     email: String,
/// }
///
/// impl Mailable for WelcomeEmail {
///     fn envelope(&self) -> Envelope {
///         Envelope::new("Welcome to Rainier").to(self.email.clone())
///     }
///
///     fn content(&self) -> Result<Content> {
///         Content::view("mail.welcome", serde_json::json!({ "name": self.name }))
///     }
/// }
/// ```
pub trait Mailable: Send + Sync {
    /// Who the message is from, to, and about.
    fn envelope(&self) -> Envelope;

    /// The body.
    fn content(&self) -> Result<Content>;

    /// Files to send with it.
    fn attachments(&self) -> Result<Vec<Attachment>> {
        Ok(Vec::new())
    }

    /// Extra headers.
    fn headers(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Assemble the message, rendering any views through `views`.
    ///
    /// Provided rather than required: overriding it means taking on the
    /// rendering too, which almost nothing needs.
    fn build(&self, views: &dyn ViewEngine) -> Result<Message> {
        let mut message = Message::new(self.envelope());

        match self.content()? {
            Content::View { html, text, data } => {
                message.html = Some(views.render(&html, &data)?);
                message.text = match text {
                    Some(name) => Some(views.render(&name, &data)?),
                    // No text view: the mailer derives one from the HTML, so
                    // the message is still readable in a text-only client.
                    None => None,
                };
            }
            Content::Literal { html, text } => {
                message.html = html;
                message.text = text;
            }
        }

        message.attachments = self.attachments()?;
        for (name, value) in self.headers() {
            message = message.with_header(name, value);
        }
        Ok(message)
    }
}

/// An already-rendered message is a mailable that needs no rendering.
///
/// So anything taking a `&dyn Mailable` — the mailer, a notification's
/// [`to_mail`](../../rainier_notify/trait.Notification.html#method.to_mail) —
/// accepts a hand-assembled `Message` too. The template case and the
/// three-lines-of-text case go down one path.
impl Mailable for Message {
    fn envelope(&self) -> Envelope {
        self.envelope.clone()
    }

    fn content(&self) -> Result<Content> {
        Ok(Content::Literal { html: self.html.clone(), text: self.text.clone() })
    }

    fn attachments(&self) -> Result<Vec<Attachment>> {
        Ok(self.attachments.clone())
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.headers.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_view::MemoryEngine;

    struct Welcome {
        name: String,
    }

    impl Mailable for Welcome {
        fn envelope(&self) -> Envelope {
            Envelope::new("Welcome").from("app@example.com").to("ada@example.com")
        }

        fn content(&self) -> Result<Content> {
            Content::view("mail.welcome", serde_json::json!({ "name": self.name }))
        }

        fn attachments(&self) -> Result<Vec<Attachment>> {
            Ok(vec![Attachment::from_bytes("terms.txt", "text/plain", b"terms".to_vec())])
        }

        fn headers(&self) -> Vec<(String, String)> {
            vec![("X-Campaign".into(), "onboarding".into())]
        }
    }

    struct Plain;

    impl Mailable for Plain {
        fn envelope(&self) -> Envelope {
            Envelope::new("Plain").to("ada@example.com")
        }
        fn content(&self) -> Result<Content> {
            Ok(Content::text("just text"))
        }
    }

    fn views() -> MemoryEngine {
        MemoryEngine::new()
            .with("mail.welcome", "<p>Hi {{ name }}</p>")
            .with("mail.welcome_text", "Hi {{ name }}")
    }

    #[test]
    fn building_renders_the_view_and_collects_everything() {
        let message = Welcome { name: "Ada".into() }.build(&views()).unwrap();

        assert_eq!(message.envelope.subject, "Welcome");
        assert_eq!(message.html.as_deref(), Some("<p>Hi Ada</p>"));
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.headers, vec![("X-Campaign".to_string(), "onboarding".to_string())]);
        assert!(message.validate().is_ok());
    }

    #[test]
    fn a_text_view_is_rendered_when_one_is_declared() {
        struct WithText;
        impl Mailable for WithText {
            fn envelope(&self) -> Envelope {
                Envelope::new("Hi").from("a@b.co").to("c@d.co")
            }
            fn content(&self) -> Result<Content> {
                Ok(Content::view("mail.welcome", serde_json::json!({ "name": "Ada" }))
                    .unwrap()
                    .with_text_view("mail.welcome_text"))
            }
        }

        let message = WithText.build(&views()).unwrap();
        assert_eq!(message.text.as_deref(), Some("Hi Ada"));
        assert_eq!(message.html.as_deref(), Some("<p>Hi Ada</p>"));
    }

    #[test]
    fn without_a_text_view_the_text_body_is_derived_from_the_html() {
        let message = Welcome { name: "Ada".into() }.build(&views()).unwrap();
        assert!(message.text.is_none());
        assert_eq!(message.text_body(), "Hi Ada");
    }

    #[test]
    fn literal_content_needs_no_view_engine() {
        let message = Plain.build(&MemoryEngine::new()).unwrap();
        assert_eq!(message.text.as_deref(), Some("just text"));
        assert!(message.html.is_none());
    }

    #[test]
    fn a_missing_view_surfaces_as_an_error() {
        struct Broken;
        impl Mailable for Broken {
            fn envelope(&self) -> Envelope {
                Envelope::new("Hi").to("a@b.co")
            }
            fn content(&self) -> Result<Content> {
                Content::view("mail.nonexistent", serde_json::json!({}))
            }
        }

        let err = Broken.build(&views()).err().expect("should fail");
        assert!(err.message().contains("mail.nonexistent"), "{}", err.message());
    }

    #[test]
    fn view_data_is_escaped_like_any_other_template() {
        let message = Welcome { name: "<script>".into() }.build(&views()).unwrap();
        assert_eq!(message.html.as_deref(), Some("<p>Hi &lt;script&gt;</p>"));
    }
}
