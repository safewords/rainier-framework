//! Response bodies — [`Body`].
//!
//! Requests and responses are deliberately asymmetric here:
//!
//! - **Request** bodies are always buffered, by the time a [`Request`] exists
//!   (the server reads them under a size limit). That is what lets
//!   [`Request::input`] be a plain synchronous call — a lazily-streamed
//!   request body would
//!   force `.await` into every accessor and every validation rule.
//! - **Response** bodies may stream, because file downloads and long-lived
//!   event streams genuinely need it, and nothing about a response has to be
//!   inspected synchronously after the fact.
//!
//! [`Request`]: crate::Request
//! [`Request::input`]: crate::Request::input

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use rainier_support::Error;

/// A boxed stream of body chunks.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, Error>> + Send>>;

/// An HTTP response body: nothing, some bytes, or a stream of chunks.
#[derive(Default)]
pub enum Body {
    /// No body at all — distinct from a zero-length one, so `204 No Content`
    /// can omit `Content-Length` entirely.
    #[default]
    Empty,
    /// A body already in memory. Its length is known, so `Content-Length` can
    /// be set.
    Bytes(Bytes),
    /// A body produced incrementally. Its length is unknown, so it goes out
    /// chunked.
    Stream(BodyStream),
}

impl Body {
    /// An empty body.
    pub fn empty() -> Self {
        Body::Empty
    }

    /// A body from anything byte-like.
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        Body::Bytes(bytes.into())
    }

    /// A streaming body.
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, Error>> + Send + 'static,
    {
        Body::Stream(Box::pin(stream))
    }

    /// The body's length in bytes, when it is known up front. `None` for a
    /// stream.
    pub fn size_hint(&self) -> Option<u64> {
        match self {
            Body::Empty => Some(0),
            Body::Bytes(bytes) => Some(bytes.len() as u64),
            Body::Stream(_) => None,
        }
    }

    /// Whether this body is definitely empty. `false` for a stream, which may
    /// yet yield nothing — we cannot know without polling it.
    pub fn is_empty(&self) -> bool {
        match self {
            Body::Empty => true,
            Body::Bytes(bytes) => bytes.is_empty(),
            Body::Stream(_) => false,
        }
    }

    /// Collect the whole body into memory.
    ///
    /// Drains a stream to completion, so only call it on a body you know is
    /// bounded — in tests, or on a response you are about to assert against.
    pub async fn collect(self) -> Result<Bytes, Error> {
        match self {
            Body::Empty => Ok(Bytes::new()),
            Body::Bytes(bytes) => Ok(bytes),
            Body::Stream(mut stream) => {
                let mut collected = Vec::new();
                // Poll the stream by hand rather than take a `futures-util`
                // dependency for one `next()`.
                std::future::poll_fn(|cx| loop {
                    match stream.as_mut().poll_next(cx) {
                        Poll::Ready(Some(Ok(chunk))) => collected.extend_from_slice(&chunk),
                        Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                        Poll::Ready(None) => return Poll::Ready(Ok(())),
                        Poll::Pending => return Poll::Pending,
                    }
                })
                .await?;
                Ok(Bytes::from(collected))
            }
        }
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Empty => f.write_str("Body::Empty"),
            Body::Bytes(bytes) => write!(f, "Body::Bytes({} bytes)", bytes.len()),
            Body::Stream(_) => f.write_str("Body::Stream(..)"),
        }
    }
}

impl From<()> for Body {
    fn from(_: ()) -> Self {
        Body::Empty
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Body::Bytes(bytes)
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        Body::Bytes(Bytes::from(bytes))
    }
}

impl From<String> for Body {
    fn from(text: String) -> Self {
        Body::Bytes(Bytes::from(text))
    }
}

impl From<&'static str> for Body {
    fn from(text: &'static str) -> Self {
        Body::Bytes(Bytes::from_static(text.as_bytes()))
    }
}

/// Lets a [`Body`] be handed straight to hyper, which writes responses through
/// the `http-body` trait.
impl http_body::Body for Body {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Error>>> {
        // No `unsafe` needed: every variant is `Unpin` (`Bytes` holds no
        // self-references, and the stream variant is already a `Pin<Box<..>>`,
        // which is `Unpin` whatever it points at), so `Body` is too.
        let this = self.get_mut();
        match this {
            Body::Empty => Poll::Ready(None),
            Body::Bytes(bytes) => {
                if bytes.is_empty() {
                    Poll::Ready(None)
                } else {
                    let chunk = std::mem::take(bytes);
                    Poll::Ready(Some(Ok(http_body::Frame::data(chunk))))
                }
            }
            Body::Stream(stream) => match stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    Poll::Ready(Some(Ok(http_body::Frame::data(chunk))))
                }
                Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Body::Empty => true,
            Body::Bytes(bytes) => bytes.is_empty(),
            Body::Stream(_) => false,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match Body::size_hint(self) {
            Some(len) => http_body::SizeHint::with_exact(len),
            None => http_body::SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn collects_in_memory_bodies() {
        assert_eq!(Body::empty().collect().await.unwrap(), Bytes::new());
        assert_eq!(Body::from("hello").collect().await.unwrap(), Bytes::from("hello"));
    }

    #[tokio::test]
    async fn collects_a_stream() {
        let body =
            Body::from_stream(stream::iter(vec![Ok(Bytes::from("ab")), Ok(Bytes::from("cd"))]));
        assert_eq!(body.collect().await.unwrap(), Bytes::from("abcd"));
    }

    #[tokio::test]
    async fn a_stream_error_surfaces_from_collect() {
        let body = Body::from_stream(stream::iter(vec![
            Ok(Bytes::from("ab")),
            Err(Error::internal("disk died")),
        ]));
        assert_eq!(body.collect().await.unwrap_err().message(), "disk died");
    }

    #[test]
    fn size_hints_are_known_except_for_streams() {
        assert_eq!(Body::empty().size_hint(), Some(0));
        assert_eq!(Body::from("abc").size_hint(), Some(3));
        assert_eq!(Body::from_stream(stream::empty()).size_hint(), None);
    }

    #[test]
    fn emptiness_is_only_claimed_when_certain() {
        assert!(Body::empty().is_empty());
        assert!(Body::from("").is_empty());
        assert!(!Body::from("x").is_empty());
        // A stream might yield nothing, but we cannot know without polling.
        assert!(!Body::from_stream(stream::empty()).is_empty());
    }

    #[tokio::test]
    async fn drives_as_an_http_body() {
        use http_body_util::BodyExt;

        let collected = Body::from("payload").collect().await.unwrap();
        assert_eq!(collected, Bytes::from("payload"));

        // And through the http-body trait itself, which is how hyper reads it.
        let body = Body::from("payload");
        let aggregated = BodyExt::collect(body).await.unwrap().to_bytes();
        assert_eq!(aggregated, Bytes::from("payload"));
    }
}
