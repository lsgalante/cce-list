//! `cce-list-sync` — two-way sync between remote task lists and cce-list's
//! markdown checklists.
//!
//! Accounts are cce-mail's (accounts.json, owned by cce-system-interface).
//! Two backends, one of which is chosen per run (`--backend google|icloud`;
//! default Google when an OAuth account exists, else iCloud):
//!
//! - **Google Tasks**, over its REST API with the OAuth tokens the settings
//!   app's Google sign-in stores (it requests the `tasks` scope). The
//!   access token is refreshed in memory each run, never written back. This
//!   is the backend that reaches the phone. Lists sync both ways: a file
//!   created in cce-list becomes a Google list, a list made on the phone
//!   becomes a file, renames and deletions follow in either direction.
//! - **iCloud Reminders**, over CalDAV VTODO with the "cce-mail" keyring
//!   password. Each VTODO calendar is a list, read-only as a list (items
//!   inside it sync both ways; creating, renaming or deleting calendars is
//!   not attempted). Kept working but unlikely to be useful: an account
//!   whose Reminders were "upgraded" (CloudKit) exposes only Apple's legacy
//!   stub list over CalDAV, invisible to the Reminders app.
//!
//! The DAV discovery code is deliberately duplicated from cce-calendar-sync
//! rather than extracted: every crate builds standalone (multi-repo), and
//! two copies of ~100 lines beats a new published crate until a third
//! consumer exists.
//!
//! The merge is three-way against `sync-state.json`, the last-synced server
//! snapshot: for lists (by id: title), and for items (by uid: text, done).
//! A difference between the files and the state is a local edit to push;
//! between the server and the state, a remote edit to pull; both changed →
//! local wins (the next tick reconciles). Deletions propagate both ways,
//! guarded: a missing `lists/` directory re-imports instead of deleting, a
//! run that would delete every remote list refuses, and one that would
//! delete most of a list's tracked items (>5 and >50%) refuses without
//! `--force-deletes` — a mangled tree must not empty the phone. State
//! entries record their account, so a run only reasons about its own
//! backend's lists and items; rows another backend owns pass through.
//!
//! An item whose uid belongs to a different list than the file it sits in
//! has been moved by hand; it is recreated in the new list and deleted from
//! the old one (the Tasks API has no cross-list move). CalDAV pushes PATCH
//! the fetched iCalendar rather than rebuilding it, so due dates, notes, and
//! alarms Apple attached survive a checkbox toggle; Google pushes are
//! field-level PATCHes for the same reason. Recurring reminders (RRULE) are
//! skipped entirely. Server-side completed items that were never tracked
//! are not imported, and neither are blank-title tasks.
//!
//! Usage: `cce-list-sync [--dry-run] [--force-deletes] [--backend google|icloud]`.
//! Driven by cce-list-sync.timer; harmless to run by hand.

use std::collections::{BTreeMap, BTreeSet};

use cce_list::{
    delete_list, legacy_path, list_path, lists_dir, load_current, load_lists, load_sync_state,
    rename_list, safe_title, save_current, save_list, save_sync_state, Item, ListFile, SyncState,
    SyncedItem, SyncedList,
};
use chrono::Utc;

const CALDAV_ROOT: &str = "https://caldav.icloud.com/";
const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";
const KEYRING_SERVICE: &str = "cce-mail";

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let force_deletes = args.iter().any(|a| a == "--force-deletes");
    let wanted = args
        .iter()
        .position(|a| a == "--backend")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.to_ascii_lowercase());

    let backend = match choose_backend(wanted.as_deref()) {
        Ok(Some(b)) => b,
        Ok(None) => {
            log::info!("no usable account in accounts.json; nothing to sync");
            return;
        }
        Err(e) => {
            log::error!("cannot read accounts: {e}");
            std::process::exit(1);
        }
    };

    match run_sync(&backend, dry_run, force_deletes) {
        Ok(()) => {}
        Err(e) => {
            log::error!("{}: sync failed: {e}", backend.email());
            std::process::exit(1);
        }
    }
}

enum Backend {
    ICloud(Account),
    Google(GoogleAccount),
}

impl Backend {
    fn email(&self) -> &str {
        match self {
            Backend::ICloud(a) => &a.email,
            Backend::Google(a) => &a.email,
        }
    }
}

fn choose_backend(wanted: Option<&str>) -> Result<Option<Backend>, String> {
    match wanted {
        Some("icloud") => Ok(icloud_accounts()?.into_iter().next().map(Backend::ICloud)),
        Some("google") => Ok(google_accounts()?.into_iter().next().map(Backend::Google)),
        Some(other) => Err(format!("unknown --backend {other:?} (google|icloud)")),
        None => {
            if let Some(g) = google_accounts()?.into_iter().next() {
                return Ok(Some(Backend::Google(g)));
            }
            Ok(icloud_accounts()?.into_iter().next().map(Backend::ICloud))
        }
    }
}

/// Per-run credentials the pushes need beyond the account itself.
enum Session {
    ICloud,
    Google(String),
}

// ── Remote model (both backends produce it) ───────────────────────────────

#[derive(Debug, Clone)]
struct RemoteList {
    title: String,
    etag: String,
    /// Where a new item in this list is created.
    create_target: reqwest::Url,
}

#[derive(Debug)]
struct RemoteTodo {
    url: reqwest::Url,
    etag: String,
    summary: String,
    done: bool,
    /// Unfolded logical lines of the full VCALENDAR, for patch-and-PUT
    /// (CalDAV only; empty for Google).
    lines: Vec<String>,
    /// The list (id) the item lives in.
    list: String,
}

struct RemoteSnapshot {
    lists: BTreeMap<String, RemoteList>,
    todos: BTreeMap<String, RemoteTodo>,
}

// ── The pass ──────────────────────────────────────────────────────────────

