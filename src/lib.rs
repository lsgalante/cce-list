//! Shared between the `cce-list` app and the `cce-list-sync` helper: the
//! item model, the markdown checklist on disk, and the sync-state sidecar.
//!
//! The list stays a plain markdown checklist (`~/.local/share/cce-list/
//! list.md`), readable and editable with anything. Items mirrored from a
//! server carry their identity as a trailing HTML comment —
//! `- [ ] call mom <!-- uid:ABC-123 -->` — which markdown renderers hide and
//! hand-editors can ignore (or delete, which reads as "delete and recreate").
//! Everything else the sync needs (etags, item URLs, the last-synced
//! snapshot) lives in `sync-state.json` next to the list, never in the
//! markdown.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub done: bool,
    /// Server identity for synced items; None for purely local ones.
    pub uid: Option<String>,
}

pub fn data_path() -> PathBuf {
    data_dir().join("list.md")
}

pub fn sync_state_path() -> PathBuf {
    data_dir().join("sync-state.json")
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("cce-list")
}

/// Checklist lines become items; any other non-empty line is adopted as a
/// not-done item rather than parsed around — the next save rewrites the file,
/// so a line this reader skipped would be a line silently deleted.
pub fn parse_items(text: &str) -> Vec<Item> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let (done, rest) = if let Some(r) = trimmed.strip_prefix("- [ ] ") {
                (false, r)
            } else if let Some(r) =
                trimmed.strip_prefix("- [x] ").or_else(|| trimmed.strip_prefix("- [X] "))
            {
                (true, r)
            } else {
                (false, trimmed)
            };
            let (text, uid) = split_uid_comment(rest);
            Some(Item { text: text.to_string(), done, uid })
        })
        .collect()
}

/// Peel a trailing `<!-- uid:… -->` off an item's text, if present.
fn split_uid_comment(rest: &str) -> (&str, Option<String>) {
    let rest = rest.trim_end();
    if let Some(open) = rest.rfind("<!-- uid:") {
        if let Some(inner) = rest[open..].strip_prefix("<!-- uid:").and_then(|s| s.strip_suffix("-->")) {
            let uid = inner.trim();
            if !uid.is_empty() {
                return (rest[..open].trim_end(), Some(uid.to_string()));
            }
        }
    }
    (rest, None)
}

pub fn serialize_items(items: &[Item]) -> String {
    items
        .iter()
        .map(|i| {
            let mark = if i.done { 'x' } else { ' ' };
            match &i.uid {
                Some(uid) => format!("- [{mark}] {} <!-- uid:{uid} -->\n", i.text),
                None => format!("- [{mark}] {}\n", i.text),
            }
        })
        .collect()
}

pub fn load_items() -> Vec<Item> {
    match std::fs::read_to_string(data_path()) {
        Ok(text) => parse_items(&text),
        Err(_) => Vec::new(),
    }
}

/// Write-temp-then-rename in the same directory, so a crash mid-write never
/// leaves a truncated list behind.
pub fn save_items(items: &[Item]) -> std::io::Result<()> {
    atomic_write(&data_path(), &serialize_items(items))
}

pub fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

// ── Sync state (cce-list-sync's merge base; the app never touches it) ─────

/// What the server held for one item at the end of the last sync. Comparing
/// the live list and the live server against this is what tells "the user
/// checked it off here" apart from "it changed on the phone" — and a uid in
/// the state but missing from the list is a local deletion to push, where a
/// uid on the server but not in the state is a new item to pull.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncedItem {
    /// Absolute resource URL (PUT/DELETE target).
    pub url: String,
    pub etag: String,
    /// Which account's credentials the URL answers to.
    pub account: String,
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub items: BTreeMap<String, SyncedItem>,
}

pub fn load_sync_state() -> std::io::Result<SyncState> {
    let text = match std::fs::read_to_string(sync_state_path()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SyncState::default()),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&text).map_err(std::io::Error::other)
}

pub fn save_sync_state(state: &SyncState) -> std::io::Result<()> {
    atomic_write(
        &sync_state_path(),
        &serde_json::to_string_pretty(state).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checklist_round_trips() {
        let items = vec![
            Item { text: "water the plants".into(), done: false, uid: None },
            Item { text: "renew passport".into(), done: true, uid: Some("AB-12".into()) },
        ];
        assert_eq!(parse_items(&serialize_items(&items)), items);
    }

    /// A hand-edited file must survive a load/save cycle: plain lines are
    /// adopted as items, not dropped, and `[X]` reads the same as `[x]`.
    #[test]
    fn foreign_lines_are_adopted_not_dropped() {
        let parsed = parse_items("buy stamps\n- [X] call mom\n\n  - [ ] indented\n");
        assert_eq!(
            parsed,
            vec![
                Item { text: "buy stamps".into(), done: false, uid: None },
                Item { text: "call mom".into(), done: true, uid: None },
                Item { text: "indented".into(), done: false, uid: None },
            ]
        );
    }

    #[test]
    fn uid_comment_is_identity_not_text() {
        let parsed = parse_items("- [ ] call mom <!-- uid:X-1 -->\n- [ ] literal <!-- not a uid -->\n");
        assert_eq!(parsed[0], Item { text: "call mom".into(), done: false, uid: Some("X-1".into()) });
        // A comment that is not `uid:` stays part of the text.
        assert_eq!(parsed[1].uid, None);
        assert_eq!(parsed[1].text, "literal <!-- not a uid -->");
    }
}
