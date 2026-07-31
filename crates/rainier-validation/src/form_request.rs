//! Request contracts — [`FormRequest`] and the [`Validated`] extractor.
//!
//! A form request bundles three things a controller would otherwise do
//! by hand: **authorise** the caller, **validate** the input, and hand the
//! action a payload it can trust — an actual typed struct rather than a
//! loose map:
//!
//! ```
//! use rainier_validation::{FormRequest, Rule, RuleSet};
//! use rainier_http::Request;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct StorePost {
//!     title: String,
//!     body: String,
//!     published: Option<bool>,
//! }
//!
//! #[async_trait::async_trait]
//! impl FormRequest for StorePost {
//!     fn rules() -> RuleSet {
//!         vec![
//!             ("title", vec![Rule::Required, Rule::String, Rule::Between(3.0, 120.0)]),
//!             ("body", vec![Rule::Required, Rule::String]),
//!             ("published", vec![Rule::Boolean]),
//!         ]
//!     }
//!
//!     async fn authorize(request: &Request) -> bool {
//!         request.bearer_token().is_some()
//!     }
//! }
//! ```
//!
//! A handler then takes `Validated<StorePost>` and receives a struct that is
//! authorised, validated, and free of any field the contract did not declare.
//!
//! ## Why the payload is filtered
//!
//! [`Validated`] deserialises from the *validated subset* of the input, not
//! from everything the client sent. A field with no rule never reaches the
//! struct — mass-assignment protection, obtained for free from the rules you
//! already wrote.

use std::sync::Arc;

use rainier_http::coerce;
use rainier_http::{FromRequest, Request};
use rainier_support::{BoxedFuture, Error, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::rule::Rule;
use crate::validator::{RuleSet, ValidationErrors, Validator};

/// A typed, authorised, validated request payload.
#[async_trait::async_trait]
pub trait FormRequest: DeserializeOwned + Send + Sync + 'static {
    /// The validation rules, by field.
    fn rules() -> RuleSet;

    /// Whether the caller may make this request at all.
    ///
    /// Runs **before** validation, so an unauthorised caller cannot use
    /// validation messages to probe what the endpoint expects. Returning
    /// `false` produces a `403`.
    ///
    /// `async` because a real authorisation check reads a policy, a role, or
    /// the record being modified.
    async fn authorize(request: &Request) -> bool {
        let _ = request;
        true
    }

    /// Messages overriding the defaults, keyed `"field.rule"`.
    fn messages() -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// What to validate. Defaults to the merged query string and body.
    ///
    /// Override to fold in route parameters — an update contract that needs
    /// `{post}` to check uniqueness-except-self, for instance.
    fn validation_data(request: &Request) -> Value {
        request.all()
    }

    /// Build the payload from the validated input.
    ///
    /// The default deserialises with string coercion, so a urlencoded form
    /// fills a `u32` field. Override for a payload that is not a plain
    /// deserialisation of the input.
    fn from_input(input: Value) -> Result<Self> {
        coerce::from_value(&input).map_err(|e| {
            // The rules passed but the struct did not fit, which means the two
            // disagree — a programming error, not a client one.
            Error::internal(format!(
                "`{}` passed validation but could not be built from the input: {e}",
                std::any::type_name::<Self>()
            ))
        })
    }

    /// Run authorisation and validation, and build the payload.
    ///
    /// The whole contract in one call, so it can be used outside an extractor
    /// — from a console command, or a test.
    async fn validate_request(request: &Request) -> Result<Self> {
        if !Self::authorize(request).await {
            return Err(Error::unauthorized("This action is unauthorized."));
        }

        let input = Self::validation_data(request);
        let validator = Self::validator();

        let validated = validator.validated_only(&input).map_err(Error::from)?;
        Self::from_input(validated)
    }

    /// The configured [`Validator`] for this contract.
    fn validator() -> Validator {
        Validator::from_rules(Self::rules()).custom_messages(Self::messages())
    }

    /// Validate without building the payload, returning the failures.
    fn check(request: &Request) -> std::result::Result<(), ValidationErrors> {
        Self::validator().validate(&Self::validation_data(request))
    }
}

