# Rust style and module layout

This page covers the conventions that `cargo fmt` and Clippy do not enforce. The workspace uses
Rust 2024 and the modern module layout introduced by the Rust 2018 edition.

## Module layout

Use the `foo.rs` + `foo/bar.rs` layout. A directory is not a module root by itself; the sibling
file is.

```text
crates/neuradex-audit/src/
├── lib.rs
├── event.rs
├── event/
│   ├── header_policy.rs
│   └── request_record.rs
└── sink.rs
```

`lib.rs` declares `mod event;` and `mod sink;`. `event.rs` declares
`mod header_policy;` and `mod request_record;`. There is no `event/mod.rs`.

| Do | Do not |
| --- | --- |
| `src/event.rs` + `src/event/header.rs` | `src/event/mod.rs` + `src/event/header.rs` |
| `mod event;` in `lib.rs` | `#[path = "event/mod.rs"] mod event;` |
| `mod header;` in `event.rs` | A directory-only module with no `event.rs` |

Reasons:

- Rust 2018 lifted the rule that a module with submodules had to live in `mod.rs`. The Edition
  Guide's [Path and module system changes](https://doc.rust-lang.org/edition-guide/rust-2018/path-changes.html)
  documents the `foo.rs` + `foo/bar.rs` layout directly.
- The file name tells you which module it defines. A tree full of `mod.rs` tabs is hard to
  navigate and review.
- The [Rust Style Guide](https://doc.rust-lang.org/nightly/style-guide/advice.html) says to avoid
  `#[path]` annotations where possible. Standard module lookup keeps the map from module path to
  file path obvious to readers, rust-analyzer, and tooling.
- `#[path]` makes module resolution depend on an attribute rather than the file tree. Do not use
  it for domain code. If generated code makes it unavoidable, isolate it at the generated
  boundary and explain why in a comment.

`lib.rs` and `main.rs` are the crate roots and are exempt from the file-as-module rule. A module
named `foo` is `foo.rs`; it does not need a `foo/` directory unless it has submodules.

## Naming

The [Rust API Guidelines naming rules](https://rust-lang.github.io/api-guidelines/naming.html)
apply:

| Item | Convention | Example |
| --- | --- | --- |
| Modules | `snake_case` | `packfile`, `header_policy` |
| Crates | `snake_case` (Cargo normalizes `-`/`_`) | `neuradex-audit` |
| Types / traits | `UpperCamelCase` | `AuditEvent`, `ContentReference` |
| Enum variants | `UpperCamelCase` | `Outcome::ClientError` |
| Functions / methods | `snake_case` | `hash_body`, `read_entry` |
| Local variables | `snake_case` | `frame_offset` |
| Constants / statics | `SCREAMING_SNAKE_CASE` | `FRAME_HEADER_LEN` |
| Macros | `snake_case!` | `assert_eq!` |
| Type parameters | `UpperCamelCase` | `T`, `W`, `S` |

Module names are concepts, not file paths: `header_policy`, not `headerPolicy` or
`header-policy`. The module name must match the file stem (`header_policy.rs`).

## Paths

- Use `crate::` for the current crate, `super::` for the parent module, and `self::` for the
  current module. Do not rely on 2015-style implicit crate-root paths.
- External crates are in scope without `extern crate`; add them to `Cargo.toml` and `use` them.
- Prefer importing the type/function over glob imports. Glob imports are acceptable in tests and
  preludes.
- Keep `use` statements before module declarations and version-sort them (Style Guide, Items).
- Do not add `#[path]` to work around a layout; move the file instead.

## Visibility

- Default to private. Use `pub(crate)` for crate-internal APIs and `pub` only for the crate's
  intended surface. These crates are `publish = false`, but the same discipline keeps the public
  docs meaningful.
- A module's public API is defined by its `pub` items, not by its file name.
- `missing_docs` is warn workspace-wide; every public item needs a doc comment, including
  `# Errors` for fallible functions.

## Documentation examples

When a doc or RFD shows a crate tree, use the modern layout:

```text
crates/neuradex-packfile/src/
├── lib.rs
├── frame.rs
├── footer.rs
├── trailer.rs
├── writer.rs
└── reader.rs
```

Do not show a `foo/mod.rs` tree. If a historical layout must be described, label it explicitly as
the old 2015 layout and keep it out of the recommended example.

## Enforcement

- `cargo fmt --check` enforces formatting, not module layout.
- Clippy does not flag a `mod.rs` layout or `#[path]`.
- Reviewers check this convention. A PR that adds `foo/mod.rs` or `#[path]` should be changed
  before merge.
- New modules should also update the crate-level docs (`//!`) and, if the module owns a new
  concept, the relevant RFD/reference page.

## References

- [The Rust Edition Guide — Path and module system changes (Rust 2018)](https://doc.rust-lang.org/edition-guide/rust-2018/path-changes.html)
- [The Rust Style Guide — Other style advice (Modules: avoid `#[path]`)](https://doc.rust-lang.org/nightly/style-guide/advice.html)
- [Rust API Guidelines — Naming](https://rust-lang.github.io/api-guidelines/naming.html)
- [The Rust Reference — Modules](https://doc.rust-lang.org/reference/items/modules.html)
- [RFD 0006: Crate boundaries](../rfd/0006-crate-boundaries.md)
