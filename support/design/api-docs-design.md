# neuradex API documentation page — design spec

> **Superseded in part (2026-09):** the runtime-fetch constraints (§3) and the single-file
> constraints (§5) are superseded by the askama + runtime-CSS-bundling implementation in
> `crates/neuradex-docs`: the page is rendered server-side from the OpenAPI document instead
> of fetching `/openapi.json` at runtime, and the theme CSS is bundled at runtime by
> lightningcss rather than kept inline in a single committed file. The design tokens (§1)
> and layout (§2) still apply.

The API documentation page served at `/docs` is restyled to read and feel like a
warm, editorial, paper-textured research article with
serif typography, hairline rules, burnt-orange accents, and richly interactive inline
widgets. This file is the single source of truth for that restyle. The design tokens were
extracted from the reference page's stylesheets.

Reference page inventory (what "heavily based on" means): sticky numbered table of
contents, annotated figures with margin notes, clickable parameter selectors, drag
sliders, animated simulators with live counters, toggleable dot plots, identicon-style
diagrams, KaTeX typesetting, warm-toned syntax-highlighted code blocks, and a
light/dark theme system driven by `data-theme`.

## 1. Design tokens (verbatim from the reference design)

CSS custom properties on `:root`, overridden by `[data-theme="dark"]`:

| Token          | Light                        | Dark                          |
| -------------- | ---------------------------- | ----------------------------- |
| `--page-bg`    | `#f7f4ee`                    | `#0a0a0a`                     |
| `--text-heading` | `#2c2824`                  | `#ebe7df`                     |
| `--text-body`  | `#4f4a45`                    | `#ccc`                        |
| `--text-secondary` | `#6f6962`                | `#8f8a82`                     |
| `--line`       | `#d8d1c7`                    | `#2e2e2e`                     |
| `--line-strong` | `#c8c0b5`                   | `#3a3731`                     |
| `--code-bg`    | `#ece6dd`                    | `#171717`                     |
| `--link`       | `#b14900`                    | `#ee9c40`                     |
| `--link-hover` | `#8f4400`                    | `#ffc15a`                     |
| `--accent-dim` | `#b45d00`                    | `#c47a28`                     |
| `--positive`   | `#28734b`                    | `#75c493`                     |
| `--negative`   | `#ad4038`                    | `#e57870`                     |

`color-scheme: light` / `dark` must be set to match. `color-mix(in srgb, ...)` is used
liberally for soft variants (`--line-soft` = `--line` at 65% over `--page-bg`); reuse
that technique.

Typography (system fallbacks only — the page must never load external assets):

- Body: `"TeX Gyre Pagella", Palatino, "Palatino Linotype", "Book Antiqua", Georgia, serif`
- Display/headings: `"Crimson Text", Georgia, serif` — slightly condensed, large sizes
- Mono: `"Inconsolata", "JetBrains Mono", ui-monospace, "SF Mono", Menlo, monospace` —
  used for method badges, parameter names, code, paths, status pills

Shape and texture: hairline 1px borders in `--line`, tiny radii (`0`–`3px`; `50%` only
for dots/drag handles), generous whitespace, an unobtrusive subtle grid texture is
optional (`--text-heading` at ~3% alpha over the background, as the reference design does for its
research grid). No shadows, no gradients, no rounded cards.

## 2. Layout

1. **Header** (top of the page, article-style): the `neuradex` wordmark in display
   serif, the version from `info.version` in secondary text, a one-line description from
   `info.description`, and links to the raw spec (`/openapi.json`) and the theme toggle.
   Underline links in `--link` with the 65%-alpha underline trick.
2. **Left sidebar** (sticky, ≥ 1024px viewports; collapses above content on narrow):
   numbered endpoint index grouped by tag, like the article's numbered TOC
   ("1. How good are query optimizers, really?"). Entries: `01 Fetch a page`,
   `02 Fetch a page's head metadata`, `03 Search the web`, `04 Check the service health`.
   Active entry highlighted with `--text-heading` + a small `--accent-dim` marker.
3. **Main column**: single article column, `min(72ch, 92vw)`-ish measure, generous
   vertical rhythm between endpoint sections. Each endpoint section reads like an
   article section: display-serif heading with the operation `summary`, body serif
   `description` (rendered with basic inline-code styling for backticks), then the
   widgets below.

## 3. Content rendering (from `/openapi.json`)

The page fetches the spec at runtime and renders it — nothing is hardcoded:

- `GET /openapi.json` by default; an optional `?spec=<url>` query override for local
  preview (`support/design/openapi-fixture.json` is a checked-in sample). If the fetch
  fails, render a quiet inline notice in `--negative` with a retry link; never a blank page.
