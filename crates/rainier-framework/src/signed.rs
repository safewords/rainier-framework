//! Signed routes — [`SignedUrls`] and [`ValidateSignature`].
//!
//! ```ignore
//! // Generating one, from a named route.
//! let link = signed.route("unsubscribe", &[("user", "42")])?;
//! let link = signed.temporary_route("verify-email", expires_at, &[("user", "42")])?;
//!
//! // Checking it.
//! router.get("/verify-email", verify).name("verify-email")
//!     .middleware(ValidateSignature::resolved());
//! ```
//!
//! The point is what
//! it removes: an unsubscribe link, a verification link or a one-time download
//! needs no token table, no lookup and no sweep job, because the URL carries
//! its own proof.
//!
//! Read [`UrlSigner`] for what is signed and — more importantly — for the two
//! things a signature is not: single-use, and secret.

use std::sync::Arc;

use rainier_crypt::UrlSigner;
use rainier_http::{IntoResponse, Request, Response};
use rainier_middleware::{Middleware, MiddlewareStack, Next};
use rainier_routing::UrlGenerator;
use rainier_support::Result;

/// Signed URLs for this application's named routes.
///
/// Bound at boot from the same key ring everything else uses, and composed
/// with the [`UrlGenerator`] so a signed link is built from a **route name**
/// rather than a path somebody typed twice.
pub struct SignedUrls {
    urls: Arc<UrlGenerator>,
    signer: Arc<UrlSigner>,
}

impl SignedUrls {
    /// Sign links to the routes `urls` knows about.
    pub fn new(urls: Arc<UrlGenerator>, signer: Arc<UrlSigner>) -> Self {
        Self { urls, signer }
    }

    /// A signed link to a named route.
    ///
    /// ```ignore
    /// signed.route("unsubscribe", &[("user", "42")])?
    /// // /unsubscribe/42?signature=…
    /// ```
    ///
    /// Route parameters are substituted into the path as usual; anything the
    /// route does not name becomes a query parameter, and is signed with the
    /// rest.
    pub fn route(&self, name: &str, params: &[(&str, &str)]) -> Result<String> {
        self.signer.sign(&self.urls.route(name, params)?)
    }

    /// A signed link that stops working after `expires_at`.
    ///
    /// `expires_at` is seconds since the epoch. It is part of the query and so
    /// part of the signature: moving it invalidates the link.
    ///
    /// This is the one to reach for. An unsigned expiry is a promise; a signed
    /// one is a fact, and a link with no expiry at all lives in a mailbox for
    /// years.
    pub fn temporary_route(
        &self,
        name: &str,
        expires_at: i64,
        params: &[(&str, &str)],
    ) -> Result<String> {
        self.signer.sign_until(&self.urls.route(name, params)?, expires_at)
    }

    /// A signed absolute URL — with the scheme and host in front.
    ///
    /// What goes in an email. Note the host is **not** covered by the
    /// signature; see [`UrlSigner`].
    pub fn absolute_route(&self, name: &str, params: &[(&str, &str)]) -> Result<String> {
        let absolute = self.urls.absolute(name, params)?;
        self.signer.sign(&absolute)
    }

    /// [`absolute_route`](Self::absolute_route), expiring.
    pub fn temporary_absolute_route(
        &self,
        name: &str,
        expires_at: i64,
        params: &[(&str, &str)],
    ) -> Result<String> {
        let absolute = self.urls.absolute(name, params)?;
        self.signer.sign_until(&absolute, expires_at)
    }

    /// Sign a path directly, for a link no named route describes.
    pub fn sign(&self, url: &str) -> Result<String> {
        self.signer.sign(url)
    }

    /// The signer underneath.
    pub fn signer(&self) -> &Arc<UrlSigner> {
        &self.signer
    }
}

impl std::fmt::Debug for SignedUrls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedUrls")
    }
}

/// Refuses a request whose URL is not signed by this application.
///
/// ```ignore
/// router.get("/unsubscribe", unsubscribe).middleware(ValidateSignature::resolved());
/// ```
///
/// Answers `403` for a missing, forged or expired signature — and says which,
/// because "this link has expired" and "this link is not valid" send the
/// reader to different places.
pub struct ValidateSignature {
    signer: Arc<UrlSigner>,
}

impl ValidateSignature {
    /// Check against `signer`.
    pub fn new(signer: Arc<UrlSigner>) -> Self {
        Self { signer }
    }

