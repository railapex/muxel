//! Where a link in a rendered Markdown file goes.
//!
//! A Markdown file's links are written relative to the file itself, the way a
//! repository host renders them, so `other.md` and `../src/main.rs#L10` resolve
//! against the file's folder. Only links muxel can follow safely resolve: web
//! pages, mail addresses and local files. Anything else is left alone rather
//! than handed to whatever protocol handler the OS has registered.

use std::path::{Component, Path, PathBuf};

/// A followable link from a rendered Markdown file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkdownLink {
    /// `#heading`: a place in the same document.
    Anchor(String),
    /// A local file, with the link's `#fragment` (such as `L10`) if it had one.
    File {
        path: PathBuf,
        fragment: Option<String>,
    },
    /// A `file://` URL, as written.
    FileUrl(String),
    /// An http(s) page, as written.
    Web(String),
    /// A `mailto:` address, for the system's mail handler.
    Mail(String),
}

/// Resolve `href` from a Markdown file in `folder`. `None` for an empty link, a
/// scheme other than http(s), mailto and file, and a network path, which
/// Windows would reach over SMB.
pub fn resolve(folder: &Path, href: &str) -> Option<MarkdownLink> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    if let Some(anchor) = href.strip_prefix('#') {
        return Some(MarkdownLink::Anchor(anchor.to_string()));
    }
    if let Some(scheme) = scheme(href) {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => Some(MarkdownLink::Web(href.to_string())),
            "mailto" => Some(MarkdownLink::Mail(href.to_string())),
            "file" => Some(MarkdownLink::FileUrl(href.to_string())),
            _ => None,
        };
    }
    let (rest, fragment) = match href.split_once('#') {
        Some((rest, fragment)) => (rest, (!fragment.is_empty()).then(|| fragment.to_string())),
        None => (href, None),
    };
    // A query means nothing to a local file (`image.png?raw=true`).
    let rest = rest.split_once('?').map_or(rest, |(rest, _)| rest);
    // Separators: authors on Windows write `..\src\main.rs`.
    let decoded = percent_decode(rest)?.replace('\\', "/");
    if decoded.is_empty() || decoded.starts_with("//") {
        return None;
    }
    let written = Path::new(&decoded);
    let path = if written.is_absolute() {
        written.to_path_buf()
    } else {
        folder.join(written)
    };
    Some(MarkdownLink::File {
        path: normalize(&path),
        fragment,
    })
}