fn run_sync(backend: &Backend, dry_run: bool, force_deletes: bool) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let email = backend.email().to_string();

    // Whether there is any local knowledge at all, judged BEFORE load_lists
    // runs the legacy migration (which creates lists/ from list.md).
    let had_local = lists_dir().exists() || legacy_path().exists();
    let mut local = load_lists().map_err(|e| format!("reading lists: {e}"))?;
    let mut state = load_sync_state().map_err(|e| format!("sync-state.json: {e}"))?;
    if !had_local && (!state.items.is_empty() || !state.lists.is_empty()) {
        // The tree is gone (fresh clone, deleted directory). Re-import
        // rather than reading absence as "delete everything on the server".
        log::warn!("lists/ missing; discarding sync state and re-importing");
        state = SyncState::default();
    }

    let (remote, session) = match backend {
        Backend::ICloud(acc) => (fetch_icloud(&client, acc)?, Session::ICloud),
        Backend::Google(acc) => {
            let token = google_access_token(&client, acc)?;
            (google_fetch(&client, &token, &acc.email)?, Session::Google(token))
        }
    };
    let ops = Ops { client: &client, backend, session: &session };

    // ── Lists ───────────────────────────────────────────────────────────
    let mine_lists: BTreeMap<String, SyncedList> = state
        .lists
        .iter()
        .filter(|(_, v)| v.account == email)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let lplan = plan_lists(&local, &mine_lists, &remote.lists);
    if !force_deletes && !lplan.push_deletes.is_empty() && lplan.push_deletes.len() >= remote.lists.len()
    {
        return Err(format!(
            "refusing to delete every remote list ({}) — if that was really meant, run \
             cce-list-sync --force-deletes",
            lplan.push_deletes.len()
        ));
    }
    log::info!(
        "{email}: lists — pull {} new / {} renamed / {} deleted; push {} new / {} renamed / {} deleted",
        lplan.pull_new.len(),
        lplan.pull_renames.len(),
        lplan.pull_deletes.len(),
        lplan.push_creates.len(),
        lplan.push_renames.len(),
        lplan.push_deletes.len(),
    );
    if dry_run {
        print_list_plan(&lplan, &remote.lists);
    } else {
        apply_list_plan(&ops, &lplan, &mut local, &mut state, &remote, &email)?;
    }

    // The lists this run can sync items for: local files with an id the
    // server knows (after the list phase, that is every list unless a push
    // failed and will retry next tick).
    let mut remote_lists = remote.lists.clone();
    for (id, l) in &state.lists {
        // Lists created this run are not in the fetched snapshot yet.
        if l.account == email && !remote_lists.contains_key(id) {
            if let Some(target) = ops.create_target_for(id) {
                remote_lists.insert(
                    id.clone(),
                    RemoteList { title: l.title.clone(), etag: l.etag.clone(), create_target: target },
                );
            }
        }
    }

    // ── Items, per list ─────────────────────────────────────────────────
    let foreign_uids: BTreeSet<String> = state
        .items
        .iter()
        .filter(|(_, v)| v.account != email)
        .map(|(k, _)| k.clone())
        .collect();
    // Where each uid lives, server-side or as last synced: a row found in a
    // different file has been moved by hand. Precomputed so the loop below
    // can take `state` mutably.
    let owners: BTreeMap<String, String> = state
        .items
        .iter()
        .filter(|(_, v)| v.account == email)
        .map(|(k, v)| (k.clone(), v.list_id()))
        .chain(remote.todos.iter().map(|(k, t)| (k.clone(), t.list.clone())))
        .collect();
    let owner_of = |uid: &str| -> Option<String> { owners.get(uid).cloned() };

    let mut current_title = load_current();
    for file in &local {
        let Some(list_id) = file.id.clone() else {
            continue; // creation failed this run; retried next tick
        };
        if !remote_lists.contains_key(&list_id) {
            continue;
        }
        let rows: Vec<Item> = file
            .items
            .iter()
            .filter(|i| i.uid.as_deref().is_none_or(|u| !foreign_uids.contains(u)))
            .map(|i| {
                let moved = i
                    .uid
                    .as_deref()
                    .and_then(owner_of)
                    .is_some_and(|owner| owner != list_id);
                if moved {
                    // Recreate here; the old list's plan pushes the delete.
                    Item { uid: None, ..i.clone() }
                } else {
                    i.clone()
                }
            })
            .collect();
        // Rows that were moved keep their old uid on disk until apply_local
        // swaps it, so remember which text came from which uid.
        let moved_from: Vec<(String, String)> = file
            .items
            .iter()
            .filter_map(|i| {
                let uid = i.uid.as_deref()?;
                (owner_of(uid)? != list_id).then(|| (i.text.clone(), uid.to_string()))
            })
            .collect();

        let mine = SyncState {
            items: state
                .items
                .iter()
                .filter(|(_, v)| v.account == email && v.list_id() == list_id)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            lists: BTreeMap::new(),
        };
        let todos: BTreeMap<String, &RemoteTodo> = remote
            .todos
            .iter()
            .filter(|(_, t)| t.list == list_id)
            .map(|(k, v)| (k.clone(), v))
            .collect();
        let plan = plan_items(&rows, &mine, &todos);

        if !force_deletes && plan.push_deletes.len() > 5 && plan.push_deletes.len() * 2 > mine.items.len()
        {
            return Err(format!(
                "{}: refusing to delete {} of {} tracked items on the server — if the list \
                 was really emptied on purpose, run cce-list-sync --force-deletes",
                file.title,
                plan.push_deletes.len(),
                mine.items.len()
            ));
        }
        log::info!(
            "{email}: {} — pull {} new / {} changed / {} deleted; push {} changed / {} new / {} deleted",
            file.title,
            plan.pull_new.len(),
            plan.pull_updates.len(),
            plan.pull_deletes.len(),
            plan.push_updates.len(),
            plan.push_creates.len(),
            plan.push_deletes.len(),
        );
        if dry_run {
            print_item_plan(&plan, &todos);
            continue;
        }

        let target = &remote_lists[&list_id].create_target;
        let created = apply_item_plan(&ops, &plan, &rows, &todos, target, &list_id, &mut state, &email)?;

        // Local side last, as deltas on a FRESH read: the user may have
        // edited the list while the network calls ran, and rows this plan
        // does not touch must survive verbatim.
        let mut fresh = cce_list::load_list(&file.title).unwrap_or_else(|_| ListFile {
            title: file.title.clone(),
            id: Some(list_id.clone()),
            items: Vec::new(),
        });
        fresh.id = Some(list_id.clone());
        apply_local(&mut fresh.items, &plan, &todos, &created, &moved_from);
        save_list(&fresh).map_err(|e| e.to_string())?;
    }

    if !dry_run {
        // The app's pointer may name a list this run renamed or removed.
        if let Some(cur) = current_title.take() {
            if !list_path(&cur).exists() {
                if let Some(first) = local.first() {
                    save_current(&first.title).map_err(|e| e.to_string())?;
                }
            }
        }
        save_sync_state(&state).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The backend operations, bundled so the two phases share one signature.
struct Ops<'a> {
    client: &'a reqwest::blocking::Client,
    backend: &'a Backend,
    session: &'a Session,
}

impl Ops<'_> {
    fn token(&self) -> &str {
        match self.session {
            Session::Google(t) => t,
            Session::ICloud => "",
        }
    }

    fn create_target_for(&self, list_id: &str) -> Option<reqwest::Url> {
        match self.backend {
            Backend::Google(_) => reqwest::Url::parse(&format!("{TASKS_API}/lists/{list_id}/tasks")).ok(),
            Backend::ICloud(_) => reqwest::Url::parse(list_id).ok(),
        }
    }

    fn list_create(&self, title: &str) -> Result<(String, RemoteList), String> {
        match self.backend {
            Backend::Google(_) => google_list_create(self.client, self.token(), title),
            Backend::ICloud(_) => Err("the iCloud backend cannot create lists".into()),
        }
    }

    fn list_rename(&self, id: &str, title: &str) -> Result<String, String> {
        match self.backend {
            Backend::Google(_) => google_list_rename(self.client, self.token(), id, title),
            Backend::ICloud(_) => Err("the iCloud backend cannot rename lists".into()),
        }
    }

    fn list_delete(&self, id: &str) -> Result<(), String> {
        match self.backend {
            Backend::Google(_) => google_list_delete(self.client, self.token(), id),
            Backend::ICloud(_) => Err("the iCloud backend cannot delete lists".into()),
        }
    }

    fn item_update(&self, todo: &RemoteTodo, text: &str, done: bool) -> Result<String, String> {
        match self.backend {
            Backend::ICloud(acc) => {
                let body = patch_vtodo(&todo.lines, text, done);
                put_ics(self.client, acc, &todo.url, &body, Some(&todo.etag))
            }
            Backend::Google(_) => google_update(self.client, self.token(), &todo.url, text, done),
        }
    }

    fn item_create(
        &self,
        target: &reqwest::Url,
        text: &str,
        done: bool,
    ) -> Result<(String, reqwest::Url, String), String> {
        match self.backend {
            Backend::ICloud(acc) => {
                let uid = new_uid();
                let url = target.join(&format!("{uid}.ics")).map_err(|e| e.to_string())?;
                put_ics(self.client, acc, &url, &new_vtodo(&uid, text, done), None)
                    .map(|etag| (uid, url, etag))
            }
            Backend::Google(_) => google_create(self.client, self.token(), target, text, done),
        }
    }

    fn item_delete(&self, url: &reqwest::Url, etag: &str) -> Result<(), String> {
        match self.backend {
            Backend::ICloud(acc) => delete_ics(self.client, acc, url, etag),
            Backend::Google(_) => google_delete(self.client, self.token(), url),
        }
    }
}