    /// Check against the signer in the container.
    ///
    /// The usual form, and the reason it is a stack: routes are declared
    /// before the container exists.
    pub fn resolved() -> MiddlewareStack {
        MiddlewareStack::new().resolved(|signer: Arc<UrlSigner>| Self::new(signer))
    }
}

#[async_trait::async_trait]
impl Middleware for ValidateSignature {
    async fn handle(&self, request: Request, next: Next) -> Response {
        // Path and query only. The host is deliberately outside the signature,
        // and including it here would check something that was never signed.
        let url = match request.uri().query() {
            Some(query) => format!("{}?{}", request.uri().path(), query),
            None => request.uri().path().to_string(),
        };

        match self.signer.verify(&url) {
            Ok(()) => next.run(request).await,
            Err(e) => e.into_response(),
        }
    }

    fn name(&self) -> &'static str {
        "ValidateSignature"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::{Key, KeyRing};
    use rainier_http::{Method, StatusCode};
    use rainier_middleware::Pipeline;

    fn signer() -> Arc<UrlSigner> {
        Arc::new(UrlSigner::new(KeyRing::new(Key::generate())))
    }

    fn urls() -> Arc<UrlGenerator> {
        Arc::new(UrlGenerator::from_routes([
            ("unsubscribe".to_string(), "/unsubscribe/{user}".to_string()),
            ("verify".to_string(), "/verify".to_string()),
        ]))
    }

    fn in_an_hour() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
            + 3600
    }

    async fn through(signer: Arc<UrlSigner>, url: &str) -> StatusCode {
        Pipeline::new()
            .through(ValidateSignature::new(signer))
            .then(|_| async { Response::ok("followed") })
            .run(Request::builder().method(Method::GET).uri(url).build())
            .await
            .status()
    }

    #[tokio::test]
    async fn a_signed_route_is_followed() {
        let signer = signer();
        let signed = SignedUrls::new(urls(), Arc::clone(&signer));

        let link = signed.route("unsubscribe", &[("user", "42")]).unwrap();

        assert!(link.starts_with("/unsubscribe/42?"), "{link}");
        assert_eq!(through(signer, &link).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unsigned_request_to_the_same_route_is_refused() {
        assert_eq!(through(signer(), "/unsubscribe/42").await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn tampering_with_the_id_is_refused() {
        // The attack this exists to stop: unsubscribing somebody else.
        let signer = signer();
        let signed = SignedUrls::new(urls(), Arc::clone(&signer));

        let link = signed.route("unsubscribe", &[("user", "42")]).unwrap();
        let tampered = link.replace("/unsubscribe/42", "/unsubscribe/43");

        assert_eq!(through(signer, &tampered).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_expired_link_is_refused_and_says_so() {
        let signer = signer();
        let signed = SignedUrls::new(urls(), Arc::clone(&signer));

        let expired = signed.temporary_route("verify", 1, &[("user", "42")]).unwrap();

        let response = Pipeline::new()
            .through(ValidateSignature::new(signer))
            .then(|_| async { Response::ok("followed") })
            .run(Request::builder().method(Method::GET).uri(&expired).build())
            .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // Which one it is matters: an expired link deserves "here is a new
        // one", and a forged one does not.
        assert!(response.into_string().await.unwrap().contains("expired"));
    }

    #[tokio::test]
    async fn a_live_temporary_link_is_followed() {
        let signer = signer();
        let signed = SignedUrls::new(urls(), Arc::clone(&signer));

        let link = signed.temporary_route("verify", in_an_hour(), &[("user", "42")]).unwrap();

        assert!(link.contains("expires="));
        assert_eq!(through(signer, &link).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_absolute_link_verifies_by_its_path() {
        // What goes in an email. The middleware sees only the path and query,
        // which is exactly what was signed.
        let signer = signer();
        let signed = SignedUrls::new(
            Arc::new(
                UrlGenerator::from_routes([("verify".to_string(), "/verify".to_string())])
                    .with_base("https://app.example.com"),
            ),
            Arc::clone(&signer),
        );

        let absolute = signed.absolute_route("verify", &[("user", "42")]).unwrap();
        assert!(absolute.starts_with("https://app.example.com/verify"), "{absolute}");

        let path = absolute.trim_start_matches("https://app.example.com");
        assert_eq!(through(signer, path).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unknown_route_name_is_an_error_rather_than_an_unsigned_link() {
        let signed = SignedUrls::new(urls(), signer());

        assert!(signed.route("nope", &[]).is_err());
    }
}
