//! `docs/` is the website's documentation, and `docs/_sidebar.json` is its
//! table of contents: rainierframework.io builds its docs section from this
//! directory as it is on `main`, in the order the sidebar gives.
//!
//! So the two must agree, and this is where a disagreement is caught — here,
//! on the commit that caused it, rather than as a failed website build later.
//! Every page is listed once; every listed page exists. `README.md` is the
//! sidebar's `overview`.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn docs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

fn slugs_on_disk() -> BTreeSet<String> {
    std::fs::read_dir(docs())
        .expect("docs/ exists")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let slug = name.strip_suffix(".md")?;
            Some(if slug == "README" { "overview".to_string() } else { slug.to_string() })
        })
        .collect()
}

fn slugs_in_sidebar() -> Vec<String> {
    let raw = std::fs::read_to_string(docs().join("_sidebar.json")).expect("docs/_sidebar.json");
    let groups: serde_json::Value = serde_json::from_str(&raw).expect("the sidebar is JSON");
    let mut slugs = Vec::new();
    for group in groups.as_array().expect("an array of groups") {
        assert!(group["group"].is_string(), "every group has a name: {group}");
        for item in group["items"].as_array().expect("every group has items") {
            let slug = match item {
                serde_json::Value::String(slug) => slug.clone(),
                other => other["slug"].as_str().expect("an item is a slug or {slug, title}").into(),
            };
            slugs.push(slug);
        }
    }
    slugs
}

#[test]
fn every_doc_is_in_the_sidebar_once_and_every_entry_is_a_doc() {
    let on_disk = slugs_on_disk();
    let listed = slugs_in_sidebar();

    let mut seen = BTreeSet::new();
    let twice: Vec<&String> = listed.iter().filter(|s| !seen.insert(*s)).collect();
    assert!(twice.is_empty(), "listed more than once in docs/_sidebar.json: {twice:?}");

    let missing: Vec<&String> = on_disk.iter().filter(|s| !seen.contains(*s)).collect();
    assert!(
        missing.is_empty(),
        "docs with no place in docs/_sidebar.json (the website would not show them): {missing:?}"
    );

    let dangling: Vec<&String> = listed.iter().filter(|s| !on_disk.contains(*s)).collect();
    assert!(dangling.is_empty(), "docs/_sidebar.json lists docs that do not exist: {dangling:?}");
}