// ── List planning (pure; tested) ──────────────────────────────────────────

#[derive(Default, Debug, PartialEq)]
struct ListPlan {
    /// Remote ids with no local file and no state: new on the server.
    pull_new: Vec<String>,
    /// (id, local title, remote title): renamed on the server.
    pull_renames: Vec<(String, String, String)>,
    /// (id, local title): the server list is gone.
    pull_deletes: Vec<(String, String)>,
    /// (title, stale id if the file carried one the server does not know).
    push_creates: Vec<(String, Option<String>)>,
    /// (id, new title): renamed locally.
    push_renames: Vec<(String, String)>,
    /// Ids whose local file is gone.
    push_deletes: Vec<String>,
    /// Title/etag unchanged in substance; just record the server's etag.
    refresh: Vec<String>,
}

fn plan_lists(
    local: &[ListFile],
    state: &BTreeMap<String, SyncedList>,
    remote: &BTreeMap<String, RemoteList>,
) -> ListPlan {
    let mut plan = ListPlan::default();
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    for file in local {
        match &file.id {
            None => plan.push_creates.push((file.title.clone(), None)),
            Some(id) => match (remote.get(id), state.get(id)) {
                (Some(r), base) => {
                    claimed.insert(id.as_str());
                    let remote_title = safe_title(&r.title);
                    match base {
                        // Never synced as a list before (e.g. the migrated
                        // legacy file): the server's name wins.
                        None if remote_title != file.title => {
                            plan.pull_renames.push((id.clone(), file.title.clone(), remote_title));
                        }
                        None => plan.refresh.push(id.clone()),
                        Some(b) => {
                            let base_title = safe_title(&b.title);
                            let local_changed = file.title != base_title;
                            let remote_changed = remote_title != base_title;
                            if local_changed && file.title != remote_title {
                                plan.push_renames.push((id.clone(), file.title.clone()));
                            } else if remote_changed && file.title != remote_title {
                                plan.pull_renames.push((id.clone(), file.title.clone(), remote_title));
                            } else if r.etag != b.etag || local_changed {
                                plan.refresh.push(id.clone());
                            }
                        }
                    }
                }
                (None, Some(_)) => plan.pull_deletes.push((id.clone(), file.title.clone())),
                // A header the server never heard of and the state does not
                // track: recreate rather than orphan the file.
                (None, None) => plan.push_creates.push((file.title.clone(), Some(id.clone()))),
            },
        }
    }
    for id in remote.keys() {
        if claimed.contains(id.as_str()) {
            continue;
        }
        if state.contains_key(id) {
            plan.push_deletes.push(id.clone());
        } else {
            plan.pull_new.push(id.clone());
        }
    }
    plan
}

fn print_list_plan(plan: &ListPlan, remote: &BTreeMap<String, RemoteList>) {
    for id in &plan.pull_new {
        println!("list pull new:    {} ({id})", remote[id].title);
    }
    for (id, from, to) in &plan.pull_renames {
        println!("list pull rename: {from} -> {to} ({id})");
    }
    for (id, title) in &plan.pull_deletes {
        println!("list pull delete: {title} ({id})");
    }
    for (title, _) in &plan.push_creates {
        println!("list push new:    {title}");
    }
    for (id, title) in &plan.push_renames {
        println!("list push rename: -> {title} ({id})");
    }
    for id in &plan.push_deletes {
        println!("list push delete: {} ({id})", remote.get(id).map(|l| l.title.as_str()).unwrap_or("?"));
    }
}

/// A free file stem for a server title: `Groceries`, then `Groceries (2)`.
fn unique_local_title(title: &str, taken: &[ListFile]) -> String {
    let base = safe_title(title);
    let exists = |t: &str| taken.iter().any(|l| l.title == t) || list_path(t).exists();
    if !exists(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|t| !exists(t))
        .expect("unbounded")
}

fn apply_list_plan(
    ops: &Ops,
    plan: &ListPlan,
    local: &mut Vec<ListFile>,
    state: &mut SyncState,
    remote: &RemoteSnapshot,
    email: &str,
) -> Result<(), String> {
    // Server side first; each success is recorded before the next call, so
    // a failure mid-way retries just the remainder next tick.
    for (title, stale_id) in &plan.push_creates {
        match ops.list_create(title) {
            Ok((id, rl)) => {
                if let Some(file) = local.iter_mut().find(|f| f.title == *title) {
                    file.id = Some(id.clone());
                    save_list(file).map_err(|e| e.to_string())?;
                }
                if let Some(old) = stale_id {
                    state.lists.remove(old);
                }
                state.lists.insert(
                    id,
                    SyncedList { title: rl.title, etag: rl.etag, account: email.to_string() },
                );
            }
            Err(e) => log::warn!("creating list {title:?} failed (will retry next tick): {e}"),
        }
    }
    for (id, title) in &plan.push_renames {
        match ops.list_rename(id, title) {
            Ok(etag) => {
                state.lists.insert(
                    id.clone(),
                    SyncedList { title: title.clone(), etag, account: email.to_string() },
                );
            }
            Err(e) => log::warn!("renaming list {id} failed (will retry next tick): {e}"),
        }
    }
    for id in &plan.push_deletes {
        match ops.list_delete(id) {
            Ok(()) => {
                state.lists.remove(id);
                state.items.retain(|_, v| v.list_id() != *id);
            }
            Err(e) => log::warn!("deleting list {id} failed (will retry next tick): {e}"),
        }
    }

    // Then the local tree.
    let current = load_current();
    for (id, from, to) in &plan.pull_renames {
        let to = unique_local_title(to, local);
        rename_list(from, &to).map_err(|e| format!("renaming {from} -> {to}: {e}"))?;
        if let Some(file) = local.iter_mut().find(|f| f.title == *from) {
            file.title = to.clone();
        }
        if current.as_deref() == Some(from.as_str()) {
            save_current(&to).map_err(|e| e.to_string())?;
        }
        let r = &remote.lists[id];
        state.lists.insert(
            id.clone(),
            SyncedList { title: r.title.clone(), etag: r.etag.clone(), account: email.to_string() },
        );
    }
    for (id, title) in &plan.pull_deletes {
        delete_list(title).map_err(|e| format!("deleting {title}: {e}"))?;
        local.retain(|f| f.title != *title);
        state.lists.remove(id);
        state.items.retain(|_, v| v.list_id() != *id);
    }
    for id in &plan.pull_new {
        let r = &remote.lists[id];
        let title = unique_local_title(&r.title, local);
        let file = ListFile { title, id: Some(id.clone()), items: Vec::new() };
        save_list(&file).map_err(|e| e.to_string())?;
        local.push(file);
        state.lists.insert(
            id.clone(),
            SyncedList { title: r.title.clone(), etag: r.etag.clone(), account: email.to_string() },
        );
    }
    for id in &plan.refresh {
        let r = &remote.lists[id];
        let title = local
            .iter()
            .find(|f| f.id.as_deref() == Some(id))
            .map(|f| f.title.clone())
            .unwrap_or_else(|| r.title.clone());
        state.lists.insert(
            id.clone(),
            SyncedList { title, etag: r.etag.clone(), account: email.to_string() },
        );
    }
    local.sort_by_key(|l| l.title.to_lowercase());
    Ok(())
}

// ── Item planning (pure; tested) ──────────────────────────────────────────

#[derive(Default, Debug)]
struct Plan {
    pull_new: Vec<String>,
    pull_updates: Vec<String>,
    pull_deletes: Vec<String>,
    push_updates: Vec<String>,
    /// (text, done, the row's stale uid if it carried one) to create.
    push_creates: Vec<(String, bool, Option<String>)>,
    push_deletes: Vec<String>,
    /// Server etag moved but content is identical — track it, change nothing.
    refresh_etags: Vec<String>,
}

