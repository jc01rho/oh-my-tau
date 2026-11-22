//! Pinned-session store (TS `session/session-pins.ts`).
//!
//! `~/.omp-rs/session-pins.json` holds the pinned session ids as a JSON array,
//! keyed by session id (not file path) so pins survive moves. Shared by the
//! `/pin` / `/unpin` commands and the resume picker's `app.session.pin` /
//! `app.session.unpin` keys, so both edit the same set.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// `~/.omp-rs/session-pins.json`, the omp-rs counterpart of TS
/// `~/.omp/session-pins.json`. `None` without a `HOME`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".omp-rs")
            .join("session-pins.json"),
    )
}

/// The pinned ids stored at `path` (TS `loadPinnedSessionIds`): a missing,
/// unreadable or corrupt file is an empty set, and non-string members are
/// dropped, so a broken file never breaks the resume picker.
#[must_use]
pub fn load(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<serde_json::Value>>(&text).ok())
        .map(|values| {
            values
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Persist `pins` at `path` as a sorted JSON array, creating the parent dir.
/// Written to a temp file first and renamed over the target, so a crash
/// mid-write leaves the previous pins intact (TS writes temp-then-replace).
///
/// # Errors
/// Any filesystem error creating the dir, writing or renaming the file.
pub fn store(path: &Path, pins: &HashSet<String>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut sorted: Vec<&String> = pins.iter().collect();
    sorted.sort();
    let text = serde_json::to_string_pretty(&sorted).map_err(io::Error::other)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Pin or unpin `id` in the store at `path` (TS `setSessionPinned`); a no-op
/// write when it is already in that state. Returns whether it was pinned
/// before the update.
///
/// # Errors
/// Any filesystem error from [`store`].
pub fn set_pinned(path: &Path, id: &str, pinned: bool) -> io::Result<bool> {
    let mut pins = load(path);
    let was = pins.contains(id);
    if was == pinned {
        return Ok(was);
    }
    if pinned {
        pins.insert(id.to_owned());
    } else {
        pins.remove(id);
    }
    store(path, &pins)?;
    Ok(was)
}

/// Flip `id`'s pin in the store at `path` (TS `toggleSessionPin`); returns the
/// new pinned state.
///
/// # Errors
/// Any filesystem error from [`store`].
pub fn toggle(path: &Path, id: &str) -> io::Result<bool> {
    let now = !load(path).contains(id);
    set_pinned(path, id, now)?;
    Ok(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omp-session-pins-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("session-pins.json")
    }

    #[test]
    fn set_toggle_and_reload_round_trip() {
        let path = temp_store("round-trip");
        assert!(load(&path).is_empty(), "missing file is no pins");
        assert!(!set_pinned(&path, "b", true).unwrap());
        assert!(!set_pinned(&path, "a", true).unwrap());
        assert!(set_pinned(&path, "a", true).unwrap(), "already pinned");
        assert_eq!(load(&path), HashSet::from(["a".to_owned(), "b".to_owned()]));
        let stored: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored, vec!["a", "b"], "stored sorted");
        assert!(!toggle(&path, "a").unwrap(), "toggle unpins");
        assert!(toggle(&path, "c").unwrap(), "toggle pins");
        assert!(set_pinned(&path, "b", false).unwrap());
        assert_eq!(load(&path), HashSet::from(["c".to_owned()]));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_or_mixed_file_degrades() {
        let path = temp_store("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_empty());
        std::fs::write(&path, r#"["ok", 3, null]"#).unwrap();
        assert_eq!(load(&path), HashSet::from(["ok".to_owned()]));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).ok();
    }
}
