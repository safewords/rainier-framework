//! Compressing responses on the way out — [`Compress`].
//!
//! ```ignore
//! registry.global(Compress::new());
//! ```
//!
//! A JSON list of two hundred records is mostly repeated key names, and gzip
//! takes it to roughly a tenth. On a mobile connection that is the difference
//! between a page that appears and a page that arrives.
//!
//! # What it deliberately leaves alone
//!
//! - **Small bodies.** Below [`min_size`](Compress::min_size) the gzip header
//!   and trailer cost more than the saving, and the CPU is spent for nothing.
//! - **Already-compressed types.** A PNG, a JPEG, a zip, a woff2 — running
//!   deflate over them adds a few bytes and a few milliseconds and takes away
//!   nothing.
//! - **Streaming bodies.** A server-sent-events response works by arriving in
//!   pieces; buffering it to compress it would hold every event until the
//!   stream ended, which is the one thing that must not happen to it.
//! - **Anything with a `content-encoding` already.** Somebody has decided.
//!
//! # `Vary: accept-encoding`
//!
//! Set on every response that *could* have been compressed, whether or not it
//! was. Without it a shared cache can hand a gzipped body to a client that did
//! not ask for one — which reads, at the far end, as a corrupt response from
//! an endpoint that works fine when tested directly.
//!
//! # It compresses on the runtime thread
//!
//! Deflating a few hundred kilobytes is a millisecond or two, and doing it
//! inline avoids a `spawn_blocking` hop per response. A route that returns
//! megabytes should raise [`min_size`], or not use this.
//!
//! [`min_size`]: Compress::min_size

use std::io::Write;

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::Compression;
use rainier_http::{Body, Request, Response};

use crate::pipeline::{Middleware, Next};

/// The default below which compressing costs more than it saves.
const DEFAULT_MIN_SIZE: usize = 1024;

/// Which encoding was negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Gzip,
    Deflate,
}

impl Encoding {
    fn header_value(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
        }
    }
}

/// Gzips or deflates a response when the client asked for it and the body is
/// worth it.
#[derive(Debug, Clone)]
pub struct Compress {
    min_size: usize,
    level: u32,
}

impl Default for Compress {
    fn default() -> Self {
        Self::new()
    }
}

impl Compress {
    /// Compression at the default level, for bodies of 1 KiB or more.
    pub fn new() -> Self {
        Self { min_size: DEFAULT_MIN_SIZE, level: Compression::default().level() }
    }

    /// Only compress bodies of at least this many bytes.
    #[must_use = "this returns a configured middleware rather than configuring in place"]
    pub fn min_size(mut self, bytes: usize) -> Self {
        self.min_size = bytes;
        self
    }

    /// The deflate level, `0` (none) to `9` (smallest).
    ///
    /// The default is `6`. `9` costs noticeably more CPU for a few percent,
    /// which is rarely the trade a web server wants to make per request.
    #[must_use = "this returns a configured middleware rather than configuring in place"]
    pub fn level(mut self, level: u32) -> Self {
        self.level = level.min(9);
        self
    }
}

#[async_trait::async_trait]
impl Middleware for Compress {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let accepted = request.header("accept-encoding").map(str::to_owned);

        let mut response = next.run(request).await;

        // Somebody already encoded this — a pre-compressed asset off disk, or
        // another layer of middleware.
        if response.headers().contains_key("content-encoding") {
            return response;
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !worth_compressing(&content_type) {
            return response;
        }

        // Compressible by type, so a cache must key on the request's
        // `accept-encoding` even if this particular response went out plain.
        response = response.with_header("vary", "accept-encoding");

        let Some(encoding) = accepted.as_deref().and_then(best_encoding) else {
            return response;
        };

        // Only an in-memory body, and *checked* before taking it: a stream
        // taken and not put back is a response whose body silently vanished.
        if !matches!(response.body(), Body::Bytes(_)) {
            return response;
        }
        let Body::Bytes(bytes) = response.take_body() else {
            unreachable!("just checked");
        };

        if bytes.len() < self.min_size {
            return response.with_body(bytes);
        }

        match compress(&bytes, encoding, self.level) {
            Some(compressed) if compressed.len() < bytes.len() => response
                .with_body(compressed)
                .with_header("content-encoding", encoding.header_value()),
            // Either it failed, or it came out bigger — which happens with
            // small or already-dense payloads. Either way the original is the
            // better answer.
            _ => response.with_body(bytes),
        }
    }

    fn name(&self) -> &'static str {
        "Compress"
    }
}