fn plan_items(local: &[Item], state: &SyncState, remote: &BTreeMap<String, &RemoteTodo>) -> Plan {
    let mut plan = Plan::default();
    let local_by_uid: BTreeMap<&str, &Item> = local
        .iter()
        .filter_map(|i| i.uid.as_deref().map(|u| (u, i)))
        .collect();

    for (uid, todo) in remote {
        let in_state = state.items.get(uid);
        let in_local = local_by_uid.get(uid.as_str());
        match (in_state, in_local) {
            (Some(base), Some(item)) => {
                let local_changed = item.text != base.text || item.done != base.done;
                let remote_changed = todo.summary != base.text || todo.done != base.done;
                if local_changed {
                    // Local wins on both-changed; the push makes the server
                    // match, and the next tick sees all three agree.
                    plan.push_updates.push(uid.clone());
                } else if remote_changed {
                    plan.pull_updates.push(uid.clone());
                } else if todo.etag != base.etag {
                    plan.refresh_etags.push(uid.clone());
                }
            }
            (Some(_), None) => plan.push_deletes.push(uid.clone()),
            (None, Some(item)) => {
                // Untracked but present on both ends (state lost, or a
                // hand-copied line): adopt it, local text/done winning.
                if item.text != todo.summary || item.done != todo.done {
                    plan.push_updates.push(uid.clone());
                } else {
                    plan.refresh_etags.push(uid.clone());
                }
            }
            (None, None) => {
                if !todo.done {
                    plan.pull_new.push(uid.clone());
                }
            }
        }
    }
    for uid in state.items.keys() {
        if !remote.contains_key(uid) {
            plan.pull_deletes.push(uid.clone());
        }
    }
    for item in local {
        match &item.uid {
            None => plan.push_creates.push((item.text.clone(), item.done, None)),
            // A uid the server never heard of and the state does not track:
            // recreate it rather than orphan the row.
            Some(uid) if !remote.contains_key(uid) && !state.items.contains_key(uid) => {
                plan.push_creates.push((item.text.clone(), item.done, Some(uid.clone())));
            }
            Some(_) => {}
        }
    }
    plan
}

fn print_item_plan(plan: &Plan, remote: &BTreeMap<String, &RemoteTodo>) {
    for uid in &plan.pull_new {
        println!("  pull new:    {} ({uid})", remote[uid].summary);
    }
    for uid in &plan.pull_updates {
        println!("  pull change: {} ({uid})", remote[uid].summary);
    }
    for uid in &plan.pull_deletes {
        println!("  pull delete: {uid}");
    }
    for uid in &plan.push_updates {
        println!("  push change: {uid}");
    }
    for (text, done, _) in &plan.push_creates {
        println!("  push new:    {}{text}", if *done { "[x] " } else { "" });
    }
    for uid in &plan.push_deletes {
        println!("  push delete: {uid}");
    }
}

/// Server-side half of an item plan. Returns the creates that succeeded as
/// (text, stale uid the row carried, new uid) for `apply_local` to annotate.
#[allow(clippy::too_many_arguments)]
fn apply_item_plan(
    ops: &Ops,
    plan: &Plan,
    rows: &[Item],
    todos: &BTreeMap<String, &RemoteTodo>,
    target: &reqwest::Url,
    list_id: &str,
    state: &mut SyncState,
    email: &str,
) -> Result<Vec<(String, Option<String>, String)>, String> {
    let synced = |url: &reqwest::Url, etag: String, text: &str, done: bool| SyncedItem {
        url: url.to_string(),
        etag,
        account: email.to_string(),
        text: text.to_string(),
        done,
        list: list_id.to_string(),
    };
    for uid in &plan.push_updates {
        let todo = todos[uid];
        let item = rows.iter().find(|i| i.uid.as_deref() == Some(uid)).expect("planned");
        match ops.item_update(todo, &item.text, item.done) {
            Ok(etag) => {
                state.items.insert(uid.clone(), synced(&todo.url, etag, &item.text, item.done));
            }
            Err(e) => log::warn!("push update {uid} failed (will retry next tick): {e}"),
        }
    }
    let mut created = Vec::new();
    for (text, done, stale) in &plan.push_creates {
        match ops.item_create(target, text, *done) {
            Ok((uid, url, etag)) => {
                state.items.insert(uid.clone(), synced(&url, etag, text, *done));
                created.push((text.clone(), stale.clone(), uid));
            }
            Err(e) => log::warn!("push create {text:?} failed (will retry next tick): {e}"),
        }
    }
    for uid in &plan.push_deletes {
        let entry = &state.items[uid];
        let url = reqwest::Url::parse(&entry.url).map_err(|e| e.to_string())?;
        match ops.item_delete(&url, &entry.etag) {
            Ok(()) => {
                state.items.remove(uid);
            }
            Err(e) => log::warn!("push delete {uid} failed (will retry next tick): {e}"),
        }
    }
    // Pulls refresh the state from the server snapshot.
    for uid in plan.pull_new.iter().chain(&plan.pull_updates) {
        let todo = todos[uid];
        state.items.insert(uid.clone(), synced(&todo.url, todo.etag.clone(), &todo.summary, todo.done));
    }
    for uid in &plan.pull_deletes {
        state.items.remove(uid);
    }
    for uid in &plan.refresh_etags {
        if let (Some(entry), Some(todo)) = (state.items.get_mut(uid), todos.get(uid)) {
            entry.etag = todo.etag.clone();
            entry.list = list_id.to_string();
        }
    }
    Ok(created)
}

/// Apply the plan's local half as deltas onto a fresh read of the list.
/// `moved_from` pairs a text with the stale uid its row carried when it was
/// moved in from another list, so the annotation swap finds the row.
fn apply_local(
    items: &mut Vec<Item>,
    plan: &Plan,
    remote: &BTreeMap<String, &RemoteTodo>,
    created: &[(String, Option<String>, String)],
    moved_from: &[(String, String)],
) {
    items.retain(|i| {
        i.uid.as_deref().is_none_or(|u| !plan.pull_deletes.iter().any(|d| d == u))
    });
    for uid in &plan.pull_updates {
        let todo = remote[uid];
        if let Some(item) = items.iter_mut().find(|i| i.uid.as_deref() == Some(uid)) {
            item.text = todo.summary.clone();
            item.done = todo.done;
        }
    }
    for (text, stale, uid) in created {
        let stale = stale.clone().or_else(|| {
            moved_from.iter().find(|(t, _)| t == text).map(|(_, u)| u.clone())
        });
        let row = match &stale {
            Some(old) => items.iter_mut().find(|i| i.uid.as_deref() == Some(old)),
            None => items.iter_mut().find(|i| i.uid.is_none() && i.text == *text),
        };
        if let Some(item) = row {
            item.uid = Some(uid.clone());
        }
    }
    for uid in &plan.pull_new {
        let todo = remote[uid];
        items.push(Item { text: todo.summary.clone(), done: todo.done, uid: Some(uid.clone()) });
    }
}

// ── Accounts ──────────────────────────────────────────────────────────────

struct Account {
    email: String,
    password: String,
}

#[derive(serde::Deserialize)]
struct AccountOnDisk {
    email: String,
    #[serde(default)]
    imap: String,
    #[serde(default)]
    password: String,
}

