//! The theme sources: canonical order, embedded copies, and the debug disk loader.
//!
//! The lists below are the single source of truth for the bundling order — templates and
//! scripts may rely on it (the behavior scripts must run after `helpers.js` creates
//! `window.docsUI`). Release builds embed everything; debug builds re-read the files from
//! disk on every call so local edits show up on browser refresh without recompiling,
//! falling back to the embedded copies when a file cannot be read.

#[cfg(debug_assertions)]
use std::path::Path;
use std::sync::OnceLock;

/// One theme source file: its path under `theme/` and its embedded copy.
struct Source {
    /// The path under the theme directory, e.g. `styles/tokens.css`. Only the debug disk
    /// loader reads it.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    name: &'static str,
    /// The content embedded at compile time.
    embedded: &'static str,
}

/// The stylesheets, in canonical bundling order.
const STYLES: &[Source] = &[
    Source {
        name: "styles/tokens.css",
        embedded: include_str!("../theme/styles/tokens.css"),
    },
    Source {
        name: "styles/base.css",
        embedded: include_str!("../theme/styles/base.css"),
    },
    Source {
        name: "styles/layout.css",
        embedded: include_str!("../theme/styles/layout.css"),
    },
    Source {
        name: "styles/endpoint.css",
        embedded: include_str!("../theme/styles/endpoint.css"),
    },
    Source {
        name: "styles/widgets.css",
        embedded: include_str!("../theme/styles/widgets.css"),
    },
    Source {
        name: "styles/params.css",
        embedded: include_str!("../theme/styles/params.css"),
    },
    Source {
        name: "styles/tryit.css",
        embedded: include_str!("../theme/styles/tryit.css"),
    },
    Source {
        name: "styles/responses.css",
        embedded: include_str!("../theme/styles/responses.css"),
    },
    Source {
        name: "styles/schema.css",
        embedded: include_str!("../theme/styles/schema.css"),
    },
];

/// The behavior scripts, in canonical bundling order. Each file wires itself; they run
/// inline at the end of `<body>` and communicate through `window.docsUI`, which
/// `helpers.js` creates — it must come first.
const SCRIPTS: &[Source] = &[
    Source {
        name: "scripts/helpers.js",
        embedded: include_str!("../theme/scripts/helpers.js"),
    },
    Source {
        name: "scripts/theme.js",
        embedded: include_str!("../theme/scripts/theme.js"),
    },
    Source {
        name: "scripts/tryit.js",
        embedded: include_str!("../theme/scripts/tryit.js"),
    },
    Source {
        name: "scripts/schema.js",
        embedded: include_str!("../theme/scripts/schema.js"),
    },
    Source {
        name: "scripts/filter.js",
        embedded: include_str!("../theme/scripts/filter.js"),
    },
    Source {
        name: "scripts/spy.js",
        embedded: include_str!("../theme/scripts/spy.js"),
    },
];

/// The pre-paint theme bootstrap — runs from `<head>`, never bundled with the other
/// scripts.
const BOOTSTRAP: &str = include_str!("../theme/bootstrap.js");

/// Reads every listed source from `base`, falling back to the embedded copy per file on
/// I/O errors. Parameterized over the base directory for tests.
#[cfg(debug_assertions)]
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

/// The embedded copies of every listed source, in order.
#[cfg(not(debug_assertions))]
fn embedded_sources(list: &[Source]) -> Vec<&'static str> {
    list.iter().map(|source| source.embedded).collect()
}

/// Concatenates the stylesheet sources in canonical order: re-read from disk per call in
/// debug builds, the embedded copies in release.
pub fn style_raw() -> String {
    #[cfg(debug_assertions)]
    let sources = read_sources(theme_dir(), STYLES);
    #[cfg(not(debug_assertions))]
    let sources = embedded_sources(STYLES);
    join(&sources)
}

/// Concatenates the behavior scripts in canonical order: re-read from disk per call in
/// debug builds, the embedded copies in release.
pub fn behavior_raw() -> String {
    #[cfg(debug_assertions)]
    let sources = read_sources(theme_dir(), SCRIPTS);
    #[cfg(not(debug_assertions))]
    let sources = embedded_sources(SCRIPTS);
    join(&sources)
}

