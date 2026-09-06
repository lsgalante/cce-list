//! `cce-list-sync` — two-way sync between a remote task list and cce-list's
//! markdown checklist.
//!
//! Accounts are cce-mail's (accounts.json, owned by cce-system-interface).
//! Two backends, one of which is chosen per run (`--backend google|icloud`;
//! default Google when an OAuth account exists, else iCloud):
//!
//! - **Google Tasks**, over its REST API with the OAuth tokens the settings
//!   app's Google sign-in stores (it requests the `tasks` scope). The
//!   access token is refreshed in memory each run, never written back. This
//!   is the backend that reaches the phone.
//! - **iCloud Reminders**, over CalDAV VTODO with the "cce-mail" keyring
//!   password. Kept working but unlikely to be useful: an account whose
//!   Reminders were "upgraded" (CloudKit) exposes only Apple's legacy stub
//!   list over CalDAV, invisible to the Reminders app.
//!
//! The DAV discovery code is deliberately duplicated from cce-calendar-sync
//! rather than extracted: every crate builds standalone (multi-repo), and
//! two copies of ~100 lines beats a new published crate until a third
//! consumer exists.
//!
//! The merge is three-way against `sync-state.json`, the last-synced server
//! snapshot per uid: a difference between the list and the state is a local
//! edit to push; between the server and the state, a remote edit to pull;
//! both changed → local wins (the next tick reconciles). Deletions propagate
//! both ways, guarded: a missing list.md re-imports instead of deleting, and
//! a run that would delete most tracked items (>5 and >50%) refuses without
//! `--force-deletes` — a mangled file must not empty the phone. State
//! entries record their account, so a run only reasons about its own
//! backend's items; rows another backend owns pass through untouched.
//!
//! CalDAV pushes PATCH the fetched iCalendar rather than rebuilding it, so
//! due dates, notes, and alarms Apple attached survive a checkbox toggle;
//! Google pushes are field-level PATCHes for the same reason. Recurring
//! reminders (RRULE) are skipped entirely — completing one means "advance
//! to the next occurrence", which this checkbox model cannot say.
//! Server-side completed items that were never tracked are not imported
//! (years of checked-off junk stays on the phone).
//!
//! Usage: `cce-list-sync [--dry-run] [--force-deletes] [--backend google|icloud]`.
//! Driven by cce-list-sync.timer; harmless to run by hand.

use std::collections::{BTreeMap, BTreeSet};

