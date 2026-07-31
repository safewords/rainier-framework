//! [`FromRequest`] — typed extractors for controller actions.
//!
//! A controller signature says what it needs and the framework supplies
//! it, with the types doing the describing:
//!
//! ```ignore
//! async fn store(Json(post): Json<NewPost>, Query(page): Query<Pagination>) -> Response
//! ```
//!
//! Every extractor takes `Arc<Request>` and returns a `'static` future. Sharing
//! one `Arc` is what lets several extractors read the same request
//! concurrently-shaped without cloning it or fighting over a borrow, and the
//! `'static` future is what lets the router store handlers of many different
//! shapes in one table.
//!
//! Extraction is `async` because some extractors genuinely need to be: route
//! **model binding** loads a record from the database, and an authenticated-user
//! extractor asks the guard. Those live in `rainier-database` and
//! `rainier-auth` respectively; the ones here need no I/O but share the shape.

use std::sync::Arc;

use rainier_support::{BoxedFuture, Error, Result};
use serde::de::DeserializeOwned;

use crate::coerce;
use crate::request::Request;
use crate::upload::UploadedFile;

/// Something a handler parameter can be built from.
pub trait FromRequest: Sized + Send + 'static {
    /// Build `Self` from the request, or fail with the error the client should
    /// see (a 400 for a malformed body, a 422 for a failed contract).
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>>;
}

/// The whole request.
impl FromRequest for Arc<Request> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { Ok(request) })
    }
}

/// An extractor that never fails: `Option<T>` is `None` when `T` would have
/// errored. For inputs that are genuinely optional.
impl<T: FromRequest> FromRequest for Option<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { Ok(T::from_request(request).await.ok()) })
    }
}

/// Hands the extraction error to the handler instead of short-circuiting, for
/// actions that want to answer a bad request themselves.
impl<T: FromRequest> FromRequest for Result<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { Ok(T::from_request(request).await) })
    }
}

/// The JSON request body, deserialised.
///
/// ```
/// # use rainier_http::{extract::Json, Request, FromRequest};
/// # use std::sync::Arc;
/// #[derive(serde::Deserialize)]
/// struct NewPost { title: String }
///
/// # #[tokio::main] async fn main() {
/// let request = Arc::new(
///     Request::builder().json(&serde_json::json!({ "title": "Hello" })).build(),
/// );
/// let Json(post) = Json::<NewPost>::from_request(request).await.unwrap();
/// assert_eq!(post.title, "Hello");
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T: DeserializeOwned + Send + 'static> FromRequest for Json<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { request.json::<T>().map(Json) })
    }
}

/// The merged request input (query string plus body), deserialised with string
/// coercion — so a urlencoded `page=2` fills a `u32` field.
#[derive(Debug, Clone, Copy, Default)]
pub struct Form<T>(pub T);

impl<T: DeserializeOwned + Send + 'static> FromRequest for Form<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            coerce::from_value(&request.all())
                .map(Form)
                .map_err(|e| Error::bad_request(format!("invalid form input: {e}")))
        })
    }
}

/// The query string, deserialised with string coercion.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T: DeserializeOwned + Send + 'static> FromRequest for Query<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            coerce::from_value(request.query())
                .map(Query)
                .map_err(|e| Error::bad_request(format!("invalid query string: {e}")))
        })
    }
}

/// The route parameters, deserialised with string coercion.
///
/// A struct reads them by name; a bare scalar reads the **only** parameter, so
/// `Path<u64>` works for `/posts/{post}` without a wrapper struct.
///
/// ```
/// # use rainier_http::{extract::Path, Request, FromRequest};
/// # use std::sync::Arc;
/// # #[tokio::main] async fn main() {
/// let request = Arc::new(Request::builder().route_param("post", "42").build());
/// let Path(id) = Path::<u64>::from_request(request).await.unwrap();
/// assert_eq!(id, 42);
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct Path<T>(pub T);

