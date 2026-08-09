//! `public/` against a real filesystem.
//!
//! The unit tests beside [`rainier_framework::public`] cover path resolution,
//! which is pure and is where traversal is decided. These cover what only a
//! filesystem can be wrong about: a symlink that points out of the root, a
//! directory where a file was expected, and the conditional request that makes
//! an unchanged asset cost nothing.

use std::fs;

use rainier_framework::public::PublicFiles;
use rainier_http::{Method, Request, StatusCode};

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
