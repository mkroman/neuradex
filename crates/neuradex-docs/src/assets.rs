//! The theme sources: canonical order, embedded copies, and the debug disk loader.
//!
//! The lists below are the single source of truth for the bundling order — templates and
//! scripts may rely on it (the behavior scripts must run after `helpers.js` creates
//! `window.docsUI`). Release builds embed everything and cache each concatenated list;
//! debug builds re-read the files from disk on every call so local edits show up on
//! browser refresh without recompiling, falling back to the embedded copies when a file
//! cannot be read.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// One theme source file: its path under `theme/` and its embedded copy.
struct Source {
    /// The path under the theme directory, e.g. `styles/tokens.css`.
    name: &'static str,
    /// The content embedded at compile time.
    embedded: &'static str,
}

/// Declares a [`Source`] list from theme-relative paths. Each path is both the runtime
/// name and the compile-time embedding path, so it appears once per file.
macro_rules! sources {
    ($($path:literal),* $(,)?) => {
        &[
            $(
                Source {
                    name: $path,
                    embedded: include_str!(concat!("../theme/", $path)),
                },
            )*
        ]
    };
}

/// The stylesheets, in canonical bundling order.
const STYLES: &[Source] = sources![
    "styles/tokens.css",
    "styles/base.css",
    "styles/layout.css",
    "styles/endpoint.css",
    "styles/widgets.css",
    "styles/params.css",
    "styles/tryit.css",
    "styles/responses.css",
    "styles/schema.css",
];

/// The behavior scripts, in canonical bundling order. Each file wires itself; they run
/// inline at the end of `<body>` and communicate through `window.docsUI`, which
/// `helpers.js` creates — it must come first.
const SCRIPTS: &[Source] = sources![
    "scripts/helpers.js",
    "scripts/theme.js",
    "scripts/tryit.js",
    "scripts/schema.js",
    "scripts/filter.js",
    "scripts/spy.js",
];

/// The pre-paint theme bootstrap — runs from `<head>`, never bundled with the other
/// scripts.
const BOOTSTRAP: &[Source] = sources!["bootstrap.js"];

/// Reads every listed source from `base`, falling back to the embedded copy per file on
/// I/O errors. Parameterized over the base directory for tests.
fn read_sources(base: &Path, list: &[Source]) -> Vec<String> {
    list.iter()
        .map(
            |source| match std::fs::read_to_string(base.join(source.name)) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(
                        file = source.name,
                        %error,
                        "theme source unreadable from disk; using the embedded copy"
                    );
                    source.embedded.to_owned()
                }
            },
        )
        .collect()
}

/// Concatenates the sources in canonical order: re-read from disk per call in debug
/// builds, the embedded copies in release.
fn source_text(list: &[Source]) -> String {
    if crate::RELOAD_SOURCES {
        join(&read_sources(theme_dir(), list))
    } else {
        let sources: Vec<&str> = list.iter().map(|source| source.embedded).collect();

        join(&sources)
    }
}

/// Concatenates the stylesheet sources in canonical order.
pub fn style_raw() -> String {
    source_text(STYLES)
}

/// Concatenates the behavior scripts in canonical order: re-read and joined per call in
/// debug builds, borrowed from a process-wide cache in release builds.
pub fn behavior_raw() -> Cow<'static, str> {
    if crate::RELOAD_SOURCES {
        Cow::Owned(source_text(SCRIPTS))
    } else {
        static BUNDLED: LazyLock<String> = LazyLock::new(|| source_text(SCRIPTS));

        Cow::Borrowed(BUNDLED.as_str())
    }
}

/// The pre-paint theme bootstrap: re-read per call in debug builds, borrowed from a
/// process-wide cache in release builds.
pub fn bootstrap_raw() -> Cow<'static, str> {
    if crate::RELOAD_SOURCES {
        Cow::Owned(source_text(BOOTSTRAP))
    } else {
        static BUNDLED: LazyLock<String> = LazyLock::new(|| source_text(BOOTSTRAP));

        Cow::Borrowed(BUNDLED.as_str())
    }
}

/// The theme directory the debug builds re-read from.
fn theme_dir() -> &'static Path {
    static DIR: LazyLock<PathBuf> =
        LazyLock::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("theme"));

    DIR.as_path()
}

/// Joins sources with a separating newline.
fn join<T: AsRef<str>>(sources: &[T]) -> String {
    let Some((first, rest)) = sources.split_first() else {
        return String::new();
    };

    let capacity = sources
        .iter()
        .map(|source| source.as_ref().len())
        .sum::<usize>()
        + rest.len();
    let mut out = String::with_capacity(capacity);
    out.push_str(first.as_ref());

    for source in rest {
        out.push('\n');
        out.push_str(source.as_ref());
    }

    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn source(list: &[Source], name: &str) -> &'static str {
        list.iter()
            .find(|source| source.name == name)
            .map(|source| source.embedded)
            .expect("the source is listed")
    }

    #[test]
    fn reads_files_from_disk_per_call() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir_all(dir.path().join("styles")).expect("styles dir");
        fs::write(
            dir.path().join("styles/tokens.css"),
            ":root { --page-bg: #000; }",
        )
        .expect("fixture write");
        // `base.css` is deliberately missing: the loader must fall back to the embedded
        // copy for it, and to the embedded copy for everything it never saw.
        let read = read_sources(dir.path(), &STYLES[..2]);

        assert_eq!(read[0], ":root { --page-bg: #000; }");
        assert_eq!(read[1], source(STYLES, "styles/base.css"));
    }

    #[test]
    fn reads_edits_between_calls() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir_all(dir.path().join("styles")).expect("styles dir");
        let path = dir.path().join("styles/tokens.css");
        fs::write(&path, "/* first */").expect("fixture write");
        assert_eq!(
            read_sources(dir.path(), &STYLES[..1])[0],
            "/* first */",
            "the first call sees the file"
        );
        fs::write(&path, "/* second */").expect("fixture write");
        assert_eq!(
            read_sources(dir.path(), &STYLES[..1])[0],
            "/* second */",
            "the next call sees the edit — no caching in between"
        );
    }

    #[test]
    fn joins_in_canonical_order() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir_all(dir.path().join("styles")).expect("styles dir");
        for (index, file) in STYLES.iter().enumerate() {
            fs::write(dir.path().join(file.name), format!("/* {index} */")).expect("fixture write");
        }
        let read = read_sources(dir.path(), STYLES);
        assert_eq!(read[0], "/* 0 */");
        assert_eq!(read[8], "/* 8 */");
        let css = join(&read.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(css.find("/* 0 */").expect("first file") < css.find("/* 8 */").expect("last file"));
    }
}
