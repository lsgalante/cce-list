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
//!
//! **Reading is lossless.** A file is any Markdown note: task lines become
//! items, keeping how they were written (indentation, `*`/`+`/`1.` bullets,
//! a custom status such as Obsidian's `[/]`), and every other line —
//! headings, prose, blank lines, fenced code — is kept verbatim with the
//! task below it ([`Item::before`]) or after the last one
//! ([`ListFile::trailer`]). A load and save of an untouched file writes it
//! back byte for byte (bar a missing final newline), so cce-list can open
//! an ordinary note without eating it. (It used to adopt every non-task
//! line as an item, turning a heading into a checkbox on the next save.)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    pub text: String,
    pub done: bool,
    /// Server identity for synced items; None for purely local ones.
    pub uid: Option<String>,
    /// What came before the `[` as written — `  - `, `* `, `1. ` — when it
    /// is not the plain `- ` a new item gets.
    pub prefix: Option<String>,
    /// The status between the brackets when it is neither ` ` nor `x`
    /// (`/`, `-`, `>` …). Such an item reads as done, and keeps its mark
    /// until it is unticked.
    pub mark: Option<char>,
    /// Non-task lines just above this item, verbatim, written back before
    /// it. Deleting the item hands them to the next one ([`remove_item`]).
    pub before: Vec<String>,
}

/// One checklist: its file stem, its server identity (if mirrored), items,
/// and whatever non-task lines follow the last item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListFile {
    pub title: String,
    pub id: Option<String>,
    pub items: Vec<Item>,
    pub trailer: Vec<String>,
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

/// A task line's parts: (prefix before `[`, status char, text after `] `).
/// `- [ ] a`, `  * [x] b`, `1. [/] c`; the bracket must be followed by a
/// space or end the line.
fn task_line(line: &str) -> Option<(&str, char, &str)> {
    let body = line.trim_start();
    let indent = line.len() - body.len();
    let bullet = if let Some(r) = body.strip_prefix(['-', '*', '+']) {
        body.len() - r.len()
    } else {
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        let after = &body[digits..];
        if digits == 0 || !(after.starts_with(". ") || after.starts_with(") ")) {
            return None;
        }
        digits + 1
    };
    let rest = body[bullet..].strip_prefix(' ')?;
    let mut chars = rest.chars();
    if chars.next()? != '[' {
        return None;
    }
    let status = chars.next()?;
    if chars.next()? != ']' {
        return None;
    }
    let after = chars.as_str();
    let text = match after.strip_prefix(' ') {
        Some(t) => t,
        None if after.is_empty() => "",
        None => return None,
    };
    let prefix_len = indent + bullet + 1;
    Some((&line[..prefix_len], status, text))
}

/// Split text into items and the non-task lines after the last one. Lines
/// inside fenced code blocks are never tasks.
pub fn parse_body(text: &str) -> (Vec<Item>, Vec<String>) {
    let mut items = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            pending.push(line.to_string());
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            pending.push(line.to_string());
            continue;
        }
        match task_line(line) {
            Some((prefix, status, rest)) => {
                let (text, uid) = split_uid_comment(rest);
                let done = status != ' ';
                items.push(Item {
                    text: text.to_string(),
                    done,
                    uid,
                    prefix: (prefix != "- ").then(|| prefix.to_string()),
                    mark: (done && status != 'x').then_some(status),
                    before: std::mem::take(&mut pending),
                });
            }
            None => pending.push(line.to_string()),
        }
    }
    (items, pending)
}

/// The items of a body, its trailing lines dropped (tests, the migration).
pub fn parse_items(text: &str) -> Vec<Item> {
    parse_body(text).0
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
    let mut out = String::new();
    for i in items {
        for line in &i.before {
            out.push_str(line);
            out.push('\n');
        }
        let mark = if i.done { i.mark.unwrap_or('x') } else { ' ' };
        let prefix = i.prefix.as_deref().unwrap_or("- ");
        let sep = if i.text.is_empty() && i.uid.is_none() { "" } else { " " };
        match &i.uid {
            Some(uid) => out.push_str(&format!("{prefix}[{mark}]{sep}{} <!-- uid:{uid} -->\n", i.text)),
            None => out.push_str(&format!("{prefix}[{mark}]{sep}{}\n", i.text)),
        }
    }
    out
}

/// Remove item `i`, handing the lines kept above it to the item that
/// follows (or the trailer), so deleting a task never deletes a heading.
pub fn remove_item(list: &mut ListFile, i: usize) -> Item {
    let mut item = list.items.remove(i);
    let before = std::mem::take(&mut item.before);
    match list.items.get_mut(i) {
        Some(next) => {
            let mut lines = before;
            lines.append(&mut next.before);
            next.before = lines;
        }
        None => {
            let mut lines = before;
            lines.append(&mut list.trailer);
            list.trailer = lines;
        }
    }
    item
}

/// `Vec::retain` for items, keeping the lines above a dropped item with
/// the next kept one. Lines that no kept item follows are returned, for
/// the caller to put at the front of the trailer.
pub fn retain_items(items: &mut Vec<Item>, mut keep: impl FnMut(&Item) -> bool) -> Vec<String> {
    let mut carried: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(items.len());
    for mut item in items.drain(..) {
        if keep(&item) {
            if !carried.is_empty() {
                carried.append(&mut item.before);
                item.before = std::mem::take(&mut carried);
            }
            out.push(item);
        } else {
            carried.append(&mut item.before);
        }
    }
    *items = out;
    carried
}