use cce_list::{
    atomic_write, data_path, load_sync_state, parse_items, save_sync_state, serialize_items,
    Item, SyncState, SyncedItem,
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

/// Every task in every list; the default list is where creates go.
fn google_fetch(
    client: &reqwest::blocking::Client,
    token: &str,
    email: &str,
) -> Result<RemoteSnapshot, String> {
    let get = |url: &str, q: &[(&str, &str)]| {
        google_call(client, token, reqwest::Method::GET, url, q, None)
    };
    let default = get(&format!("{TASKS_API}/users/@me/lists/@default"), &[])?;
    let default_id = default["id"].as_str().ok_or("default task list has no id")?.to_string();
    let lists = get(&format!("{TASKS_API}/users/@me/lists"), &[("maxResults", "100")])?;
    let lists: Vec<(String, String)> = lists["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| Some((l["id"].as_str()?.to_string(), l["title"].as_str().unwrap_or("?").to_string())))
        .collect();
    log::info!(
        "{email}: {} task list(s), new items go to {}",
        lists.len(),
        lists.iter().find(|(id, _)| *id == default_id).map(|(_, t)| t.as_str()).unwrap_or("default")
    );

    let mut todos = BTreeMap::new();
    for (list_id, title) in &lists {
        let base = format!("{TASKS_API}/lists/{list_id}/tasks");
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
            let page = get(&base, &q).map_err(|e| format!("list {title}: {e}"))?;
            for t in page["items"].as_array().into_iter().flatten() {
                if t["deleted"].as_bool().unwrap_or(false) {
                    continue;
                }
                let Some(id) = t["id"].as_str() else { continue };
                let url = reqwest::Url::parse(&format!("{base}/{id}")).map_err(|e| e.to_string())?;
                let summary = t["title"].as_str().unwrap_or("").trim().to_string();
                todos.insert(id.to_string(), RemoteTodo {
                    url,
                    etag: t["etag"].as_str().unwrap_or("").to_string(),
                    summary: if summary.is_empty() { "(untitled)".to_string() } else { summary },
                    done: t["status"].as_str() == Some("completed"),
                    lines: Vec::new(),
                });
            }
            match page["nextPageToken"].as_str() {
                Some(next) if !next.is_empty() => page_token = next.to_string(),
                _ => break,
            }
        }
    }
    let create_target = reqwest::Url::parse(&format!("{TASKS_API}/lists/{default_id}/tasks"))
        .map_err(|e| e.to_string())?;
    Ok(RemoteSnapshot { todos, create_target })
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
) -> Result<(String, reqwest::Url, String), String> {
    let body = serde_json::json!({ "title": text, "status": "needsAction" });
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
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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

// ── The pass ──────────────────────────────────────────────────────────────

fn run_sync(backend: &Backend, dry_run: bool, force_deletes: bool) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let email = backend.email().to_string();

    // Read the list first: if it is unreadable there is nothing safe to do.
    let list_exists = data_path().exists();
    let local_all = if list_exists {
        parse_items(&std::fs::read_to_string(data_path()).map_err(|e| e.to_string())?)
    } else {
        Vec::new()
    };
    let mut state = load_sync_state().map_err(|e| format!("sync-state.json: {e}"))?;
    if !list_exists && !state.items.is_empty() {
        // The list is gone (fresh clone, deleted file). Re-import rather
        // than reading absence as "delete everything on the server".
        log::warn!("list.md missing; discarding sync state and re-importing");
        state = SyncState::default();
    }

    // This run reasons only about its own account: the state entries it
    // owns, and the local rows not claimed by some other account's entry.
    let foreign: BTreeSet<&str> = state
        .items
        .iter()
        .filter(|(_, v)| v.account != email)
        .map(|(k, _)| k.as_str())
        .collect();
    let local: Vec<Item> = local_all
        .iter()
        .filter(|i| i.uid.as_deref().is_none_or(|u| !foreign.contains(u)))
        .cloned()
        .collect();
    let mine = SyncState {
        items: state.items.iter().filter(|(_, v)| v.account == email).map(|(k, v)| (k.clone(), v.clone())).collect(),
    };

    let (remote, session) = match backend {
        Backend::ICloud(acc) => (fetch_remote(&client, acc)?, Session::ICloud),
        Backend::Google(acc) => {
            let token = google_access_token(&client, acc)?;
            (google_fetch(&client, &token, &acc.email)?, Session::Google(token))
        }
    };
    let plan = plan(&local, &mine, &remote.todos);

    if !force_deletes && plan.push_deletes.len() > 5 && plan.push_deletes.len() * 2 > mine.items.len()
    {
        return Err(format!(
            "refusing to delete {} of {} tracked items on the server — if the list \
             was really emptied on purpose, run cce-list-sync --force-deletes",
            plan.push_deletes.len(),
            mine.items.len()
        ));
    }

    log::info!(
        "{email}: pull {} new / {} changed / {} deleted; push {} changed / {} new / {} deleted",
        plan.pull_new.len(),
        plan.pull_updates.len(),
        plan.pull_deletes.len(),
        plan.push_updates.len(),
        plan.push_creates.len(),
        plan.push_deletes.len(),
    );
    if dry_run {
        print_plan(&plan, &remote.todos);
        return Ok(());
    }

    // Server side first: every push refreshes `state` only on success, so a
    // failed request is simply retried next tick.
    for uid in &plan.push_updates {
        let todo = &remote.todos[uid];
        let item = local.iter().find(|i| i.uid.as_deref() == Some(uid)).expect("planned");
        let pushed = match (backend, &session) {
            (Backend::ICloud(acc), _) => {
                let body = patch_vtodo(&todo.lines, &item.text, item.done);
                put_ics(&client, acc, &todo.url, &body, Some(&todo.etag))
            }
            (Backend::Google(_), Session::Google(token)) => {
                google_update(&client, token, &todo.url, &item.text, item.done)
            }
            (Backend::Google(_), Session::ICloud) => unreachable!("session matches backend"),
        };
        match pushed {
            Ok(etag) => {
                state.items.insert(uid.clone(), SyncedItem {
                    url: todo.url.to_string(),
                    etag,
                    account: email.clone(),
                    text: item.text.clone(),
                    done: item.done,
                });
            }
            Err(e) => log::warn!("push update {uid} failed (will retry next tick): {e}"),
        }
    }
    let mut created: Vec<(String, String)> = Vec::new(); // (text, uid) to annotate
    for text in &plan.push_creates {
        let made = match (backend, &session) {
            (Backend::ICloud(acc), _) => {
                let uid = new_uid();
                remote
                    .create_target
                    .join(&format!("{uid}.ics"))
                    .map_err(|e| e.to_string())
                    .and_then(|url| {
                        put_ics(&client, acc, &url, &new_vtodo(&uid, text, false), None)
                            .map(|etag| (uid, url, etag))
                    })
            }
            (Backend::Google(_), Session::Google(token)) => {
                google_create(&client, token, &remote.create_target, text)
            }
            (Backend::Google(_), Session::ICloud) => unreachable!("session matches backend"),
        };
        match made {
            Ok((uid, url, etag)) => {
                state.items.insert(uid.clone(), SyncedItem {
                    url: url.to_string(),
                    etag,
                    account: email.clone(),
                    text: text.clone(),
                    done: false,
                });
                created.push((text.clone(), uid));
            }
            Err(e) => log::warn!("push create {text:?} failed (will retry next tick): {e}"),
        }
    }
    for uid in &plan.push_deletes {
        let entry = &state.items[uid];
        let url = reqwest::Url::parse(&entry.url).map_err(|e| e.to_string())?;
        let gone = match (backend, &session) {
            (Backend::ICloud(acc), _) => delete_ics(&client, acc, &url, &entry.etag),
            (Backend::Google(_), Session::Google(token)) => google_delete(&client, token, &url),
            (Backend::Google(_), Session::ICloud) => unreachable!("session matches backend"),
        };
        match gone {
            Ok(()) => {
                state.items.remove(uid);
            }
            Err(e) => log::warn!("push delete {uid} failed (will retry next tick): {e}"),
        }
    }

    // Pulls refresh the state from the server snapshot.
    for uid in plan.pull_new.iter().chain(&plan.pull_updates) {
        let todo = &remote.todos[uid];
        state.items.insert(uid.clone(), SyncedItem {
            url: todo.url.to_string(),
            etag: todo.etag.clone(),
            account: email.clone(),
            text: todo.summary.clone(),
            done: todo.done,
        });
    }
    for uid in &plan.pull_deletes {
        state.items.remove(uid);
    }
    for uid in &plan.refresh_etags {
        if let (Some(entry), Some(todo)) = (state.items.get_mut(uid), remote.todos.get(uid)) {
            entry.etag = todo.etag.clone();
        }
    }

    // Local side last, as deltas on a FRESH read: the user may have edited
    // the list while the network calls ran, and rows this plan does not
    // touch must survive verbatim.
    let mut fresh = if data_path().exists() {
        parse_items(&std::fs::read_to_string(data_path()).map_err(|e| e.to_string())?)
    } else {
        Vec::new()
    };
    apply_local(&mut fresh, &plan, &remote.todos, &created);
    atomic_write(&data_path(), &serialize_items(&fresh)).map_err(|e| e.to_string())?;
    save_sync_state(&state).map_err(|e| e.to_string())?;
    Ok(())
}

fn print_plan(plan: &Plan, remote: &BTreeMap<String, RemoteTodo>) {
    for uid in &plan.pull_new {
        println!("pull new:    {} ({uid})", remote[uid].summary);
    }
    for uid in &plan.pull_updates {
        println!("pull change: {} ({uid})", remote[uid].summary);
    }
    for uid in &plan.pull_deletes {
        println!("pull delete: {uid}");
    }
    for uid in &plan.push_updates {
        println!("push change: {uid}");
    }
    for text in &plan.push_creates {
        println!("push new:    {text}");
    }
    for uid in &plan.push_deletes {
        println!("push delete: {uid}");
    }
}

// ── Merge planning (pure; the tests live on this) ─────────────────────────

#[derive(Default, Debug)]
struct Plan {
    pull_new: Vec<String>,
    pull_updates: Vec<String>,
    pull_deletes: Vec<String>,
    push_updates: Vec<String>,
    /// Texts of local uid-less rows to create server-side.
    push_creates: Vec<String>,
    push_deletes: Vec<String>,
    /// Server etag moved but content is identical — track it, change nothing.
    refresh_etags: Vec<String>,
}

fn plan(local: &[Item], state: &SyncState, remote: &BTreeMap<String, RemoteTodo>) -> Plan {
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
    for (uid, _) in &state.items {
        if !remote.contains_key(uid) {
            if local_by_uid.contains_key(uid.as_str()) {
                plan.pull_deletes.push(uid.clone());
            } else {
                // Gone on both ends independently; just forget it.
                plan.pull_deletes.push(uid.clone());
            }
        }
    }
    for item in local {
        match &item.uid {
            None => plan.push_creates.push(item.text.clone()),
            // A uid the server never heard of and the state does not track:
            // recreate it under that uid rather than orphaning the row.
            Some(uid) if !remote.contains_key(uid) && !state.items.contains_key(uid) => {
                plan.push_creates.push(item.text.clone());
            }
            Some(_) => {}
        }
    }
    plan
}

/// Apply the plan's local half as deltas onto a fresh read of the list.
fn apply_local(
    items: &mut Vec<Item>,
    plan: &Plan,
    remote: &BTreeMap<String, RemoteTodo>,
    created: &[(String, String)],
) {
    items.retain(|i| {
        i.uid.as_deref().is_none_or(|u| !plan.pull_deletes.iter().any(|d| d == u))
    });
    for uid in &plan.pull_updates {
        let todo = &remote[uid];
        if let Some(item) = items.iter_mut().find(|i| i.uid.as_deref() == Some(uid)) {
            item.text = todo.summary.clone();
            item.done = todo.done;
        }
    }
    for (text, uid) in created {
        // A row we recreated under its own stale uid already carries it.
        if let Some(item) =
            items.iter_mut().find(|i| i.uid.is_none() && i.text == *text)
        {
            item.uid = Some(uid.clone());
        } else if let Some(item) = items
            .iter_mut()
            .find(|i| i.text == *text && i.uid.as_deref() == Some(uid))
        {
            item.uid = Some(uid.clone());
        }
    }
    for uid in &plan.pull_new {
        let todo = &remote[uid];
        items.push(Item { text: todo.summary.clone(), done: todo.done, uid: Some(uid.clone()) });
    }
}

// ── CalDAV ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct RemoteTodo {
    url: reqwest::Url,
    etag: String,
    summary: String,
    done: bool,
    /// Unfolded logical lines of the full VCALENDAR, for patch-and-PUT.
    lines: Vec<String>,
}

