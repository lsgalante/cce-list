//! Shared between the `cce-list` app and the `cce-list-sync` helper: the
//! item model, the markdown checklists on disk, and the sync-state sidecar.
//!
//! Lists are plain markdown checklists, one file per list under
//! `~/.local/share/cce-list/lists/<title>.md`, readable and editable with
//! anything. The file's stem is the list's title. A list mirrored from a
//! server carries its identity as a first-line HTML comment —
//! `<!-- list:MDM5… -->` — and each mirrored item as a trailing one —
//! `- [ ] call mom <!-- uid:ABC-123 -->`; markdown renderers hide both and
//! hand-editors can ignore them (deleting one reads as "delete and
//! recreate"). Which list the app shows is a one-line `current` file next
//! to `lists/`. Everything else the sync needs (etags, item URLs, the
//! last-synced snapshot) lives in `sync-state.json`, never in the markdown.
//!
//! Before lists existed there was a single `list.md`; `load_lists` migrates
//! it on first sight (see [`migrate_legacy`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub text: String,
    pub done: bool,
    /// Server identity for synced items; None for purely local ones.
    pub uid: Option<String>,
}

/// One checklist: its file stem, its server identity (if mirrored), items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFile {
    pub title: String,
    pub id: Option<String>,
    pub items: Vec<Item>,
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("cce-list")
}

pub fn lists_dir() -> PathBuf {
    data_dir().join("lists")
}

/// The pre-lists single checklist; only read by the migration.
pub fn legacy_path() -> PathBuf {
    data_dir().join("list.md")
}

pub fn current_path() -> PathBuf {
    data_dir().join("current")
}

pub fn sync_state_path() -> PathBuf {
    data_dir().join("sync-state.json")
}

/// A title as a file stem. `/` is the one character a stem cannot hold; a
/// server title carrying one comes back to the server renamed, which is the
/// lesser evil next to a list that cannot be written at all.
pub fn safe_title(title: &str) -> String {
    let t: String = title.trim().replace('/', "-");
    if t.is_empty() || t == "." || t == ".." { "Untitled".to_string() } else { t }
}

pub fn list_path(title: &str) -> PathBuf {
    lists_dir().join(format!("{}.md", safe_title(title)))
}

// ── Markdown ──────────────────────────────────────────────────────────────

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

/// A whole list file: the optional `<!-- list:ID -->` header, then items.
pub fn parse_list(text: &str) -> (Option<String>, Vec<Item>) {
    let mut lines = text.lines();
    let mut first = lines.next();
    while matches!(first, Some(l) if l.trim().is_empty()) {
        first = lines.next();
    }
    if let Some(id) = first
        .map(str::trim)
        .and_then(|l| l.strip_prefix("<!-- list:"))
        .and_then(|l| l.strip_suffix("-->"))
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let rest: Vec<&str> = lines.collect();
        return (Some(id.to_string()), parse_items(&rest.join("\n")));
    }
    (None, parse_items(text))
}

pub fn serialize_list(id: Option<&str>, items: &[Item]) -> String {
    let mut out = String::new();
    if let Some(id) = id {
        out.push_str(&format!("<!-- list:{id} -->\n"));
    }
    out.push_str(&serialize_items(items));
    out
}

// ── Files ─────────────────────────────────────────────────────────────────

/// Every list on disk, titles sorted case-insensitively. Runs the legacy
/// migration first, so a pre-lists install comes up with its old checklist
/// intact rather than empty.
pub fn load_lists() -> std::io::Result<Vec<ListFile>> {
    migrate_legacy()?;
    let dir = lists_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(title) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let text = std::fs::read_to_string(&path)?;
        let (id, items) = parse_list(&text);
        out.push(ListFile { title, id, items });
    }
    out.sort_by_key(|l| l.title.to_lowercase());
    Ok(out)
}

pub fn load_list(title: &str) -> std::io::Result<ListFile> {
    let text = std::fs::read_to_string(list_path(title))?;
    let (id, items) = parse_list(&text);
    Ok(ListFile { title: safe_title(title), id, items })
}

/// Write-temp-then-rename in the same directory, so a crash mid-write never
/// leaves a truncated list behind.
pub fn save_list(list: &ListFile) -> std::io::Result<()> {
    atomic_write(&list_path(&list.title), &serialize_list(list.id.as_deref(), &list.items))
}

