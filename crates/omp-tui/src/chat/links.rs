//! Hyperlinks in assistant text (TS `copy-targets.ts` `extractLinks` /
//! `extractLastLink`), shared by `/copy link` and `/open`, plus the system
//! opener (TS `utils/open.ts` `openPath`).

/// `http(s)://` links of `text` in order, deduplicated by href (only
/// http(s) targets qualify, like TS: the result feeds the clipboard and the
/// system opener).
#[must_use]
pub fn http_links(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("http") {
        let tail = &rest[pos..];
        if tail.starts_with("https://") || tail.starts_with("http://") {
            let end = tail
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`' | '<')
                })
                .unwrap_or(tail.len());
            let url = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
            if url.len() > "https://".len() && !out.iter().any(|u| u == url) {
                out.push(url.to_string());
            }
            rest = &tail[end.max(1)..];
        } else {
            rest = &tail[4..];
        }
    }
    out
}

/// Open `url` with the platform opener without waiting for it (TS
/// `openPath`: `open` on macOS, `cmd /c start` on Windows, `xdg-open`
/// elsewhere).
///
/// # Errors
/// The opener could not be spawned.
pub fn open_external(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_http_links_in_order_once() {
        let text = "See [docs](https://example.com/a), https://x.dev/b. Again https://example.com/a and ftp://no.";
        assert_eq!(
            http_links(text),
            ["https://example.com/a", "https://x.dev/b"]
        );
        assert!(http_links("http://").is_empty());
    }
}