fn icloud_accounts() -> Result<Vec<Account>, String> {
    let path = cce_ui::config::cce_config_dir().join("accounts.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let on_disk: Vec<AccountOnDisk> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for acc in on_disk {
        let host = acc.imap.split(':').next().unwrap_or("");
        let icloud = host.ends_with(".mail.me.com")
            || ["@icloud.com", "@me.com", "@mac.com"].iter().any(|d| acc.email.ends_with(d));
        if !icloud {
            continue;
        }
        let password = if !acc.password.is_empty() {
            acc.password.clone()
        } else {
            match keyring::Entry::new(KEYRING_SERVICE, &acc.email).and_then(|e| e.get_password()) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("{}: no password available ({e}); skipping", acc.email);
                    continue;
                }
            }
        };
        out.push(Account { email: acc.email, password });
    }
    Ok(out)
}

// ── Google Tasks ──────────────────────────────────────────────────────────

const TASKS_API: &str = "https://tasks.googleapis.com/tasks/v1";

struct GoogleAccount {
    email: String,
    refresh_token: String,
    client_id: String,
    client_secret: String,
}

/// The OAuth fields the settings app's Google sign-in writes.
#[derive(serde::Deserialize)]
struct OAuthOnDisk {
    email: String,
    #[serde(default)]
    is_oauth: bool,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct GoogleClientConfig {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
}

fn google_accounts() -> Result<Vec<GoogleAccount>, String> {
    let dir = cce_ui::config::cce_config_dir();
    let path = dir.join("accounts.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let on_disk: Vec<OAuthOnDisk> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    // An account without its own pinned client credentials falls back to
    // the global template the settings app maintains.
    let template: GoogleClientConfig = std::fs::read_to_string(dir.join("google_client.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut out = Vec::new();
    for acc in on_disk {
        if !acc.is_oauth {
            continue;
        }
        let Some(refresh_token) = acc.refresh_token.filter(|t| !t.is_empty()) else {
            log::warn!("{}: OAuth account without a refresh token; sign in again", acc.email);
            continue;
        };
        out.push(GoogleAccount {
            email: acc.email,
            refresh_token,
            client_id: acc.client_id.filter(|s| !s.is_empty()).unwrap_or(template.client_id.clone()),
            client_secret: acc
                .client_secret
                .filter(|s| !s.is_empty())
                .unwrap_or(template.client_secret.clone()),
        });
    }
    Ok(out)
}

/// A fresh access token from the refresh grant. Tokens last an hour and a
/// tick is one request burst, so refreshing every run is simpler than
/// tracking expiry — and keeps this helper from writing accounts.json.
fn google_access_token(
    client: &reqwest::blocking::Client,
    acc: &GoogleAccount,
) -> Result<String, String> {
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", acc.client_id.as_str()),
            ("client_secret", acc.client_secret.as_str()),
            ("refresh_token", acc.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| format!("token refresh: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().map_err(|e| format!("token refresh: {e}"))?;
    if !status.is_success() {
        // invalid_grant here means the refresh token was revoked or the
        // consent predates the tasks scope — a re-login fixes both.
        return Err(format!("token refresh: HTTP {status} {body}"));
    }
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "token refresh: no access_token in response".to_string())
}

fn google_call(
    client: &reqwest::blocking::Client,
    token: &str,
    method: reqwest::Method,
    url: &str,
    query: &[(&str, &str)],
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut req = client.request(method.clone(), url).bearer_auth(token).query(query);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send().map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(serde_json::Value::Null);
    }
    let text = resp.text().map_err(|e| format!("{method} {url}: {e}"))?;
    if !status.is_success() {
        return Err(format!("{method} {url}: HTTP {status} {text}"));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("{method} {url}: bad JSON: {e}"))
}

fn google_list_url(id: &str) -> String {
    format!("{TASKS_API}/users/@me/lists/{id}")
}

/// Every list and every task in it.
fn google_fetch(
    client: &reqwest::blocking::Client,
    token: &str,
    email: &str,
) -> Result<RemoteSnapshot, String> {
    let get = |url: &str, q: &[(&str, &str)]| {
        google_call(client, token, reqwest::Method::GET, url, q, None)
    };
    let page = get(&format!("{TASKS_API}/users/@me/lists"), &[("maxResults", "100")])?;
    let mut lists = BTreeMap::new();
    for l in page["items"].as_array().into_iter().flatten() {
        let Some(id) = l["id"].as_str() else { continue };
        lists.insert(
            id.to_string(),
            RemoteList {
                title: l["title"].as_str().unwrap_or("Untitled").to_string(),
                etag: l["etag"].as_str().unwrap_or("").to_string(),
                create_target: reqwest::Url::parse(&format!("{TASKS_API}/lists/{id}/tasks"))
                    .map_err(|e| e.to_string())?,
            },
        );
    }
    log::info!("{email}: {} task list(s)", lists.len());

    let mut todos = BTreeMap::new();
    for (list_id, list) in &lists {
        let base = list.create_target.as_str().to_string();
        let mut page_token = String::new();
        loop {
            let mut q = vec![
                ("showCompleted", "true"),
                ("showHidden", "true"),
                ("maxResults", "100"),
                ("fields", "nextPageToken,items(id,title,status,etag,deleted)"),
            ];
            if !page_token.is_empty() {
                q.push(("pageToken", page_token.as_str()));
            }
            let page = get(&base, &q).map_err(|e| format!("list {}: {e}", list.title))?;
            for t in page["items"].as_array().into_iter().flatten() {
                if t["deleted"].as_bool().unwrap_or(false) {
                    continue;
                }
                let Some(id) = t["id"].as_str() else { continue };
                let summary = t["title"].as_str().unwrap_or("").trim().to_string();
                if summary.is_empty() {
                    // Google's apps mint blank placeholder tasks freely; a
                    // row with no text is nothing to remember.
                    continue;
                }
                let url = reqwest::Url::parse(&format!("{base}/{id}")).map_err(|e| e.to_string())?;
                todos.insert(id.to_string(), RemoteTodo {
                    url,
                    etag: t["etag"].as_str().unwrap_or("").to_string(),
                    summary,
                    done: t["status"].as_str() == Some("completed"),
                    lines: Vec::new(),
                    list: list_id.clone(),
                });
            }
            match page["nextPageToken"].as_str() {
                Some(next) if !next.is_empty() => page_token = next.to_string(),
                _ => break,
            }
        }
    }
    Ok(RemoteSnapshot { lists, todos })
}

fn google_list_create(
    client: &reqwest::blocking::Client,
    token: &str,
    title: &str,
) -> Result<(String, RemoteList), String> {
    let body = serde_json::json!({ "title": title });
    let resp = google_call(
        client, token, reqwest::Method::POST,
        &format!("{TASKS_API}/users/@me/lists"), &[], Some(&body),
    )?;
    let id = resp["id"].as_str().ok_or("created list has no id")?.to_string();
    let list = RemoteList {
        title: resp["title"].as_str().unwrap_or(title).to_string(),
        etag: resp["etag"].as_str().unwrap_or("").to_string(),
        create_target: reqwest::Url::parse(&format!("{TASKS_API}/lists/{id}/tasks"))
            .map_err(|e| e.to_string())?,
    };
    Ok((id, list))
}

fn google_list_rename(
    client: &reqwest::blocking::Client,
    token: &str,
    id: &str,
    title: &str,
) -> Result<String, String> {
    let body = serde_json::json!({ "title": title });
    let resp = google_call(client, token, reqwest::Method::PATCH, &google_list_url(id), &[], Some(&body))?;
    Ok(resp["etag"].as_str().unwrap_or("").to_string())
}

fn google_list_delete(client: &reqwest::blocking::Client, token: &str, id: &str) -> Result<(), String> {
    match google_call(client, token, reqwest::Method::DELETE, &google_list_url(id), &[], None) {
        Ok(_) => Ok(()),
        Err(e) if e.contains("HTTP 404") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Field-level PATCH: title and status only, so due dates and notes set in
/// Google's own apps ride through. Un-completing must also clear the
/// completion timestamp or the API rejects the status.
fn google_update(
    client: &reqwest::blocking::Client,
    token: &str,
    url: &reqwest::Url,
    text: &str,
    done: bool,
) -> Result<String, String> {
    let body = if done {
        serde_json::json!({ "title": text, "status": "completed" })
    } else {
        serde_json::json!({ "title": text, "status": "needsAction", "completed": null })
    };
    let resp = google_call(client, token, reqwest::Method::PATCH, url.as_str(), &[], Some(&body))?;
    Ok(resp["etag"].as_str().unwrap_or("").to_string())
}

fn google_create(
    client: &reqwest::blocking::Client,
    token: &str,
    target: &reqwest::Url,
    text: &str,
    done: bool,
) -> Result<(String, reqwest::Url, String), String> {
    let status = if done { "completed" } else { "needsAction" };
    let body = serde_json::json!({ "title": text, "status": status });
    let resp = google_call(client, token, reqwest::Method::POST, target.as_str(), &[], Some(&body))?;
    let id = resp["id"].as_str().ok_or("created task has no id")?.to_string();
    let url = reqwest::Url::parse(&format!("{}/{id}", target.as_str().trim_end_matches('/')))
        .map_err(|e| e.to_string())?;
    Ok((id, url, resp["etag"].as_str().unwrap_or("").to_string()))
}

fn google_delete(
    client: &reqwest::blocking::Client,
    token: &str,
    url: &reqwest::Url,
) -> Result<(), String> {
    match google_call(client, token, reqwest::Method::DELETE, url.as_str(), &[], None) {
        Ok(_) => Ok(()),
        // Already gone counts as done.
        Err(e) if e.contains("HTTP 404") => Ok(()),
        Err(e) => Err(e),
    }
}

// ── CalDAV (iCloud) ───────────────────────────────────────────────────────

/// Every VTODO calendar as a list (its URL is the list id) and its items.
fn fetch_icloud(client: &reqwest::blocking::Client, acc: &Account) -> Result<RemoteSnapshot, String> {
    let root = reqwest::Url::parse(CALDAV_ROOT).expect("static url");
    let principal = discover_href(
        client, acc, &root, "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><current-user-principal/></prop></propfind>"#,
        "current-user-principal",
    )?;
    let home = discover_href(
        client, acc, &principal, "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><prop><C:calendar-home-set/></prop></propfind>"#,
        "calendar-home-set",
    )?;
    let calendars = todo_calendars(client, acc, &home)?;
    if calendars.is_empty() {
        return Err("no VTODO calendars (Reminders lists) found".into());
    }
    log::info!("{}: {} Reminders list(s)", acc.email, calendars.len());

    let mut lists = BTreeMap::new();
    let mut todos = BTreeMap::new();
    let mut skipped_recurring = 0usize;
    for (url, name) in &calendars {
        lists.insert(
            url.to_string(),
            RemoteList { title: name.clone(), etag: String::new(), create_target: url.clone() },
        );
        fetch_todos(client, acc, url, &mut todos, &mut skipped_recurring)
            .map_err(|e| format!("list {name}: {e}"))?;
    }
    if skipped_recurring > 0 {
        log::info!("{skipped_recurring} recurring reminder(s) left alone (RRULE)");
    }
    Ok(RemoteSnapshot { lists, todos })
}

fn dav_request(
    client: &reqwest::blocking::Client,
    acc: &Account,
    method: &str,
    url: &reqwest::Url,
    depth: &str,
    body: &str,
) -> Result<String, String> {
    let resp = client
        .request(reqwest::Method::from_bytes(method.as_bytes()).expect("static method"), url.clone())
        .basic_auth(&acc.email, Some(&acc.password))
        .header("Depth", depth)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{method} {url}: HTTP {status}"));
    }
    Ok(text)
}

fn discover_href(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
    depth: &str,
    body: &str,
    prop: &str,
) -> Result<reqwest::Url, String> {
    let xml = dav_request(client, acc, "PROPFIND", url, depth, body)?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("bad multistatus: {e}"))?;
    let href = doc
        .descendants()
        .find(|n| n.tag_name().name() == prop)
        .and_then(|n| n.descendants().find(|c| c.tag_name().name() == "href"))
        .and_then(|n| n.text())
        .ok_or_else(|| format!("no {prop} in PROPFIND response"))?;
    url.join(href.trim()).map_err(|e| format!("bad {prop} href {href:?}: {e}"))
}

fn todo_calendars(
    client: &reqwest::blocking::Client,
    acc: &Account,
    home: &reqwest::Url,
) -> Result<Vec<(reqwest::Url, String)>, String> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop><resourcetype/><displayname/><C:supported-calendar-component-set/></prop>
</propfind>"#;
    let xml = dav_request(client, acc, "PROPFIND", home, "1", body)?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("bad multistatus: {e}"))?;
    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| n.tag_name().name() == "response") {
        let Some(href) = resp
            .children()
            .find(|c| c.tag_name().name() == "href")
            .and_then(|n| n.text())
        else {
            continue;
        };
        let is_calendar = resp.descendants().any(|n| {
            n.tag_name().name() == "calendar" && n.tag_name().namespace() == Some(CALDAV_NS)
        });
        if !is_calendar {
            continue;
        }
        // Unlike the events side, VTODO support must be stated: a calendar
        // that lists no component set is an events calendar here.
        let supports_vtodo = resp
            .descendants()
            .filter(|n| n.tag_name().name() == "comp")
            .filter_map(|n| n.attribute("name"))
            .any(|c| c == "VTODO");
        if !supports_vtodo {
            continue;
        }
        let name = resp
            .descendants()
            .find(|n| n.tag_name().name() == "displayname")
            .and_then(|n| n.text())
            .unwrap_or(href)
            .to_string();
        let url = home.join(href.trim()).map_err(|e| format!("bad href {href:?}: {e}"))?;
        if url.path().trim_end_matches('/') == home.path().trim_end_matches('/') {
            continue;
        }
        out.push((url, name));
    }
    Ok(out)
}

fn fetch_todos(
    client: &reqwest::blocking::Client,
    acc: &Account,
    cal: &reqwest::Url,
    todos: &mut BTreeMap<String, RemoteTodo>,
    skipped_recurring: &mut usize,
) -> Result<(), String> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO"/></C:comp-filter></C:filter>
</C:calendar-query>"#;
    let xml = dav_request(client, acc, "REPORT", cal, "1", body)?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("bad multistatus: {e}"))?;
    for resp in doc.descendants().filter(|n| n.tag_name().name() == "response") {
        let href = resp
            .children()
            .find(|c| c.tag_name().name() == "href")
            .and_then(|n| n.text())
            .unwrap_or_default();
        let etag = resp
            .descendants()
            .find(|n| n.tag_name().name() == "getetag")
            .and_then(|n| n.text())
            .unwrap_or_default()
            .to_string();
        let Some(ics) = resp
            .descendants()
            .find(|n| n.tag_name().name() == "calendar-data")
            .and_then(|n| n.text())
        else {
            continue;
        };
        let url = cal.join(href.trim()).map_err(|e| format!("bad href {href:?}: {e}"))?;
        match parse_vtodo(ics) {
            Some(parsed) if parsed.recurring => *skipped_recurring += 1,
            Some(parsed) => {
                todos.insert(parsed.uid.clone(), RemoteTodo {
                    url,
                    etag,
                    summary: parsed.summary,
                    done: parsed.done,
                    lines: parsed.lines,
                    list: cal.to_string(),
                });
            }
            None => log::warn!("unparsable VTODO at {url}, skipping"),
        }
    }
    Ok(())
}