pub fn delete_list(title: &str) -> std::io::Result<()> {
    match std::fs::remove_file(list_path(title)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

pub fn rename_list(old: &str, new: &str) -> std::io::Result<()> {
    let (from, to) = (list_path(old), list_path(new));
    if from == to {
        return Ok(());
    }
    std::fs::rename(from, to)
}

pub fn load_current() -> Option<String> {
    std::fs::read_to_string(current_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_current(title: &str) -> std::io::Result<()> {
    atomic_write(&current_path(), &format!("{}\n", safe_title(title)))
}

pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// `list.md` → `lists/…`, once. The single checklist used to mirror EVERY
/// server list flat, so its rows are split by the list the sync state says
/// each belongs to: the biggest group becomes `Tasks.md`, any other group a
/// file named by its list id — both carrying that id in the header, so the
/// next sync recognises them as those lists and renames the files to the
/// server's titles instead of creating new lists on the phone. Rows the
/// state does not know (typed locally, never synced) go with the biggest
/// group. The old file is kept as `list.md.migrated`.
pub fn migrate_legacy() -> std::io::Result<()> {
    let legacy = legacy_path();
    if lists_dir().exists() || !legacy.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&legacy)?;
    let items = parse_items(&text);
    let state = load_sync_state().unwrap_or_default();
    let majority = state.majority_list_id();
    let mut groups: BTreeMap<Option<String>, Vec<Item>> = BTreeMap::new();
    for item in items {
        let owner = item
            .uid
            .as_deref()
            .and_then(|u| state.items.get(u))
            .map(|s| s.list_id())
            .filter(|id| !id.is_empty())
            .or_else(|| majority.clone());
        groups.entry(owner).or_default().push(item);
    }
    if groups.is_empty() {
        groups.insert(majority.clone(), Vec::new());
    }
    for (id, items) in groups {
        let title = match (&id, &majority) {
            (Some(i), Some(m)) if i != m => safe_title(i),
            _ => "Tasks".to_string(),
        };
        save_list(&ListFile { title, id, items })?;
    }
    save_current("Tasks")?;
    std::fs::rename(&legacy, legacy.with_extension("md.migrated"))
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
    /// The server list the item belongs to. Older state files lack it; see
    /// [`SyncedItem::list_id`], which falls back to reading the URL.
    #[serde(default)]
    pub list: String,
}

impl SyncedItem {
    /// Google task URLs are `…/lists/{id}/tasks/{task}`; CalDAV item URLs
    /// are `<calendar>/<uid>.ics`, where the calendar URL is the list id.
    pub fn list_id(&self) -> String {
        if !self.list.is_empty() {
            return self.list.clone();
        }
        list_id_from_url(&self.url)
    }
}

pub fn list_id_from_url(url: &str) -> String {
    if let Some(rest) = url.split("/lists/").nth(1) {
        if let Some(id) = rest.split("/tasks").next() {
            return id.to_string();
        }
    }
    match url.rfind('/') {
        Some(i) => url[..=i].to_string(),
        None => String::new(),
    }
}

/// A server list as last synced: its title then, so a rename on either
/// side is told apart from the other.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncedList {
    pub title: String,
    #[serde(default)]
    pub etag: String,
    pub account: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub items: BTreeMap<String, SyncedItem>,
    /// Keyed by server list id.
    #[serde(default)]
    pub lists: BTreeMap<String, SyncedList>,
}

impl SyncState {
    /// The list most tracked items belong to — what the legacy single
    /// checklist "was", for the migration.
    pub fn majority_list_id(&self) -> Option<String> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for item in self.items.values() {
            let id = item.list_id();
            if !id.is_empty() {
                *counts.entry(id).or_default() += 1;
            }
        }
        counts.into_iter().max_by_key(|(_, n)| *n).map(|(id, _)| id)
    }
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

    #[test]
    fn list_header_round_trips_and_is_optional() {
        let items = vec![Item { text: "a".into(), done: false, uid: None }];
        let text = serialize_list(Some("MDM5"), &items);
        assert_eq!(parse_list(&text), (Some("MDM5".into()), items.clone()));
        // No header: a hand-made file is a local-only list, first line and all.
        assert_eq!(parse_list("- [ ] a\n"), (None, items));
        // A header that is not `list:` is just an adopted line.
        let (id, adopted) = parse_list("<!-- note -->\n- [ ] a\n");
        assert_eq!(id, None);
        assert_eq!(adopted.len(), 2);
    }

    #[test]
    fn list_ids_come_from_urls_when_the_state_predates_them() {
        assert_eq!(
            list_id_from_url("https://tasks.googleapis.com/tasks/v1/lists/MDM5/tasks/abc"),
            "MDM5"
        );
        assert_eq!(
            list_id_from_url("https://p1-caldav.icloud.com/1/calendars/reminders/X.ics"),
            "https://p1-caldav.icloud.com/1/calendars/reminders/"
        );
    }

    #[test]
    fn titles_become_safe_stems() {
        assert_eq!(safe_title("Home/Garden"), "Home-Garden");
        assert_eq!(safe_title("  "), "Untitled");
    }
}

