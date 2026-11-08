//! End-to-end tests for the `neuradex` binary's command-line interface.

use assert_cmd::Command;

#[test]
fn export_docs_requires_an_output_path() {
    Command::cargo_bin("neuradex")
        .expect("the neuradex binary is built")
        .arg("export-docs")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("export-docs"));
}

#[cfg(feature = "docs")]
#[test]
fn export_docs_writes_the_page_and_spec_without_a_token() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let page = dir.path().join("docs.html");
    let spec = dir.path().join("openapi.json");

    Command::cargo_bin("neuradex")
        .expect("the neuradex binary is built")
        .env_remove("KAGI_SESSION_TOKEN")
        .arg("export-docs")
        .arg(&page)
        .arg("--spec")
        .arg(&spec)
        .assert()
        .success()
        .stdout(predicates::str::contains("wrote"));

    let page = std::fs::read_to_string(page).expect("the documentation page is written");
    assert!(
        page.starts_with("<!doctype html>"),
        "the exported page has the expected document shell"
    );
    assert!(
        page.contains(r#"data-docs="neuradex""#),
        "the exported page has the neuradex marker"
    );

    let spec = std::fs::read_to_string(spec).expect("the OpenAPI document is written");
    let document: serde_json::Value = serde_json::from_str(&spec).expect("valid OpenAPI JSON");

    for path in ["/healthz", "/v1/fetch", "/v1/peek", "/v1/search"] {
        assert!(document["paths"].get(path).is_some(), "missing {path}");
    }
}

#[cfg(not(feature = "docs"))]
#[test]
fn export_docs_reports_a_missing_docs_feature() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    Command::cargo_bin("neuradex")
        .expect("the neuradex binary is built")
        .arg("export-docs")
        .arg(dir.path().join("docs.html"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("without the docs feature"));
}