fn put_ics(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
    body: &str,
    etag: Option<&str>,
) -> Result<String, String> {
    let mut req = client
        .put(url.clone())
        .basic_auth(&acc.email, Some(&acc.password))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .body(body.to_string());
    req = match etag {
        // An empty stored etag (a PUT whose response carried none) falls
        // back to an unconditional overwrite of our own resource.
        Some(e) if !e.is_empty() => req.header("If-Match", e),
        Some(_) => req,
        None => req.header("If-None-Match", "*"),
    };
    let resp = req.send().map_err(|e| format!("PUT {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("PUT {url}: HTTP {status}"));
    }
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !etag.is_empty() {
        return Ok(etag);
    }
    // No ETag on the PUT response: ask for it, so the next If-Match works.
    Ok(fetch_etag(client, acc, url).unwrap_or_default())
}

fn fetch_etag(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
) -> Option<String> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><getetag/></prop></propfind>"#;
    let xml = dav_request(client, acc, "PROPFIND", url, "0", body).ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;
    doc.descendants()
        .find(|n| n.tag_name().name() == "getetag")
        .and_then(|n| n.text())
        .map(|s| s.to_string())
}

fn delete_ics(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
    etag: &str,
) -> Result<(), String> {
    let mut req = client.delete(url.clone()).basic_auth(&acc.email, Some(&acc.password));
    if !etag.is_empty() {
        req = req.header("If-Match", etag);
    }
    let resp = req.send().map_err(|e| format!("DELETE {url}: {e}"))?;
    let status = resp.status();
    // Already gone counts as done.
    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(format!("DELETE {url}: HTTP {status}"))
    }
}

