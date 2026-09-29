//! The neuradex documentation page, rendered from an `utoipa::openapi::OpenApi`.
//!
//! This crate is presentation and content in one: it walks the serialized document — the
//! exact JSON `/openapi.json` serves — to build the page model, bundles the theme's
//! stylesheets, and renders the askama template into the single self-contained HTML
//! document the service serves at `/docs`. The utoipa dependency is the object model
//! only; there is no axum here and no derive-macro machinery.
//!
//! # Panics
//!
//! Nothing in this crate panics on its own; see [`render`] for the one exception.

#[cfg(not(debug_assertions))]
use std::sync::OnceLock;

use askama::Template;

mod assets;
mod bundle;
mod html;
mod model;
mod tree;

/// Renders the complete, self-contained documentation page from `document`.
///
/// The page is the one the service embeds and serves at `/docs`; because it is rendered
/// from the same document the router derives and `/openapi.json` serves, the three cannot
/// drift apart. Stylesheets and scripts are inlined — the output needs no external
/// assets and no runtime spec fetch.
///
/// In debug builds every call re-reads the theme sources from disk and re-bundles them,
/// so local edits show up on browser refresh without recompiling; release builds embed
/// the sources and bundle the stylesheet once. The document itself is walked on every
/// call — the page-level caching (if any) is the caller's, and is sound because the
/// document is fixed per process (the router's routes are static).
///
/// # Panics
///
/// Panics only if the document fails to serialize to JSON — practically impossible for a
/// utoipa-derived document.
#[must_use]
pub fn render(document: &utoipa::openapi::OpenApi) -> String {
    let spec = serde_json::to_value(document).expect("the OpenAPI document serializes to JSON");
    let page = model::page(&spec);

    let template = PageTemplate {
        page: &page,
        style: &page_style(),
        behavior: &assets::behavior_raw(),
        theme_bootstrap: &assets::bootstrap_raw(),
    };
    template
        .render()
        .expect("the documentation-page template renders")
}

/// The bundled stylesheet for the page: re-bundled per call in debug builds, bundled once
/// in release builds.
fn page_style() -> String {
    #[cfg(debug_assertions)]
    {
        bundle::bundle(&assets::style_raw())
    }
    #[cfg(not(debug_assertions))]
    {
        static BUNDLED: OnceLock<String> = OnceLock::new();
        BUNDLED
            .get_or_init(|| bundle::bundle(&assets::style_raw()))
            .clone()
    }
}

/// The theme's stylesheets, concatenated in canonical order.
///
/// These are the raw authored sources, for consumers that only want the theming. The
/// value is cached at first call: unlike [`render`], these accessors cannot honor
/// per-call disk reads in debug builds.
#[must_use]
pub fn style() -> &'static str {
    assets::style()
}

/// The theme's behavior scripts, concatenated in canonical order (the
/// namespace-creating `helpers.js` first).
///
/// These are the raw authored sources, for consumers that only want the theming. The
/// value is cached at first call: unlike [`render`], these accessors cannot honor
/// per-call disk reads in debug builds.
#[must_use]
pub fn behavior_script() -> &'static str {
    assets::behavior()
}

/// The pre-paint theme bootstrap (`theme/bootstrap.js`); must run from `<head>`, before
/// first paint.
///
/// The value is cached at first call: unlike [`render`], these accessors cannot honor
/// per-call disk reads in debug builds.
#[must_use]
pub fn theme_bootstrap() -> &'static str {
    assets::bootstrap()
}

