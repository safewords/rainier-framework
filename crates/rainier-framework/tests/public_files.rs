//! `public/` against a real filesystem.
//!
//! The unit tests beside [`rainier_framework::public`] cover path resolution,
//! which is pure and is where traversal is decided. These cover what only a
//! filesystem can be wrong about: a symlink that points out of the root, a
//! directory where a file was expected, and the conditional request that makes
//! an unchanged asset cost nothing.

use std::fs;

use rainier_framework::public::PublicFiles;
use rainier_http::{Body, Method, Request, StatusCode};

/// A fresh tree per test.
///
/// Keyed by the caller's name, not just the process id. Every test called this
/// and it rewrites `robots.txt` each time, so with one shared directory a test
/// starting up would rewrite the file another was midway through checking.
/// `an_unchanged_file_is_answered_304` reads the etag, re-requests with
/// `if-none-match`, and expects 304 — and the rewrite moved the mtime under it,
/// changing the etag and answering 200.
///
/// It passed on Windows and failed on Linux CI, which is the signature of a
/// race decided by filesystem timestamp resolution rather than by anything the
/// code does.
fn root(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rainier-public-{}-{test}", std::process::id()));
    let _ = fs::create_dir_all(dir.join("img"));
    let _ = fs::create_dir_all(dir.join("empty"));
    fs::write(dir.join("robots.txt"), "User-agent: *\n").unwrap();
    fs::write(dir.join("img/logo.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(dir.join(".env"), "SECRET=hunter2\n").unwrap();
    dir
}

fn get(path: &str) -> Request {
    Request::builder().method(Method::GET).uri(path).build()
}

#[tokio::test]
async fn a_file_is_served_with_the_type_its_extension_names() {
    let files = PublicFiles::at(root("a_file_is_served_with_the_type_its_extension_names"));

    let response = files.serve(&get("/robots.txt")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.header("content-type"), Some("text/plain; charset=utf-8"));
    assert!(response.header("etag").is_some(), "no etag, so every request re-sends the file");
}

#[tokio::test]
async fn a_nested_file_is_served() {
    let files = PublicFiles::at(root("a_nested_file_is_served"));

    let response = files.serve(&get("/img/logo.png")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.header("content-type"), Some("image/png"));
}

#[tokio::test]
async fn a_missing_file_is_a_404_and_not_an_error() {
    let files = PublicFiles::at(root("a_missing_file_is_a_404_and_not_an_error"));

    assert_eq!(files.serve(&get("/nope.txt")).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_dotfile_is_not_served() {
    // The one that ends a company.
    let files = PublicFiles::at(root("a_dotfile_is_not_served"));

    assert_eq!(files.serve(&get("/.env")).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn traversal_does_not_reach_outside_the_root() {
    let dir = root("traversal_does_not_reach_outside_the_root");
    fs::write(dir.parent().unwrap().join("rainier-public-secret.txt"), "no").unwrap();

    let files = PublicFiles::at(&dir);

    for hostile in [
        "/../rainier-public-secret.txt",
        "/img/../../rainier-public-secret.txt",
        "/%2e%2e/rainier-public-secret.txt",
    ] {
        assert_eq!(files.serve(&get(hostile)).await.status(), StatusCode::NOT_FOUND, "{hostile}");
    }
}

#[tokio::test]
async fn a_directory_serves_nothing_without_an_index() {
    // A directory that serves something unasked is how a listing becomes a
    // disclosure.
    let files = PublicFiles::at(root("a_directory_serves_nothing_without_an_index"));

    assert_eq!(files.serve(&get("/empty")).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(files.serve(&get("/")).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_index_is_served_for_the_root_when_one_is_configured() {
    let dir = root("an_index_is_served_for_the_root_when_one_is_configured");
    fs::write(dir.join("index.html"), "<!doctype html><title>hi</title>").unwrap();

    let files = PublicFiles::at(&dir).with_index("index.html");
    let response = files.serve(&get("/")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.header("content-type"), Some("text/html; charset=utf-8"));
}

#[tokio::test]
async fn an_unchanged_file_is_answered_304() {
    // The whole point of the etag: the second request for an asset costs
    // headers rather than the file.
    let files = PublicFiles::at(root("an_unchanged_file_is_answered_304"));

    let first = files.serve(&get("/robots.txt")).await;
    let tag = first.header("etag").expect("etag").to_string();

    let conditional = Request::builder()
        .method(Method::GET)
        .uri("/robots.txt")
        .header("if-none-match", &tag)
        .build();

    assert_eq!(files.serve(&conditional).await.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a_cache_control_is_sent_when_one_is_configured() {
    let files = PublicFiles::at(root("a_cache_control_is_sent_when_one_is_configured"))
        .cached_for("public, max-age=31536000, immutable");

    let response = files.serve(&get("/robots.txt")).await;

    assert_eq!(response.header("cache-control"), Some("public, max-age=31536000, immutable"));
}

#[tokio::test]
async fn none_is_sent_when_none_is_configured() {
    // Not a guessed default. A `max-age` on a filename that is not
    // content-hashed is a deploy nobody receives.
    let files = PublicFiles::at(root("none_is_sent_when_none_is_configured"));

    assert_eq!(files.serve(&get("/robots.txt")).await.header("cache-control"), None);
}

/// A file over the in-memory limit, with every byte different from its
/// neighbours' so an off-by-one in a range or a chunk boundary shows.
fn big(dir: &std::path::Path) -> Vec<u8> {
    let len = rainier_framework::public::IN_MEMORY_LIMIT as usize * 3 + 12_345;
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    fs::write(dir.join("disk.img"), &bytes).unwrap();
    bytes
}

fn with(path: &str, headers: &[(&str, &str)]) -> Request {
    let mut builder = Request::builder().method(Method::GET).uri(path);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.build()
}

#[tokio::test]
async fn a_big_file_streams_rather_than_being_read_whole() {
    // A disk image read whole is a process holding a gigabyte per download.
    let dir = root("a_big_file_streams_rather_than_being_read_whole");
    let expected = big(&dir);
    let files = PublicFiles::at(&dir);

    let mut response = files.serve(&get("/disk.img")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(matches!(response.body(), Body::Stream(_)), "{:?}", response.body());
    assert_eq!(response.header("content-length"), Some(expected.len().to_string().as_str()));
    let body = response.take_body().collect().await.unwrap();
    assert!(body[..] == expected[..], "the streamed bytes are not the file");
}

#[tokio::test]
async fn a_small_file_stays_in_memory_so_it_can_be_compressed() {
    let files = PublicFiles::at(root("a_small_file_stays_in_memory_so_it_can_be_compressed"));

    let response = files.serve(&get("/robots.txt")).await;

    assert!(matches!(response.body(), Body::Bytes(_)), "{:?}", response.body());
    assert_eq!(response.header("accept-ranges"), Some("bytes"));
}

#[tokio::test]
async fn a_range_is_answered_206_with_exactly_those_bytes() {
    let dir = root("a_range_is_answered_206_with_exactly_those_bytes");
    let expected = big(&dir);
    let len = expected.len();
    let files = PublicFiles::at(&dir);

    // One small range from the middle, and one big enough to stream.
    for (asked, start, end) in [
        ("bytes=1000-1999".to_string(), 1000, 1999),
        (format!("bytes={}-", 100), 100, len - 1),
        ("bytes=-10".to_string(), len - 10, len - 1),
    ] {
        let mut response = files.serve(&with("/disk.img", &[("range", &asked)])).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{asked}");
        assert_eq!(
            response.header("content-range"),
            Some(format!("bytes {start}-{end}/{len}").as_str()),
            "{asked}"
        );
        assert_eq!(
            response.header("content-length"),
            Some((end - start + 1).to_string().as_str()),
            "{asked}"
        );
        let body = response.take_body().collect().await.unwrap();
        assert!(body[..] == expected[start..=end], "{asked}: wrong bytes");
    }
}

#[tokio::test]
async fn a_range_of_an_older_version_gets_the_whole_file() {
    // `If-Range` naming another etag: splicing a range of this version into a
    // copy of that one would be a corrupt file.
    let dir = root("a_range_of_an_older_version_gets_the_whole_file");
    let files = PublicFiles::at(&dir);
    let tag = files.serve(&get("/robots.txt")).await.header("etag").unwrap().to_string();

    let stale =
        files.serve(&with("/robots.txt", &[("range", "bytes=0-3"), ("if-range", "\"0-0\"")])).await;
    assert_eq!(stale.status(), StatusCode::OK);

    let current =
        files.serve(&with("/robots.txt", &[("range", "bytes=0-3"), ("if-range", &tag)])).await;
    assert_eq!(current.status(), StatusCode::PARTIAL_CONTENT);
}

#[tokio::test]
async fn a_range_past_the_end_is_416() {
    let files = PublicFiles::at(root("a_range_past_the_end_is_416"));

    let response = files.serve(&with("/robots.txt", &[("range", "bytes=5000-")])).await;

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.header("content-range"), Some("bytes */14"));
}

#[tokio::test]
async fn a_head_of_a_big_file_has_its_length_and_no_body() {
    let dir = root("a_head_of_a_big_file_has_its_length_and_no_body");
    let expected = big(&dir);
    let files = PublicFiles::at(&dir);

    let head = Request::builder().method(Method::HEAD).uri("/disk.img").build();
    let response = files.serve(&head).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.header("content-length"), Some(expected.len().to_string().as_str()));
    assert!(response.body().is_empty());
}