// ── iCalendar: parse, patch, mint ─────────────────────────────────────────

struct ParsedTodo {
    uid: String,
    summary: String,
    done: bool,
    recurring: bool,
    lines: Vec<String>,
}

fn unfold(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(raw.to_string());
    }
    lines.retain(|l| !l.is_empty());
    lines
}

fn split_content_line(line: &str) -> Option<(&str, &str)> {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => return Some((&line[..i], &line[i + 1..])),
            _ => {}
        }
    }
    None
}

fn unescape_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn escape_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn parse_vtodo(ics: &str) -> Option<ParsedTodo> {
    let lines = unfold(ics);
    let mut in_todo = false;
    let mut uid = String::new();
    let mut summary = String::new();
    let mut done = false;
    let mut recurring = false;
    for line in &lines {
        let Some((head, value)) = split_content_line(line) else { continue };
        let name = head.split(';').next().unwrap_or("").to_ascii_uppercase();
        match name.as_str() {
            "BEGIN" if value.eq_ignore_ascii_case("VTODO") => in_todo = true,
            "END" if value.eq_ignore_ascii_case("VTODO") => in_todo = false,
            _ if !in_todo => {}
            _ => match name.as_str() {
                "UID" => uid = value.trim().to_string(),
                "SUMMARY" => summary = unescape_text(value.trim()),
                "STATUS" => done |= value.trim().eq_ignore_ascii_case("COMPLETED"),
                "COMPLETED" => done = true,
                "PERCENT-COMPLETE" => done |= value.trim() == "100",
                "RRULE" | "RDATE" => recurring = true,
                _ => {}
            },
        }
    }
    (!uid.is_empty()).then_some(ParsedTodo { uid, summary, done, recurring, lines })
}

/// Rewrite only SUMMARY and the completion trio inside the VTODO block,
/// leaving every other property (DUE, DESCRIPTION, VALARM, X-APPLE-*)
/// exactly as the server sent it.
fn patch_vtodo(lines: &[String], summary: &str, done: bool) -> String {
    let now = Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);
    let mut in_todo = false;
    for line in lines {
        let name = split_content_line(line)
            .map(|(h, _)| h.split(';').next().unwrap_or("").to_ascii_uppercase())
            .unwrap_or_default();
        let value = split_content_line(line).map(|(_, v)| v).unwrap_or_default();
        if name == "BEGIN" && value.eq_ignore_ascii_case("VTODO") {
            in_todo = true;
            out.push(line.clone());
            continue;
        }
        if name == "END" && value.eq_ignore_ascii_case("VTODO") {
            out.push(format!("SUMMARY:{}", escape_text(summary)));
            if done {
                out.push("STATUS:COMPLETED".to_string());
                out.push("PERCENT-COMPLETE:100".to_string());
                out.push(format!("COMPLETED:{now}"));
            } else {
                out.push("STATUS:NEEDS-ACTION".to_string());
                out.push("PERCENT-COMPLETE:0".to_string());
            }
            in_todo = false;
            out.push(line.clone());
            continue;
        }
        if in_todo
            && matches!(name.as_str(), "SUMMARY" | "STATUS" | "PERCENT-COMPLETE" | "COMPLETED")
        {
            continue;
        }
        out.push(line.clone());
    }
    let mut s = out.join("\r\n");
    s.push_str("\r\n");
    s
}

fn new_vtodo(uid: &str, summary: &str, done: bool) -> String {
    let now = Utc::now().format("%Y%m%dT%H%M%SZ");
    let status = if done {
        format!("STATUS:COMPLETED\r\nPERCENT-COMPLETE:100\r\nCOMPLETED:{now}\r\n")
    } else {
        "STATUS:NEEDS-ACTION\r\n".to_string()
    };
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//cce//cce-list-sync//EN\r\n\
         BEGIN:VTODO\r\nUID:{uid}\r\nDTSTAMP:{now}\r\nCREATED:{now}\r\n\
         SUMMARY:{}\r\n{status}END:VTODO\r\nEND:VCALENDAR\r\n",
        escape_text(summary)
    )
}