impl<T: DeserializeOwned + Send + 'static> FromRequest for Path<T> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            let params = request.route_params();
            let as_object = coerce::object_from_strings(params.iter());

            // Try the whole map first — that is what a struct or a map wants.
            if let Ok(value) = coerce::from_value::<T>(&as_object) {
                return Ok(Path(value));
            }

            // Otherwise a bare scalar: only unambiguous with exactly one
            // parameter. With two, refusing beats silently picking one.
            if params.len() == 1 {
                let only = params.values().next().expect("just checked len == 1");
                let scalar = serde_json::Value::String(only.clone());
                if let Ok(value) = coerce::from_value::<T>(&scalar) {
                    return Ok(Path(value));
                }
            }

            Err(Error::bad_request(format!(
                "could not read the route parameters {:?} as the expected type",
                params.keys().collect::<Vec<_>>()
            )))
        })
    }
}

/// A compile-time name for an extractor that needs one — a header, a form
/// field.
///
/// Rust has no stable `const N: &'static str` generic parameter, so the name
/// travels as a marker type instead of as a literal. [`static_name!`](crate::static_name) writes
/// the marker for you.
pub trait StaticName: Send + Sync + 'static {
    /// The name.
    const NAME: &'static str;
}

/// Declare a [`StaticName`] marker type.
///
/// ```
/// # use rainier_http::static_name;
/// static_name!(XRequestId, "x-request-id");
/// ```
#[macro_export]
macro_rules! static_name {
    ($(#[$meta:meta])* $vis:vis $name:ident, $value:expr) => {
        $(#[$meta])*
        #[doc = concat!("A [`StaticName`](", stringify!($crate), "::extract::StaticName) for `", $value, "`.")]
        #[derive(Debug, Clone, Copy, Default)]
        $vis struct $name;

        impl $crate::extract::StaticName for $name {
            const NAME: &'static str = $value;
        }
    };
    ($(#[$meta:meta])* $name:ident, $value:expr) => {
        $crate::static_name!($(#[$meta])* pub $name, $value);
    };
}

/// A named header's value, required.
///
/// ```
/// # use rainier_http::{extract::Header, static_name, FromRequest, Request};
/// # use std::sync::Arc;
/// static_name!(XRequestId, "x-request-id");
///
/// # #[tokio::main] async fn main() {
/// let request = Arc::new(Request::builder().header("x-request-id", "abc").build());
/// let Header(id, ..) = Header::<XRequestId>::from_request(request).await.unwrap();
/// assert_eq!(id, "abc");
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Header<K: StaticName>(pub String, pub std::marker::PhantomData<K>);

impl<K: StaticName> FromRequest for Header<K> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            request
                .header(K::NAME)
                .map(|value| Header(value.to_string(), std::marker::PhantomData))
                .ok_or_else(|| Error::bad_request(format!("the `{}` header is required", K::NAME)))
        })
    }
}

/// The `Authorization: Bearer …` token.
#[derive(Debug, Clone)]
pub struct Bearer(pub String);

impl FromRequest for Bearer {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            request
                .bearer_token()
                .map(|token| Bearer(token.to_string()))
                .ok_or_else(|| Error::unauthenticated("a bearer token is required"))
        })
    }
}

/// The raw request body.
#[derive(Debug, Clone)]
pub struct RawBody(pub bytes::Bytes);

impl FromRequest for RawBody {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { Ok(RawBody(request.body().clone())) })
    }
}

/// Every file uploaded under a named field. Empty when none arrived.
#[derive(Debug, Clone)]
pub struct Files<K: StaticName>(pub Vec<UploadedFile>, pub std::marker::PhantomData<K>);

