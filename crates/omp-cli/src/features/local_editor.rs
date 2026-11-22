//! `/local [name]` and the `app.local.open` key: resolve a session `local://`
//! file for the external editor (TS `modes/local-file-editor.ts`). The TUI
//! owns the picker and the editor launch; this module lists the files and
//! validates targets (realpath + root containment, so traversal, absolute
//! paths, directories and escaping symlinks are rejected).

use std::path::PathBuf;

use omp_urls::schemes::local::{list_local_files, locate_local};
use omp_urls::UrlCtx;

/// TS `encodeLocalPath`: encode a filesystem name as `local://` path
/// segments (`/` stays a separator, everything `encodeURIComponent` escapes
/// is percent-encoded once).
pub fn encode_local_path(name: &str) -> String {
    name.split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_uri_component(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// TS `locateAndValidateTarget`: `(display url, real path)` of a regular file.
fn locate_and_validate(
    url: &str,
    display: &str,
    ctx: &UrlCtx,
) -> Result<(String, PathBuf), String> {
    let located = locate_local(url, ctx)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Local file not found: {display}"))?;
    let meta = std::fs::metadata(&located).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err(format!("{display} is a directory; name a file inside it."));
    }
    if !meta.is_file() {
        return Err(format!("{display} is not a regular file."));
    }
    Ok((display.to_owned(), located))
}

/// TS `resolveLocalEditorTarget`: a typed name (encoded like any filename) or
/// an explicit `local://` / `local:/` URL (passed through).
pub fn resolve_target(target: &str, ctx: &UrlCtx) -> Result<(String, PathBuf), String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("Enter a local:// file name.".into());
    }
    let explicit = trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("local:/"));
    let (url, display) = if explicit {
        (trimmed.to_owned(), trimmed.to_owned())
    } else {
        (
            format!("local://{}", encode_local_path(trimmed)),
            format!("local://{trimmed}"),
        )
    };
    locate_and_validate(&url, &display, ctx)
}

/// TS `revalidateLocalEditorEntry`: re-resolve a picked entry's exact name
/// (the listing is stale once the picker returns).
pub fn revalidate_entry(name: &str, ctx: &UrlCtx) -> Result<(String, PathBuf), String> {
    let url = format!("local://{}", encode_local_path(name));
    locate_and_validate(&url, &format!("local://{name}"), ctx)
}

/// The session's `local://` files for the picker, newest first.
pub fn list(ctx: &UrlCtx) -> Result<Vec<omp_tui::port::LocalFile>, String> {
    list_local_files(ctx)
        .map(|files| {
            files
                .into_iter()
                .map(|f| omp_tui::port::LocalFile {
                    name: f.name,
                    size: f.size,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_root(tag: &str) -> (UrlCtx, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("omp-local-editor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("sess").join("local");
        std::fs::create_dir_all(&root).unwrap();
        let ctx = UrlCtx {
            session_file: Some(base.join("sess.jsonl")),
            ..UrlCtx::default()
        };
        (ctx, root)
    }

    #[test]
    fn encodes_like_encode_uri_component_per_segment() {
        assert_eq!(encode_local_path("a b/100%.md"), "a%20b/100%25.md");
        assert_eq!(encode_local_path("q?#x"), "q%3F%23x");
        assert_eq!(encode_local_path("ok-_.!~*'()"), "ok-_.!~*'()");
    }

    #[test]
    fn resolves_names_and_explicit_urls_and_rejects_bad_targets() {
        let (ctx, root) = ctx_with_root("resolve");
        std::fs::write(root.join("100%.md"), "x").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let (display, path) = resolve_target(" 100%.md ", &ctx).unwrap();
        assert_eq!(display, "local://100%.md");
        assert_eq!(path, std::fs::canonicalize(root.join("100%.md")).unwrap());
        let (display, _) = resolve_target("local://100%25.md", &ctx).unwrap();
        assert_eq!(display, "local://100%25.md");
        assert_eq!(
            resolve_target("  ", &ctx).unwrap_err(),
            "Enter a local:// file name."
        );
        assert_eq!(
            resolve_target("nope.md", &ctx).unwrap_err(),
            "Local file not found: local://nope.md"
        );
        assert_eq!(
            resolve_target("sub", &ctx).unwrap_err(),
            "local://sub is a directory; name a file inside it."
        );
        assert!(resolve_target("../escape.md", &ctx).is_err());
        let (display, _) = revalidate_entry("100%.md", &ctx).unwrap();
        assert_eq!(display, "local://100%.md");
        let _ = std::fs::remove_dir_all(root.parent().unwrap().parent().unwrap());
    }
}