/// The pre-paint theme bootstrap: re-read from disk per call in debug builds, the
/// embedded copy in release.
pub fn bootstrap_raw() -> String {
    #[cfg(debug_assertions)]
    {
        let base = theme_dir();
        match std::fs::read_to_string(base.join("bootstrap.js")) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(
                    file = "bootstrap.js",
                    %error,
                    "theme source unreadable from disk; using the embedded copy"
                );
                BOOTSTRAP.to_owned()
            }
        }
    }
    #[cfg(not(debug_assertions))]
    {
        BOOTSTRAP.to_owned()
    }
}

/// The stylesheet sources as one string, cached at first call — for the public accessor,
/// whose `&'static str` signature cannot honor per-call disk reads.
pub fn style() -> &'static str {
    static STYLE: OnceLock<String> = OnceLock::new();
    STYLE.get_or_init(style_raw)
}

/// The behavior scripts as one string, cached at first call — for the public accessor,
/// whose `&'static str` signature cannot honor per-call disk reads.
pub fn behavior() -> &'static str {
    static BEHAVIOR: OnceLock<String> = OnceLock::new();
    BEHAVIOR.get_or_init(behavior_raw)
}

/// The pre-paint theme bootstrap, cached at first call — for the public accessor, whose
/// `&'static str` signature cannot honor per-call disk reads.
pub fn bootstrap() -> &'static str {
    static BOOTSTRAP_TEXT: OnceLock<String> = OnceLock::new();
    BOOTSTRAP_TEXT.get_or_init(bootstrap_raw)
}

/// The theme directory the debug builds re-read from.
#[cfg(debug_assertions)]
fn theme_dir() -> &'static Path {
    static DIR: OnceLock<&'static Path> = OnceLock::new();
    DIR.get_or_init(|| {
        Box::leak(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("theme")
                .into_boxed_path(),
        )
    })
}

/// Joins sources with a separating newline.
fn join<T: AsRef<str>>(sources: &[T]) -> String {
    sources
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[cfg(debug_assertions)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// A fresh throwaway directory per test, so fixtures never collide.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "neuradex-docs-test-{}-{tag}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("the fixture directory is created");
        dir
    }

    fn source(list: &[Source], name: &str) -> &'static str {
        list.iter()
            .find(|source| source.name == name)
            .map(|source| source.embedded)
            .expect("the source is listed")
    }

    #[test]
    fn reads_files_from_disk_per_call() {
        let dir = temp_dir("disk");
        fs::create_dir_all(dir.join("styles")).expect("styles dir");
        fs::write(dir.join("styles/tokens.css"), ":root { --page-bg: #000; }")
            .expect("fixture write");
        // `base.css` is deliberately missing: the loader must fall back to the embedded
        // copy for it, and to the embedded copy for everything it never saw.
        let read = read_sources(&dir, &STYLES[..2]);

        assert_eq!(read[0], ":root { --page-bg: #000; }");
        assert_eq!(read[1], source(STYLES, "styles/base.css"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reads_edits_between_calls() {
        let dir = temp_dir("fresh");
        fs::create_dir_all(dir.join("styles")).expect("styles dir");
        let path = dir.join("styles/tokens.css");
        fs::write(&path, "/* first */").expect("fixture write");
        assert_eq!(
            read_sources(&dir, &STYLES[..1])[0],
            "/* first */",
            "the first call sees the file"
        );
        fs::write(&path, "/* second */").expect("fixture write");
        assert_eq!(
            read_sources(&dir, &STYLES[..1])[0],
            "/* second */",
            "the next call sees the edit — no caching in between"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn joins_in_canonical_order() {
        let dir = temp_dir("order");
        fs::create_dir_all(dir.join("styles")).expect("styles dir");
        for (index, file) in STYLES.iter().enumerate() {
            fs::write(dir.join(file.name), format!("/* {index} */")).expect("fixture write");
        }
        let read = read_sources(&dir, STYLES);
        assert_eq!(read[0], "/* 0 */");
        assert_eq!(read[8], "/* 8 */");
        let css = join(&read.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(css.find("/* 0 */").expect("first file") < css.find("/* 8 */").expect("last file"));
        fs::remove_dir_all(dir).ok();
    }
}