/// The best encoding the client will take, or `None` for none of ours.
///
/// Understands q-values, so `gzip;q=0` means *not* gzip — a client that says
/// so usually has a reason.
fn best_encoding(accept: &str) -> Option<Encoding> {
    let mut best: Option<(Encoding, f32)> = None;
    let mut wildcard: Option<f32> = None;

    for part in accept.split(',') {
        let mut pieces = part.split(';');
        let name = pieces.next()?.trim().to_ascii_lowercase();

        let quality = pieces
            .find_map(|piece| piece.trim().strip_prefix("q=").map(str::to_owned))
            .and_then(|q| q.parse::<f32>().ok())
            .unwrap_or(1.0);

        if quality <= 0.0 {
            continue;
        }

        let encoding = match name.as_str() {
            "gzip" | "x-gzip" => Encoding::Gzip,
            "deflate" => Encoding::Deflate,
            "*" => {
                wildcard = Some(quality);
                continue;
            }
            _ => continue,
        };

        // Gzip wins a tie: every proxy, CDN and client handles it without
        // surprises, and raw deflate has a long history of being sent with the
        // wrong framing.
        let better = match best {
            None => true,
            Some((_, best_q)) if quality > best_q => true,
            Some((best_encoding, best_q)) => {
                quality == best_q && encoding == Encoding::Gzip && best_encoding != Encoding::Gzip
            }
        };
        if better {
            best = Some((encoding, quality));
        }
    }

    best.map(|(encoding, _)| encoding).or(wildcard.map(|_| Encoding::Gzip))
}

/// Whether a body of this content type is worth the CPU.
fn worth_compressing(content_type: &str) -> bool {
    let content_type = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();

    if content_type.is_empty() {
        // Nothing said what this is. Compressing bytes of unknown shape is a
        // guess, and a wrong guess here costs CPU on every response.
        return false;
    }

    // `image/svg+xml` is text wearing an image prefix, and so is every
    // `application/vnd.something+json`. The suffix is the more specific fact,
    // so it is checked first.
    if content_type.starts_with("text/")
        || content_type.ends_with("+json")
        || content_type.ends_with("+xml")
    {
        return true;
    }

    // Already compressed, every one of them. Deflating a PNG makes it very
    // slightly larger.
    const DENSE: &[&str] = &[
        "image/",
        "video/",
        "audio/",
        "font/woff",
        "application/zip",
        "application/gzip",
        "application/x-gzip",
        "application/x-bzip",
        "application/x-7z",
        "application/x-rar",
        "application/pdf",
        "application/octet-stream",
        "application/wasm",
    ];
    if DENSE.iter().any(|dense| content_type.starts_with(dense)) {
        return false;
    }

    // The textual `application/*`: json, xml, javascript, x-www-form-urlencoded,
    // and every `+json` / `+xml` vendor type.
    content_type.starts_with("application/")
}