/// The extractor that runs a [`FormRequest`].
///
/// ```ignore
/// async fn store(Validated(post): Validated<StorePost>) -> Response {
///     Response::text(post.title)     // authorised, validated, filtered
/// }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct Validated<T>(pub T);

impl<T> Validated<T> {
    /// Unwrap the payload.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for Validated<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: FormRequest> FromRequest for Validated<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { T::validate_request(&request).await.map(Validated) })
    }
}

/// Sugar for building a [`RuleSet`] entry.
///
/// ```
/// # use rainier_validation::{field, Rule, RuleSet};
/// let rules: RuleSet = vec![
///     field("title", [Rule::Required, Rule::String]),
///     field("body", [Rule::Required]),
/// ];
/// assert_eq!(rules.len(), 2);
/// ```
pub fn field(
    name: &'static str,
    rules: impl IntoIterator<Item = Rule>,
) -> (&'static str, Vec<Rule>) {
    (name, rules.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::Method;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    struct StorePost {
        title: String,
        body: String,
        #[serde(default)]
        published: Option<bool>,
    }

    #[async_trait::async_trait]
    impl FormRequest for StorePost {
        fn rules() -> RuleSet {
            vec![
                field("title", [Rule::Required, Rule::String, Rule::Between(3.0, 120.0)]),
                field("body", [Rule::Required, Rule::String]),
                field("published", [Rule::Boolean]),
            ]
        }
    }

    fn post(body: Value) -> Request {
        Request::builder().method(Method::POST).json(&body).build()
    }

    #[tokio::test]
    async fn a_valid_request_becomes_a_typed_payload() {
        let request = post(json!({ "title": "Hello", "body": "World", "published": true }));
        let payload = StorePost::validate_request(&request).await.unwrap();

        assert_eq!(
            payload,
            StorePost { title: "Hello".into(), body: "World".into(), published: Some(true) }
        );
    }

    #[tokio::test]
    async fn an_invalid_request_is_a_422_with_the_failures() {
        let request = post(json!({ "title": "Hi" }));
        let err = StorePost::validate_request(&request).await.unwrap_err();

        assert_eq!(err.status(), 422);
        let details = err.details().unwrap();
        assert!(details["title"][0].as_str().unwrap().contains("between"));
        assert!(details["body"][0].as_str().unwrap().contains("required"));
    }

    #[tokio::test]
    async fn undeclared_fields_never_reach_the_payload() {
        #[derive(Debug, Deserialize)]
        struct Partial {
            title: String,
            #[serde(default)]
            is_admin: bool,
        }

        #[async_trait::async_trait]
        impl FormRequest for Partial {
            fn rules() -> RuleSet {
                vec![field("title", [Rule::Required])]
            }
        }

        let request = post(json!({ "title": "Hello", "is_admin": true }));
        let payload = Partial::validate_request(&request).await.unwrap();

        assert_eq!(payload.title, "Hello");
        assert!(!payload.is_admin, "a field with no rule must not be mass-assignable");
    }

    #[tokio::test]
    async fn authorisation_runs_before_validation() {
        #[derive(Debug, Deserialize)]
        struct Guarded {
            #[allow(dead_code)]
            title: String,
        }

        #[async_trait::async_trait]
        impl FormRequest for Guarded {
            fn rules() -> RuleSet {
                vec![field("title", [Rule::Required])]
            }
            async fn authorize(request: &Request) -> bool {
                request.bearer_token() == Some("secret")
            }
        }

        // Invalid *and* unauthorised: the 403 must win, so validation messages
        // cannot be used to probe the endpoint's shape.
        let denied = Guarded::validate_request(&post(json!({}))).await.unwrap_err();
        assert_eq!(denied.status(), 403);

        let allowed = Request::builder()
            .method(Method::POST)
            .header("authorization", "Bearer secret")
            .json(&json!({ "title": "ok" }))
            .build();
        assert!(Guarded::validate_request(&allowed).await.is_ok());
    }

    #[tokio::test]
    async fn custom_messages_are_applied() {
        #[derive(Debug, Deserialize)]
        struct Custom {
            #[allow(dead_code)]
            email: String,
        }

        #[async_trait::async_trait]
        impl FormRequest for Custom {
            fn rules() -> RuleSet {
                vec![field("email", [Rule::Required, Rule::Email])]
            }
            fn messages() -> Vec<(&'static str, &'static str)> {
                vec![("email.required", "We really need your email.")]
            }
        }

        let err = Custom::validate_request(&post(json!({}))).await.unwrap_err();
        assert_eq!(err.details().unwrap()["email"][0], "We really need your email.");
    }

    #[tokio::test]
    async fn validation_data_can_fold_in_route_parameters() {
        #[derive(Debug, Deserialize)]
        struct UpdatePost {
            id: u64,
            #[allow(dead_code)]
            title: String,
        }

        #[async_trait::async_trait]
        impl FormRequest for UpdatePost {
            fn rules() -> RuleSet {
                vec![field("id", [Rule::Required, Rule::Integer]), field("title", [Rule::Required])]
            }
            fn validation_data(request: &Request) -> Value {
                let mut data = request.all();
                if let (Some(object), Some(id)) =
                    (data.as_object_mut(), request.route_param("post"))
                {
                    object.insert("id".into(), Value::String(id.to_string()));
                }
                data
            }
        }

        let request = Request::builder()
            .method(Method::POST)
            .route_param("post", "42")
            .json(&json!({ "title": "Edited" }))
            .build();

        let payload = UpdatePost::validate_request(&request).await.unwrap();
        assert_eq!(payload.id, 42);
    }

    #[tokio::test]
    async fn coercion_lets_a_form_body_fill_typed_fields() {
        #[derive(Debug, Deserialize)]
        struct Paged {
            page: u32,
            active: bool,
        }

        #[async_trait::async_trait]
        impl FormRequest for Paged {
            fn rules() -> RuleSet {
                vec![
                    field("page", [Rule::Required, Rule::Integer]),
                    field("active", [Rule::Required, Rule::Boolean]),
                ]
            }
        }

        let request =
            Request::builder().method(Method::POST).form(&[("page", "3"), ("active", "1")]).build();

        let payload = Paged::validate_request(&request).await.unwrap();
        assert_eq!(payload.page, 3);
        assert!(payload.active);
    }

    #[tokio::test]
    async fn the_extractor_wires_it_into_a_handler_signature() {
        let request = Arc::new(post(json!({ "title": "Hello", "body": "World" })));
        let Validated(payload) = Validated::<StorePost>::from_request(request).await.unwrap();
        assert_eq!(payload.title, "Hello");
    }

    #[tokio::test]
    async fn the_extractor_surfaces_the_422() {
        let request = Arc::new(post(json!({})));
        let err = Validated::<StorePost>::from_request(request).await.unwrap_err();
        assert_eq!(err.status(), 422);
    }

    #[tokio::test]
    async fn check_validates_without_building_the_payload() {
        let errors = StorePost::check(&post(json!({ "title": "Hi" }))).unwrap_err();
        assert!(errors.has("title"));
        assert!(errors.has("body"));

        assert!(StorePost::check(&post(json!({ "title": "Hello", "body": "x" }))).is_ok());
    }

    #[tokio::test]
    async fn a_payload_that_disagrees_with_its_rules_is_an_internal_error() {
        // `title` is required by the rules but the struct wants a `u64`, so
        // the two disagree — that is the framework author's bug, not the
        // client's, and it must not be reported as a 422.
        #[derive(Debug, Deserialize)]
        struct Mismatched {
            #[allow(dead_code)]
            title: u64,
        }

        #[async_trait::async_trait]
        impl FormRequest for Mismatched {
            fn rules() -> RuleSet {
                vec![field("title", [Rule::Required, Rule::String])]
            }
        }

        let err = Mismatched::validate_request(&post(json!({ "title": "not a number" })))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 500);
        assert!(err.message().contains("passed validation"), "{}", err.message());
    }
}