- Endpoint sections are grouped by `tags` (order: fetch, peek, search, then the rest),
  numbered sequentially. `summary` is the section heading; `description` is the prose
  (backticks → inline `<code>` with `--code-bg`).
- **Parameters widget**: an editorial table — hairline rules only — one row per query
  parameter: mono name, required marker (`--negative` asterisk or "optional" in
  `--text-secondary`), serif description, and a value column of constraint annotations
  (`default`, `minimum`, `maximum`, `example`, `style/explode`) in the margin-note
  aesthetic of the article's annotations (small, `--text-secondary`, mono values).
- **Request builder / "Try it" widget** (the centerpiece — the reference design's interactive figures
  translated to an API playground):
  - One text input per parameter, seeded from the spec's `example`/`default` values.
  - `Send` button styled like the article's action controls: flat, `--accent-dim`
    border/text, hover fill; while in flight, an animated state (progress dots).
  - Builds the query string form-style with `explode` for array params, issues the
    same-origin `fetch`, and renders: a status pill (2xx in `--positive`, 4xx/5xx in
    `--negative`, network error as inline text), latency in ms (mono, like the
    article's "96 ms / 118 ms" labels), response size, and the body as
    syntax-highlighted JSON on `--code-bg` (warm palette: keys in `--accent-dim`,
    strings in `--positive`, numbers in `--link`, booleans/null in `--text-secondary`).
  - The error envelope (`{"error": {"type", "message"}}`) gets a friendly rendering:
    type as a pill, message as prose.
  - Requests are GET-only; the playground must never follow non-same-origin URLs.
- **Copy as cURL** per endpoint: builds the full curl line with the current builder
  values; on click, copies and flashes the button text to "copied" for ~1.5s.
- **Schema inspector**: every `$ref` in a response (`PageMetadata`, `Metrics`,
  `SearchMetrics`, `SearchResult`, `RedirectHop`, `ErrorBody`) renders as a collapsible
  tree with mono property names, type pills (`string`, `integer`, `boolean`, `object`,
  `array<T>`), serif descriptions, constraint annotations, and the schema's
  `examples` rendered as pretty JSON blocks. Collapsible all/expand-all affordance.
- **Responses**: status code + description per documented response (200/204 from the
  operation; 400/415/500/502 with their descriptions), with a link-jump into the
  `ErrorBody` schema inspector.

## 4. Interactivity and micro-behavior

- **Scrollspy**: IntersectionObserver marks the current endpoint in the sidebar.
- **Instant filter**: a sidebar search input filters endpoint sections as you type
  (matching on path, summary, description, tag); `/` focuses it, `Esc` clears.
  Non-matching sections collapse with a subtle animation rather than disappearing.
- **Theme**: toggle in the header sets `data-theme` on `<html>` and persists to
  `localStorage`; with no stored preference, follow `prefers-color-scheme`.
- **Reveal on scroll**: endpoint sections fade/slide in once (IntersectionObserver),
  disabled under `prefers-reduced-motion: reduce`.
- Buttons and rows have visible `:focus-visible` outlines in `--accent-dim`;
  interactive hit areas ≥ 32px.

## 5. Hard constraints

- **One self-contained file**: `assets/api-docs.html` — inline `<style>` and
  `<script>`, no external requests of any kind (no CDNs, no webfonts, no analytics).
  Vanilla JS only; no frameworks, no build step.
- Must include `<html lang="en" data-docs="neuradex">` (integration tests match on
  `data-docs="neuradex"`).
- Same-origin `fetch` only; the page runs inside the service at `/docs`.
- Degrade gracefully without JavaScript: render a static, readable summary of the
  endpoints (hardcoded fallback content listing the four operations) that the JS
  upgrades in place when the spec loads.
- Respect `prefers-reduced-motion`; keyboard navigable throughout.

## 6. Integration contract (shared workspace)

| Artifact | Owner |
| --- | --- |
| `assets/api-docs.html` | front-end agent only |
| `support/design/api-docs-design.md`, `support/design/openapi-fixture.json` | read-only reference |
| Everything else (`src/`, `Cargo.*`, `hack/`, `AGENTS.md`, …) | Rust integration agent only |

Routes after integration: `GET /docs` serves the page; `GET /swagger-ui` and
`GET /swagger-ui/` redirect (302) to `/docs`; `GET /openapi.json` keeps serving the raw
document (now from a handler, since Swagger UI is removed). `utoipa-swagger-ui` is
dropped from `Cargo.toml`.