/// A whole list file: the optional `<!-- list:ID -->` header, then the
/// body's items and trailing lines.
pub fn parse_file(text: &str) -> (Option<String>, Vec<Item>, Vec<String>) {
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
        let (items, trailer) = parse_body(&rest.join("\n"));
        return (Some(id.to_string()), items, trailer);
    }
    let (items, trailer) = parse_body(text);
    (None, items, trailer)
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

pub fn serialize_file(list: &ListFile) -> String {
    let mut out = serialize_list(list.id.as_deref(), &list.items);
    for line in &list.trailer {
        out.push_str(line);
        out.push('\n');
    }
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
        let (id, items, trailer) = parse_file(&text);
        out.push(ListFile { title, id, items, trailer });
    }
    out.sort_by_key(|l| l.title.to_lowercase());
    Ok(out)
}

pub fn load_list(title: &str) -> std::io::Result<ListFile> {
    let text = std::fs::read_to_string(list_path(title))?;
    let (id, items, trailer) = parse_file(&text);
    Ok(ListFile { title: safe_title(title), id, items, trailer })
}

/// Write-temp-then-rename in the same directory, so a crash mid-write never
/// leaves a truncated list behind.
pub fn save_list(list: &ListFile) -> std::io::Result<()> {
    atomic_write(&list_path(&list.title), &serialize_file(list))
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
        save_list(&ListFile { title, id, items, trailer: Vec::new() })?;
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
            Item { text: "water the plants".into(), done: false, uid: None, ..Default::default() },
            Item { text: "renew passport".into(), done: true, uid: Some("AB-12".into()), ..Default::default() },
        ];
        assert_eq!(parse_items(&serialize_items(&items)), items);
    }

    /// A hand-edited file survives a load/save cycle untouched: plain lines
    /// stay lines (they are no longer adopted as items), `[X]` reads as done.
    #[test]
    fn foreign_lines_are_kept_not_adopted() {
        let (items, trailer) = parse_body("buy stamps\n- [X] call mom\n\n  - [ ] indented\n");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].before, ["buy stamps"]);
        assert!(items[0].done && items[0].mark == Some('X'));
        assert_eq!(items[1].prefix.as_deref(), Some("  - "));
        assert_eq!(items[1].before, [""]);
        assert!(trailer.is_empty());
    }

    #[test]
    fn a_note_round_trips_byte_for_byte() {
        let note = "---\ntags: [x]\n---\n# Heading\n\nSome prose.\n- [ ] open\n  - [x] done nested\n* [/] in progress\n1. [ ] numbered\n- plain bullet\n- [ ]\n\n```\n- [ ] in code\n```\n## Tail\n";
        let (id, items, trailer) = parse_file(note);
        assert_eq!(id, None);
        assert_eq!(items.len(), 5, "{items:?}");
        assert_eq!(items[2].mark, Some('/'));
        let list = ListFile { title: "n".into(), id, items, trailer };
        assert_eq!(serialize_file(&list), note);
    }

    #[test]
    fn deleting_a_task_keeps_the_lines_above_it() {
        let (id, items, trailer) = parse_file("# A\n- [ ] one\n## B\n- [ ] two\nend\n");
        let mut list = ListFile { title: "t".into(), id, items, trailer };
        remove_item(&mut list, 1);
        assert_eq!(serialize_file(&list), "# A\n- [ ] one\n## B\nend\n");
        remove_item(&mut list, 0);
        assert_eq!(serialize_file(&list), "# A\n## B\nend\n");
        let (_, mut items, _) = parse_file("x\n- [ ] a\ny\n- [ ] b\nz\n- [ ] c\n");
        let orphans = retain_items(&mut items, |i| i.text == "b");
        assert_eq!(items[0].before, ["x", "y"]);
        assert_eq!(orphans, ["z"]);
    }

    /// Every list on this machine reads and writes back unchanged. Reads
    /// only; run by hand: `cargo test -p cce-list -- --ignored`.
    #[test]
    #[ignore]
    fn real_lists_round_trip() {
        let Ok(entries) = std::fs::read_dir(lists_dir()) else { return };
        for e in entries.flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap();
            let (id, items, trailer) = parse_file(&text);
            let list = ListFile { title: String::new(), id, items, trailer };
            let back = serialize_file(&list);
            let want = if text.ends_with('\n') || text.is_empty() { text.clone() } else { format!("{text}\n") };
            assert_eq!(back, want, "{}", e.path().display());
        }
    }

    #[test]
    fn unticking_drops_a_custom_mark() {
        let mut items = parse_items("- [/] half\n");
        items[0].done = false;
        assert_eq!(serialize_items(&items), "- [ ] half\n");
    }

    #[test]
    fn uid_comment_is_identity_not_text() {
        let parsed = parse_items("- [ ] call mom <!-- uid:X-1 -->\n- [ ] literal <!-- not a uid -->\n");
        assert_eq!(parsed[0], Item { text: "call mom".into(), done: false, uid: Some("X-1".into()), ..Default::default() });
        // A comment that is not `uid:` stays part of the text.
        assert_eq!(parsed[1].uid, None);
        assert_eq!(parsed[1].text, "literal <!-- not a uid -->");
    }

    #[test]
    fn list_header_round_trips_and_is_optional() {
        let items = vec![Item { text: "a".into(), done: false, uid: None, ..Default::default() }];
        let text = serialize_list(Some("MDM5"), &items);
        assert_eq!(parse_list(&text), (Some("MDM5".into()), items.clone()));
        // No header: a hand-made file is a local-only list, first line and all.
        assert_eq!(parse_list("- [ ] a\n"), (None, items));
        // A header that is not `list:` is just a kept line.
        let (id, kept) = parse_list("<!-- note -->\n- [ ] a\n");
        assert_eq!(id, None);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].before, ["<!-- note -->"]);
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