/// The askama root template; see `templates/page.html`. The partials it includes share
/// this context (`page`, `style`, `behavior`, `theme_bootstrap`, and any loop variables
/// in scope at the include).
#[derive(Template)]
#[template(path = "page.html")]
struct PageTemplate<'a> {
    /// The page data.
    page: &'a model::Page,
    /// The bundled stylesheet, inlined into `<style>`.
    style: &'a str,
    /// The behavior scripts, inlined into the trailing `<script>`.
    behavior: &'a str,
    /// The theme bootstrap, inlined into the `<head>` script.
    theme_bootstrap: &'a str,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A synthetic document with the same shapes the service produces: tagged operations,
    /// documented query parameters, a referenced response schema, and a self-referencing
    /// schema for the recursion guard.
    fn document() -> utoipa::openapi::OpenApi {
        serde_json::from_value(json!({
            "openapi": "3.1.0",
            "info": {
                "title": "test",
                "version": "1.2.3",
                "description": "A `test` service: through `</head>` and beyond."
            },
            "tags": [
                {"name": "things", "description": "Listing things."},
                {"name": "other", "description": "Other things."}
            ],
            "paths": {
                "/ping": {"get": {
                    "tags": ["other"],
                    "summary": "Ping the service.",
                    "description": "Returns `204 No Content`.",
                    "responses": {"204": {"description": "Pong."}}
                }},
                "/things": {"get": {
                    "tags": ["things"],
                    "summary": "List things.",
                    "description": "Lists `things`.",
                    "parameters": [
                        {"name": "limit", "in": "query", "required": false,
                         "description": "Max items.",
                         "schema": {"type": "integer", "format": "int64", "minimum": 1},
                         "example": 10},
                        {"name": "kind", "in": "query", "required": false,
                         "description": "Filter by kind.",
                         "style": "form", "explode": true,
                         "schema": {"type": "array", "items": {"type": "string"}},
                         "example": ["big"]},
                        {"name": "wicked", "in": "query", "required": false,
                         "description": "Escape hatch.",
                         "schema": {"type": "string"},
                         "example": "\"></script>"}
                    ],
                    "responses": {
                        "200": {"description": "The things.",
                                "content": {"application/json": {"schema":
                                    {"$ref": "#/components/schemas/Thing"}}}},
                        "404": {"description": "No things."}
                    }
                }}
            },
            "components": {"schemas": {
                "Thing": {"type": "object", "description": "A thing.",
                          "properties": {
                              "name": {"type": "string"},
                              "next": {"$ref": "#/components/schemas/Thing"}
                          },
                          "required": ["name"]}
            }}
        }))
        .expect("the fixture document deserializes")
    }

    #[test]
    fn renders_the_full_page_shell() {
        let page_html = render(&document());

        assert!(page_html.starts_with("<!doctype html>"));
        assert!(page_html.contains(r#"lang="en" data-docs="neuradex""#));
        assert!(page_html.contains("<title>test · API documentation</title>"));
        assert!(page_html.contains(r#"<h1 class="wordmark">test</h1>"#));
        assert!(page_html.contains(r#"<span class="version" id="spec-version">v1.2.3</span>"#));
        assert!(page_html.contains(r#"<span id="footer-version">v1.2.3</span>"#));
        assert!(
            page_html.contains(r#"<a class="spec-link" href="/openapi.json">openapi.json</a>"#)
        );
        assert!(!page_html.contains("1 endpoint · OpenAPI 3.1.0"));
        assert!(page_html.contains("2 endpoints · OpenAPI 3.1.0"));
        // The theme assets are inlined: stylesheet, behavior scripts, and the pre-paint
        // bootstrap.
        assert!(page_html.contains("--accent-dim:"));
        assert!(page_html.contains("querySelectorAll('.tryit')"));
        assert!(page_html.contains("prefers-color-scheme"));
        assert!(page_html.contains("window.docsUI"));
    }

    #[test]
    fn orders_endpoints_by_tag_priority_then_document_order() {
        let page_html = render(&document());

        let ping = page_html.find(r#"id="ep-ping""#).expect("ping section");
        let things = page_html.find(r#"id="ep-things""#).expect("things section");
        // `/things` (tag `things`) comes before `/ping` (tag `other`) even though `/ping`
        // is registered first.
        assert!(things < ping);
    }

    #[test]
    fn renders_endpoint_sections() {
        let page_html = render(&document());

        assert!(page_html.contains(r#"<article class="ep" id="ep-things">"#));
        assert!(page_html.contains(r#"<span class="toc-num">01</span>"#));
        assert!(page_html.contains(r#"<code class="ep-path">/things</code>"#));
        assert!(page_html.contains("<code>things</code>"));
    }

    #[test]
    fn renders_parameter_tables_and_request_builders() {
        let page_html = render(&document());

        assert!(
            page_html.contains(
                r#"<span class="param-name">limit</span><span class="opt">optional</span>"#
            )
        );
        assert!(
            page_html.contains(r#"<span class="ann-k">min</span> <span class="ann-v">1</span>"#)
        );
        assert!(
            page_html
                .contains(r#"<span class="ann-k">example</span> <span class="ann-v">10</span>"#)
        );
        // Array parameters are annotated as repeated keys and seed the builder input.
        assert!(page_html.contains(
            r#"<span class="ann-k">send as</span> <span class="ann-v">repeated keys</span>"#
        ));
        assert!(page_html.contains(r#"data-name="kind" data-array="true""#));
        assert!(page_html.contains(r#"value="big""#));
        // The parameterless endpoint renders its empty-state note.
        assert!(page_html.contains("No query parameters — the endpoint takes none."));
    }

    #[test]
    fn links_responses_to_the_schema_appendix() {
        let page_html = render(&document());

        assert!(page_html.contains(r##"<a href="#schema-Thing">→ Thing</a>"##));
        assert!(page_html.contains(r#"<span class="status-pill small status-ok">200</span>"#));
        assert!(page_html.contains(r#"<span class="status-pill small status-err">404</span>"#));
        assert!(page_html.contains(r#"<div class="schema-anchor" id="schema-Thing">"#));
        assert!(page_html.contains("↺ recursive reference to Thing"));
    }

    #[test]
    fn renders_the_schema_appendix_with_tree_controls() {
        let page_html = render(&document());

        assert!(page_html.contains(r#"<article class="ep ep-appendix" id="appendix-schemas">"#));
        assert!(
            page_html.contains(r#"<div class="schema-tree"><details class="schema-node" open>"#)
        );
        assert!(page_html.contains(r#"data-tree-action="expand""#));
        assert!(page_html.contains(r#"data-tree-action="collapse""#));
    }

    #[test]
    fn escapes_markup_from_the_document() {
        let page_html = render(&document());

        assert!(page_html.contains("<code>&lt;/head&gt;</code>"));
        // The document head's own closing tag is the only raw `</head>` in the page.
        assert_eq!(page_html.match_indices("</head>").count(), 1);
    }

    #[test]
    fn escapes_seed_values_in_attributes() {
        let page_html = render(&document());

        // The seed `"></script>` cannot break out of the value attribute: askama's html
        // escaper uses numeric character references.
        assert!(page_html.contains(r#"value="&#34;&#62;&#60;/script&#62;""#));
        assert!(!page_html.contains("<script>alert"));
    }

    #[test]
    fn renders_deterministically() {
        assert_eq!(render(&document()), render(&document()));
    }
}