fn compress(bytes: &[u8], encoding: Encoding, level: u32) -> Option<Vec<u8>> {
    let level = Compression::new(level);

    match encoding {
        Encoding::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), level);
            encoder.write_all(bytes).ok()?;
            encoder.finish().ok()
        }
        Encoding::Deflate => {
            let mut encoder = DeflateEncoder::new(Vec::new(), level);
            encoder.write_all(bytes).ok()?;
            encoder.finish().ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use rainier_http::{Method, StatusCode};
    use std::io::Read;

    /// A body big enough to be worth compressing, and repetitive enough to
    /// compress well — which is what a JSON list actually looks like.
    fn json_body() -> String {
        let record = r#"{"id":1,"name":"Ada Lovelace","role":"engineer"},"#;
        format!("[{}]", record.repeat(64))
    }

    async fn respond(accept: Option<&str>, response: Response) -> Response {
        let mut builder = Request::builder().method(Method::GET).uri("/data");
        if let Some(accept) = accept {
            builder = builder.header("accept-encoding", accept);
        }

        let response = std::sync::Arc::new(std::sync::Mutex::new(Some(response)));

        Pipeline::new()
            .through(Compress::new())
            .then(move |_| {
                let response = std::sync::Arc::clone(&response);
                async move { response.lock().unwrap().take().expect("called once") }
            })
            .run(builder.build())
            .await
    }

    fn json(body: String) -> Response {
        Response::ok(body).with_content_type("application/json")
    }

    fn gunzip(bytes: &[u8]) -> String {
        let mut out = String::new();
        flate2::read::GzDecoder::new(bytes).read_to_string(&mut out).unwrap();
        out
    }

    #[tokio::test]
    async fn a_json_body_is_gzipped_and_round_trips() {
        let original = json_body();
        let response = respond(Some("gzip, deflate"), json(original.clone())).await;

        assert_eq!(response.header("content-encoding"), Some("gzip"));
        assert_eq!(response.header("vary"), Some("accept-encoding"));

        let bytes = response.into_bytes().await.unwrap();
        assert!(bytes.len() < original.len() / 2, "{} vs {}", bytes.len(), original.len());
        assert_eq!(gunzip(&bytes), original);
    }

    #[tokio::test]
    async fn a_client_that_asked_for_nothing_gets_the_original() {
        let original = json_body();
        let response = respond(None, json(original.clone())).await;

        assert_eq!(response.header("content-encoding"), None);
        // Still varies: the same URL *would* be compressed for the next client.
        assert_eq!(response.header("vary"), Some("accept-encoding"));
        assert_eq!(response.into_string().await.unwrap(), original);
    }

    #[tokio::test]
    async fn a_small_body_is_left_alone() {
        let response = respond(Some("gzip"), json("[]".to_string())).await;

        assert_eq!(response.header("content-encoding"), None);
        assert_eq!(response.into_string().await.unwrap(), "[]");
    }

    #[tokio::test]
    async fn an_image_is_left_alone() {
        let body = "x".repeat(4096);
        let response =
            respond(Some("gzip"), Response::ok(body).with_content_type("image/png")).await;

        assert_eq!(response.header("content-encoding"), None);
        // Not even a `vary`: this URL will never be compressed, so a cache has
        // nothing to key on.
        assert_eq!(response.header("vary"), None);
    }

    #[tokio::test]
    async fn a_response_that_is_already_encoded_is_not_encoded_twice() {
        let response =
            respond(Some("gzip"), json(json_body()).with_header("content-encoding", "br")).await;

        assert_eq!(response.header("content-encoding"), Some("br"));
    }

    #[tokio::test]
    async fn a_streaming_body_is_never_buffered() {
        // Compressing this would mean holding every event until the stream
        // ended — which for server-sent events is indistinguishable from the
        // endpoint being broken.
        use futures_util::stream;

        let events = stream::iter(vec![
            Ok(bytes::Bytes::from("data: one\n\n")),
            Ok(bytes::Bytes::from("data: two\n\n")),
        ]);
        let streaming = Response::stream(events).with_content_type("text/event-stream");

        let response = respond(Some("gzip"), streaming).await;

        assert_eq!(response.header("content-encoding"), None);
        assert!(matches!(response.body(), Body::Stream(_)));
    }

    #[tokio::test]
    async fn deflate_is_used_when_it_is_the_only_thing_offered() {
        let response = respond(Some("deflate"), json(json_body())).await;

        assert_eq!(response.header("content-encoding"), Some("deflate"));
    }

    #[tokio::test]
    async fn an_encoding_refused_by_q_zero_is_not_used() {
        let response = respond(Some("gzip;q=0, deflate;q=1.0"), json(json_body())).await;

        assert_eq!(response.header("content-encoding"), Some("deflate"));
    }

    #[test]
    fn the_best_offer_wins() {
        assert_eq!(best_encoding("gzip"), Some(Encoding::Gzip));
        assert_eq!(best_encoding("deflate, gzip"), Some(Encoding::Gzip));
        assert_eq!(best_encoding("deflate;q=0.9, gzip;q=0.1"), Some(Encoding::Deflate));
        assert_eq!(best_encoding("*"), Some(Encoding::Gzip));
        assert_eq!(best_encoding("br"), None, "we do not do brotli");
        assert_eq!(best_encoding("identity"), None);
        assert_eq!(best_encoding(""), None);
        assert_eq!(best_encoding("gzip;q=0"), None);
    }

    #[test]
    fn only_the_textual_types_are_compressed() {
        assert!(worth_compressing("text/html; charset=utf-8"));
        assert!(worth_compressing("application/json"));
        assert!(worth_compressing("application/vnd.api+json"));
        assert!(worth_compressing("image/svg+xml"), "SVG is text wearing an image prefix");

        assert!(!worth_compressing("image/png"));
        assert!(!worth_compressing("video/mp4"));
        assert!(!worth_compressing("application/zip"));
        assert!(!worth_compressing("application/octet-stream"));
        assert!(!worth_compressing(""), "nothing said what this is");
    }

    #[tokio::test]
    async fn the_status_and_other_headers_survive() {
        let response = respond(
            Some("gzip"),
            json(json_body()).with_status(StatusCode::CREATED).with_header("location", "/data/1"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.header("location"), Some("/data/1"));
        assert_eq!(response.header("content-encoding"), Some("gzip"));
    }
}