/// `href`'s scheme, if it starts with one. A single letter before the colon is
/// a Windows drive (`C:/notes.md`), not a scheme.
fn scheme(href: &str) -> Option<&str> {
    let (scheme, _) = href.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (scheme.len() > 1
        && first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(scheme)
}

/// Decode `%XX` escapes. A `%` without two hex digits after it stays as
/// written. `None` when the bytes aren't UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = if bytes[i] == b'%' {
            // `from_str_radix` would also take a sign (`%+F`).
            bytes
                .get(i + 1..i + 3)
                .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        } else {
            None
        };
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `path` with `.` dropped and each `..` taking off the folder before it,
/// without touching the disk. A `..` at the root stays at the root. A Windows
/// verbatim drive (`\\?\C:`, which a canonical path carries) becomes a plain
/// one, so the path reads as a local file again in a `file://` URL.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                std::path::Prefix::VerbatimDisk(drive) => out.push(format!("{}:", drive as char)),
                _ => out.push(component),
            },
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_)) | None
                ) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{MarkdownLink, resolve};
    use std::path::PathBuf;

    /// The folder of the Markdown file the links are in: `<root>/repo/docs`.
    fn docs() -> PathBuf {
        root().join("repo").join("docs")
    }

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" })
    }

    fn file(path: PathBuf, fragment: Option<&str>) -> Option<MarkdownLink> {
        Some(MarkdownLink::File {
            path,
            fragment: fragment.map(String::from),
        })
    }

    #[test]
    fn relative_links_resolve_against_the_files_folder() {
        let docs = docs();
        let repo = root().join("repo");
        assert_eq!(
            resolve(&docs, "other.md"),
            file(docs.join("other.md"), None)
        );
        assert_eq!(
            resolve(&docs, "./guide/setup.md"),
            file(docs.join("guide").join("setup.md"), None)
        );
        assert_eq!(
            resolve(&docs, "../src/main.rs"),
            file(repo.join("src").join("main.rs"), None)
        );
        assert_eq!(
            resolve(&docs, "guide/../../README.md"),
            file(repo.join("README.md"), None)
        );
    }

    #[test]
    fn a_fragment_rides_along_and_a_query_drops() {
        let docs = docs();
        let main = root().join("repo").join("src").join("main.rs");
        assert_eq!(
            resolve(&docs, "../src/main.rs#L10"),
            file(main, Some("L10"))
        );
        assert_eq!(
            resolve(&docs, "other.md#install"),
            file(docs.join("other.md"), Some("install"))
        );
        assert_eq!(
            resolve(&docs, "shot.png?raw=true"),
            file(docs.join("shot.png"), None)
        );
        assert_eq!(
            resolve(&docs, "other.md#"),
            file(docs.join("other.md"), None)
        );
    }

    #[test]
    fn an_anchor_alone_is_a_place_in_this_document() {
        assert_eq!(
            resolve(&docs(), "#getting-started"),
            Some(MarkdownLink::Anchor("getting-started".into()))
        );
    }

    #[test]
    fn escapes_and_windows_separators_are_read_as_written() {
        let docs = docs();
        assert_eq!(
            resolve(&docs, "my%20notes.md"),
            file(docs.join("my notes.md"), None)
        );
        assert_eq!(
            resolve(&docs, "caf%C3%A9.md"),
            file(docs.join("café.md"), None)
        );
        assert_eq!(resolve(&docs, "100%.md"), file(docs.join("100%.md"), None));
        assert_eq!(
            resolve(&docs, r"..\src\lib.rs#L3"),
            file(root().join("repo").join("src").join("lib.rs"), Some("L3"))
        );
    }

    /// A file opened through a link has a canonical path, which on Windows
    /// starts `\\?\`.
    #[cfg(windows)]
    #[test]
    fn a_canonical_windows_folder_resolves_to_a_plain_path() {
        let folder = PathBuf::from(r"\\?\C:\repo\docs");
        assert_eq!(
            resolve(&folder, "../src/main.rs"),
            file(PathBuf::from(r"C:\repo\src\main.rs"), None)
        );
    }

    #[test]
    fn an_absolute_path_is_used_as_given() {
        let (written, expected) = if cfg!(windows) {
            ("D:/notes/todo.md", PathBuf::from(r"D:\notes\todo.md"))
        } else {
            ("/notes/todo.md", PathBuf::from("/notes/todo.md"))
        };
        assert_eq!(resolve(&docs(), written), file(expected, None));
    }

    #[test]
    fn web_mail_and_file_urls_pass_through() {
        let docs = docs();
        let url = "https://example.com/a?b=1#c";
        assert_eq!(resolve(&docs, url), Some(MarkdownLink::Web(url.into())));
        assert_eq!(
            resolve(&docs, "HTTP://example.com"),
            Some(MarkdownLink::Web("HTTP://example.com".into()))
        );
        assert_eq!(
            resolve(&docs, "mailto:someone@example.com"),
            Some(MarkdownLink::Mail("mailto:someone@example.com".into()))
        );
        assert_eq!(
            resolve(&docs, "file:///tmp/x.md"),
            Some(MarkdownLink::FileUrl("file:///tmp/x.md".into()))
        );
    }

    #[test]
    fn links_muxel_wont_follow_resolve_to_nothing() {
        let docs = docs();
        for href in [
            "",
            "   ",
            "javascript:alert(1)",
            "ms-settings:privacy",
            "vscode://file/x",
            "data:text/html,hi",
            "//host/share/x.md",
            r"\\host\share\x.md",
            "%5C%5Chost%5Cshare%5Cx.md",
            "%2F%2Fhost/share",
            "%FF.md",
        ] {
            assert_eq!(resolve(&docs, href), None, "{href:?}");
        }
    }
}