struct RemoteSnapshot {
    todos: BTreeMap<String, RemoteTodo>,
    /// Where push_creates land: the list named "Reminders" if there is one,
    /// else the first VTODO calendar.
    create_target: reqwest::Url,
}

fn fetch_remote(
    client: &reqwest::blocking::Client,
    acc: &Account,
) -> Result<RemoteSnapshot, String> {
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
    let lists = todo_calendars(client, acc, &home)?;
    if lists.is_empty() {
        return Err("no VTODO calendars (Reminders lists) found".into());
    }
    let create_target = lists
        .iter()
        .find(|(_, name)| name == "Reminders")
        .unwrap_or(&lists[0])
        .0
        .clone();
    log::info!(
        "{}: {} Reminders list(s), new items go to {}",
        acc.email,
        lists.len(),
        lists.iter().find(|(u, _)| *u == create_target).map(|(_, n)| n.as_str()).unwrap_or("?")
    );

    let mut todos = BTreeMap::new();
    let mut skipped_recurring = 0usize;
    for (url, name) in &lists {
        fetch_todos(client, acc, url, &mut todos, &mut skipped_recurring)
            .map_err(|e| format!("list {name}: {e}"))?;
    }
    if skipped_recurring > 0 {
        log::info!("{skipped_recurring} recurring reminder(s) left alone (RRULE)");
    }
    Ok(RemoteSnapshot { todos, create_target })
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
        }
    }

    fn synced(text: &str, done: bool, etag: &str) -> SyncedItem {
        SyncedItem {
            url: "https://example.com/cal/x.ics".into(),
            etag: etag.into(),
            account: "a@icloud.com".into(),
            text: text.into(),
            done,
        }
    }

    #[test]
    fn merge_decision_table() {
        let local = vec![
            item("unchanged", false, Some("u1")),
            item("toggled here", true, Some("u2")),  // local change → push
            item("renamed on phone", false, Some("u3")), // remote change → pull
            item("fresh local", false, None),        // no uid → create
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

        // The local u3 text matches the state ("old name" changed remotely),
        // so fix the fixture: local u3 must equal the state's text.
        let mut local = local;
        local[2].text = "old name".into();

        let p = plan(&local, &state, &remote);
        assert_eq!(p.push_updates, vec!["u2"]);
        assert_eq!(p.pull_updates, vec!["u3"]);
        assert_eq!(p.push_deletes, vec!["u4"]);
        assert_eq!(p.pull_new, vec!["u5"]);
        assert_eq!(p.push_creates, vec!["fresh local"]);
        assert!(p.pull_deletes.is_empty());
    }

    #[test]
    fn both_changed_local_wins() {
        let local = vec![item("mine", false, Some("u1"))];
        let mut state = SyncState::default();
        state.items.insert("u1".into(), synced("base", false, "e1"));
        let mut remote = BTreeMap::new();
        remote.insert("u1".into(), todo("theirs", false, "e2"));
        let p = plan(&local, &state, &remote);
        assert_eq!(p.push_updates, vec!["u1"]);
        assert!(p.pull_updates.is_empty());
    }

    #[test]
    fn server_deletion_pulls_row_out() {
        let local = vec![item("gone on phone", false, Some("u1"))];
        let mut state = SyncState::default();
        state.items.insert("u1".into(), synced("gone on phone", false, "e1"));
        let remote = BTreeMap::new();
        let p = plan(&local, &state, &remote);
        assert_eq!(p.pull_deletes, vec!["u1"]);
        assert!(p.push_creates.is_empty());

        let mut items = local;
        apply_local(&mut items, &p, &remote, &[]);
        assert!(items.is_empty());
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
        apply_local(&mut items, &plan, &remote, &[]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "typed mid-sync");
        assert_eq!(items[1].uid.as_deref(), Some("u9"));
    }
}