/// Random-enough UID from the kernel, no uuid dependency.
fn new_uid() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        // Fall back to a timestamp; uniqueness against one user's own list.
        return format!("CCE-{}", Utc::now().format("%Y%m%dT%H%M%S%fZ"));
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!("CCE-{}-{}-{}", &hex[..8], &hex[8..16], &hex[16..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, done: bool, uid: Option<&str>) -> Item {
        Item { text: text.into(), done, uid: uid.map(String::from) }
    }

    fn todo(summary: &str, done: bool, etag: &str) -> RemoteTodo {
        RemoteTodo {
            url: reqwest::Url::parse("https://example.com/cal/x.ics").unwrap(),
            etag: etag.into(),
            summary: summary.into(),
            done,
            lines: Vec::new(),
            list: "L".into(),
        }
    }

    fn synced(text: &str, done: bool, etag: &str) -> SyncedItem {
        SyncedItem {
            url: "https://example.com/cal/x.ics".into(),
            etag: etag.into(),
            account: "a@icloud.com".into(),
            text: text.into(),
            done,
            list: "L".into(),
        }
    }

    fn rlist(title: &str, etag: &str) -> RemoteList {
        RemoteList {
            title: title.into(),
            etag: etag.into(),
            create_target: reqwest::Url::parse("https://example.com/l/tasks").unwrap(),
        }
    }

    fn slist(title: &str, etag: &str) -> SyncedList {
        SyncedList { title: title.into(), etag: etag.into(), account: "a".into() }
    }

    fn lfile(title: &str, id: Option<&str>) -> ListFile {
        ListFile { title: title.into(), id: id.map(String::from), items: Vec::new() }
    }

    fn refs<'a>(m: &'a BTreeMap<String, RemoteTodo>) -> BTreeMap<String, &'a RemoteTodo> {
        m.iter().map(|(k, v)| (k.clone(), v)).collect()
    }

    #[test]
    fn merge_decision_table() {
        let local = vec![
            item("unchanged", false, Some("u1")),
            item("toggled here", true, Some("u2")), // local change → push
            item("old name", false, Some("u3")),    // remote change → pull
            item("fresh local", false, None),       // no uid → create
        ];
        // u4 in state but not local → deleted here → push delete.
        // u5 on server, unknown → pull new. u6 server-completed, unknown → ignore.
        let mut state = SyncState::default();
        state.items.insert("u1".into(), synced("unchanged", false, "e1"));
        state.items.insert("u2".into(), synced("toggled here", false, "e2"));
        state.items.insert("u3".into(), synced("old name", false, "e3"));
        state.items.insert("u4".into(), synced("deleted here", false, "e4"));
        let mut remote = BTreeMap::new();
        remote.insert("u1".into(), todo("unchanged", false, "e1"));
        remote.insert("u2".into(), todo("toggled here", false, "e2"));
        remote.insert("u3".into(), todo("renamed on phone", false, "e3b"));
        remote.insert("u4".into(), todo("deleted here", false, "e4"));
        remote.insert("u5".into(), todo("from the phone", false, "e5"));
        remote.insert("u6".into(), todo("ancient done thing", true, "e6"));

        let p = plan_items(&local, &state, &refs(&remote));
        assert_eq!(p.push_updates, vec!["u2"]);
        assert_eq!(p.pull_updates, vec!["u3"]);
        assert_eq!(p.push_deletes, vec!["u4"]);
        assert_eq!(p.pull_new, vec!["u5"]);
        assert_eq!(p.push_creates, vec![("fresh local".to_string(), false, None)]);
        assert!(p.pull_deletes.is_empty());
    }

    #[test]
    fn both_changed_local_wins() {
        let local = vec![item("mine", false, Some("u1"))];
        let mut state = SyncState::default();
        state.items.insert("u1".into(), synced("base", false, "e1"));
        let mut remote = BTreeMap::new();
        remote.insert("u1".into(), todo("theirs", false, "e2"));
        let p = plan_items(&local, &state, &refs(&remote));
        assert_eq!(p.push_updates, vec!["u1"]);
        assert!(p.pull_updates.is_empty());
    }

    #[test]
    fn server_deletion_pulls_row_out() {
        let local = vec![item("gone on phone", false, Some("u1"))];
        let mut state = SyncState::default();
        state.items.insert("u1".into(), synced("gone on phone", false, "e1"));
        let remote = BTreeMap::new();
        let p = plan_items(&local, &state, &refs(&remote));
        assert_eq!(p.pull_deletes, vec!["u1"]);
        assert!(p.push_creates.is_empty());

        let mut items = local;
        apply_local(&mut items, &p, &refs(&remote), &[], &[]);
        assert!(items.is_empty());
    }

    #[test]
    fn recreated_row_is_reannotated_by_its_stale_uid() {
        // A row carrying a uid nobody knows is recreated; the new uid must
        // replace the stale one on THAT row, not on a same-text sibling.
        let plan = Plan {
            push_creates: vec![("dup".into(), false, Some("stale".into()))],
            ..Default::default()
        };
        let mut items = vec![item("dup", false, None), item("dup", false, Some("stale"))];
        apply_local(&mut items, &plan, &BTreeMap::new(), &[("dup".into(), Some("stale".into()), "new".into())], &[]);
        assert_eq!(items[0].uid, None);
        assert_eq!(items[1].uid.as_deref(), Some("new"));
    }

    #[test]
    fn moved_row_swaps_uid_via_moved_from() {
        let plan = Plan { push_creates: vec![("milk".into(), false, None)], ..Default::default() };
        let mut items = vec![item("milk", false, Some("from-other-list"))];
        apply_local(
            &mut items,
            &plan,
            &BTreeMap::new(),
            &[("milk".into(), None, "new".into())],
            &[("milk".into(), "from-other-list".into())],
        );
        assert_eq!(items[0].uid.as_deref(), Some("new"));
    }

    #[test]
    fn patch_preserves_foreign_properties() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:u\r\nDUE;VALUE=DATE:20261001\r\nSUMMARY:old\r\nSTATUS:NEEDS-ACTION\r\nX-APPLE-SORT-ORDER:7\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let patched = patch_vtodo(&unfold(ics), "new, name", true);
        assert!(patched.contains("DUE;VALUE=DATE:20261001"));
        assert!(patched.contains("X-APPLE-SORT-ORDER:7"));
        assert!(patched.contains("SUMMARY:new\\, name"));
        assert!(patched.contains("STATUS:COMPLETED"));
        assert!(patched.contains("PERCENT-COMPLETE:100"));
        assert!(!patched.contains("SUMMARY:old"));
        assert!(!patched.contains("NEEDS-ACTION"));
    }

    #[test]
    fn fresh_edits_survive_apply_local() {
        // A row typed while the sync was talking to the network is untouched.
        let plan = Plan { pull_new: vec!["u9".into()], ..Default::default() };
        let mut remote = BTreeMap::new();
        remote.insert("u9".into(), todo("from phone", false, "e9"));
        let mut items = vec![item("typed mid-sync", false, None)];
        apply_local(&mut items, &plan, &refs(&remote), &[], &[]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "typed mid-sync");
        assert_eq!(items[1].uid.as_deref(), Some("u9"));
    }

    #[test]
    fn list_decision_table() {
        let local = vec![
            lfile("Tasks", Some("L1")),      // unchanged
            lfile("Errands", Some("L2")),    // renamed here (base "Chores") → push
            lfile("Work", Some("L3")),       // renamed on phone → pull
            lfile("Gone remote", Some("L4")), // server deleted it → pull delete
            lfile("Brand new", None),        // → push create
            lfile("Orphan", Some("LX")),     // header nobody knows → recreate
            lfile("Tasks (legacy)", Some("L7")), // never synced as list; server name wins
        ];
        let mut state = BTreeMap::new();
        state.insert("L1".into(), slist("Tasks", "e1"));
        state.insert("L2".into(), slist("Chores", "e2"));
        state.insert("L3".into(), slist("Work", "e3"));
        state.insert("L4".into(), slist("Gone remote", "e4"));
        state.insert("L5".into(), slist("Deleted here", "e5")); // no file → push delete
        let mut remote = BTreeMap::new();
        remote.insert("L1".into(), rlist("Tasks", "e1"));
        remote.insert("L2".into(), rlist("Chores", "e2"));
        remote.insert("L3".into(), rlist("Office", "e3b"));
        remote.insert("L5".into(), rlist("Deleted here", "e5"));
        remote.insert("L6".into(), rlist("From the phone", "e6")); // → pull new
        remote.insert("L7".into(), rlist("LSGalante12's list", "e7"));

        let p = plan_lists(&local, &state, &remote);
        assert_eq!(p.push_renames, vec![("L2".to_string(), "Errands".to_string())]);
        assert_eq!(p.pull_renames, vec![
            ("L3".to_string(), "Work".to_string(), "Office".to_string()),
            ("L7".to_string(), "Tasks (legacy)".to_string(), "LSGalante12's list".to_string()),
        ]);
        assert_eq!(p.pull_deletes, vec![("L4".to_string(), "Gone remote".to_string())]);
        assert_eq!(p.push_creates, vec![
            ("Brand new".to_string(), None),
            ("Orphan".to_string(), Some("LX".to_string())),
        ]);
        assert_eq!(p.push_deletes, vec!["L5"]);
        assert_eq!(p.pull_new, vec!["L6"]);
        // L1 matches the base exactly; nothing to record.
        assert!(p.refresh.is_empty());
    }

    #[test]
    fn list_rename_both_sides_local_wins() {
        let local = vec![lfile("Mine", Some("L1"))];
        let mut state = BTreeMap::new();
        state.insert("L1".into(), slist("Base", "e1"));
        let mut remote = BTreeMap::new();
        remote.insert("L1".into(), rlist("Theirs", "e2"));
        let p = plan_lists(&local, &state, &remote);
        assert_eq!(p.push_renames, vec![("L1".to_string(), "Mine".to_string())]);
        assert!(p.pull_renames.is_empty());
    }
}