impl<K: StaticName> FromRequest for Files<K> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(
            async move { Ok(Files(request.files(K::NAME).to_vec(), std::marker::PhantomData)) },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, PartialEq, Deserialize)]
    struct NewPost {
        title: String,
        draft: bool,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Page {
        page: u32,
    }

    fn shared(request: Request) -> Arc<Request> {
        Arc::new(request)
    }

    #[tokio::test]
    async fn json_extracts_the_body() {
        let request =
            shared(Request::builder().json(&json!({ "title": "Hi", "draft": true })).build());
        let Json(post) = Json::<NewPost>::from_request(request).await.unwrap();
        assert_eq!(post, NewPost { title: "Hi".into(), draft: true });
    }

    #[tokio::test]
    async fn json_reports_a_bad_body_as_a_400() {
        let request = shared(Request::builder().json(&json!({ "title": "Hi" })).build());
        let err = Json::<NewPost>::from_request(request).await.unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn query_coerces_strings() {
        let request = shared(Request::builder().uri("/x?page=3").build());
        let Query(page) = Query::<Page>::from_request(request).await.unwrap();
        assert_eq!(page, Page { page: 3 });
    }

    #[tokio::test]
    async fn form_reads_the_merged_input() {
        let request = shared(
            Request::builder()
                .method(Method::POST)
                .uri("/x?page=1")
                .form(&[("title", "Hi"), ("draft", "1")])
                .build(),
        );
        let Form(post) = Form::<NewPost>::from_request(request).await.unwrap();
        assert_eq!(post, NewPost { title: "Hi".into(), draft: true });
    }

    #[tokio::test]
    async fn path_reads_a_single_scalar_parameter() {
        let request = shared(Request::builder().route_param("post", "42").build());
        let Path(id) = Path::<u64>::from_request(request).await.unwrap();
        assert_eq!(id, 42);
    }

    #[tokio::test]
    async fn path_reads_a_struct_of_parameters() {
        #[derive(Debug, PartialEq, Deserialize)]
        struct Params {
            post: u64,
            comment: u64,
        }

        let request =
            shared(Request::builder().route_param("post", "1").route_param("comment", "2").build());
        let Path(params) = Path::<Params>::from_request(request).await.unwrap();
        assert_eq!(params, Params { post: 1, comment: 2 });
    }

    #[tokio::test]
    async fn path_refuses_to_guess_between_two_parameters() {
        let request =
            shared(Request::builder().route_param("post", "1").route_param("comment", "2").build());
        let err = Path::<u64>::from_request(request).await.unwrap_err();
        assert_eq!(err.status(), 400);
    }

    crate::static_name!(XRequestId, "x-request-id");
    crate::static_name!(XMissing, "x-missing");
    crate::static_name!(DocField, "doc");

    #[tokio::test]
    async fn header_and_bearer_extractors() {
        let request = shared(
            Request::builder()
                .header("x-request-id", "abc")
                .header("authorization", "Bearer tok")
                .build(),
        );

        let Header(id, ..) =
            Header::<XRequestId>::from_request(Arc::clone(&request)).await.unwrap();
        assert_eq!(id, "abc");

        let Bearer(token) = Bearer::from_request(Arc::clone(&request)).await.unwrap();
        assert_eq!(token, "tok");

        let missing = Header::<XMissing>::from_request(request).await.unwrap_err();
        assert_eq!(missing.status(), 400);
    }

    #[tokio::test]
    async fn a_missing_bearer_token_is_a_401() {
        let request = shared(Request::builder().build());
        assert_eq!(Bearer::from_request(request).await.unwrap_err().status(), 401);
    }

    #[tokio::test]
    async fn option_swallows_the_failure() {
        let request = shared(Request::builder().build());
        let extracted = Option::<Json<NewPost>>::from_request(request).await.unwrap();
        assert!(extracted.is_none());
    }

    #[tokio::test]
    async fn result_hands_the_failure_to_the_handler() {
        let request = shared(Request::builder().build());
        let extracted = Result::<Json<NewPost>>::from_request(request).await.unwrap();
        assert!(extracted.is_err());
    }

    #[tokio::test]
    async fn the_whole_request_is_an_extractor() {
        let request = shared(Request::builder().uri("/here").build());
        let extracted = Arc::<Request>::from_request(request).await.unwrap();
        assert_eq!(extracted.path(), "/here");
    }

    #[tokio::test]
    async fn raw_body_and_files() {
        let boundary = "B";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"a.txt\"\r\n\r\nDATA\r\n--{boundary}--\r\n"
        );
        let request = shared(
            Request::builder()
                .method(Method::POST)
                .header("content-type", &format!("multipart/form-data; boundary={boundary}"))
                .body(body.clone())
                .build(),
        );

        let RawBody(raw) = RawBody::from_request(Arc::clone(&request)).await.unwrap();
        assert_eq!(raw.len(), body.len());

        let Files(files, ..) = Files::<DocField>::from_request(request).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].bytes().as_ref(), b"DATA");
    }
}
