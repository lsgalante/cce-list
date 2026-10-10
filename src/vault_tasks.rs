//! The "Vault tasks" view: every open checkbox in the notes vault (the one
//! cce-notes uses, from `vault { path }` in config.kdl), grouped by note.
//! Ticking writes the checkbox back into its note through `cce-vault`,
//! which re-reads the file first, so an edit made a second ago elsewhere is
//! not overwritten. Typing adds a task to today's daily note.
//!
//! A task ticked here stays on screen, struck through, until the view is
//! left — so a mis-tick can be undone — although the view otherwise lists
//! only open tasks. Clicking a note's name opens it in cce-notes.
//!
//! The vault is watched like any cce-vault client: a watcher thread queues
//! changed paths, and `poll` applies them on the app's loop.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cce_vault::index::stem;
use cce_vault::{Index, Task, VaultWatcher};

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Note { path: String, name: String },
    Task { path: String, line: usize, text: String, done: bool, due: Option<chrono::NaiveDate> },
}

pub struct VaultTasks {
    index: Index,
    _watcher: Option<VaultWatcher>,
    pending: Arc<Mutex<Vec<PathBuf>>>,
    /// (path, line) of tasks ticked in this view since it was entered.
    ticked: HashSet<(String, usize)>,
    pub rows: Vec<Row>,
}

/// The view's rows: each note with open (or just-ticked) tasks, then them.
pub fn build_rows<'a>(tasks: impl Iterator<Item = (&'a str, &'a Task)>, ticked: &HashSet<(String, usize)>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut last: Option<&str> = None;
    for (path, t) in tasks {
        let shown = t.is_open() || ticked.contains(&(path.to_string(), t.line));
        if !shown {
            continue;
        }
        if last != Some(path) {
            rows.push(Row::Note { path: path.to_string(), name: stem(path).to_string() });
            last = Some(path);
        }
        rows.push(Row::Task {
            path: path.to_string(),
            line: t.line,
            text: t.text.trim().to_string(),
            done: !t.is_open(),
            due: t.due,
        });
    }
    rows
}

impl VaultTasks {
    /// The configured vault's tasks, or `None` when no vault is set up.
    /// `wake` runs on the watcher's thread after each batch is queued, so
    /// the app's loop comes round to [`poll`](Self::poll) instead of
    /// finding the batch on its next timed tick.
    pub fn open(wake: impl Fn() + Send + 'static) -> Option<VaultTasks> {
        let root = cce_vault::config::vault_root(None).ok()?;
        let index = Index::open(&root, true).map_err(|e| log::warn!("vault tasks: {e}")).ok()?;
        let pending = Arc::new(Mutex::new(Vec::new()));
        let queue = pending.clone();
        let watcher = VaultWatcher::spawn(&root, move |paths| {
            queue.lock().unwrap_or_else(|e| e.into_inner()).extend(paths);
            wake();
        })
        .map_err(|e| log::warn!("vault watcher: {e}"))
        .ok();
        let mut v = VaultTasks { index, _watcher: watcher, pending, ticked: HashSet::new(), rows: Vec::new() };
        v.rebuild();
        Some(v)
    }

    fn rebuild(&mut self) {
        self.rows = build_rows(self.index.tasks(), &self.ticked);
    }

    /// Whether the vault watcher is running (it can fail to start).
    pub fn watching(&self) -> bool {
        self._watcher.is_some()
    }

    /// Apply queued vault changes; true when the rows may have changed.
    pub fn poll(&mut self) -> bool {
        let paths: Vec<PathBuf> = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        if paths.is_empty() {
            return false;
        }
        self.index.apply_changes(&paths);
        self.rebuild();
        true
    }

    /// Entering the view starts a fresh session: earlier ticks drop out.
    pub fn enter(&mut self) {
        self.ticked.clear();
        self.poll();
        self.rebuild();
    }

    /// Tick or untick the task in row `i`.
    pub fn toggle(&mut self, i: usize) -> Result<(), String> {
        let Some(Row::Task { path, line, done, .. }) = self.rows.get(i).cloned() else { return Ok(()) };
        let status = if done { ' ' } else { 'x' };
        self.index.set_task(&path, line, status).map_err(|e| e.to_string())?;
        if done {
            self.ticked.remove(&(path, line));
        } else {
            self.ticked.insert((path, line));
        }
        self.rebuild();
        Ok(())
    }

    /// Set or clear the due date of the task in row `i`, in its note.
    pub fn set_due(&mut self, i: usize, due: Option<chrono::NaiveDate>) -> Result<(), String> {
        let Some(Row::Task { path, line, .. }) = self.rows.get(i).cloned() else { return Ok(()) };
        self.index.set_task_due(&path, line, due).map_err(|e| e.to_string())?;
        self.rebuild();
        Ok(())
    }

    /// Add `- [ ] text` to today's daily note (created from the vault's
    /// template if missing); returns its path.
    pub fn add(&mut self, text: &str) -> Result<String, String> {
        let today = chrono::Local::now().date_naive();
        let (path, _) = self.index.daily(today, true).map_err(|e| e.to_string())?;
        self.index.append(&path, &format!("- [ ] {text}")).map_err(|e| e.to_string())?;
        self.rebuild();
        Ok(path)
    }

    /// Show a note in cce-notes: hand it to the running instance over its
    /// socket, or start one.
    pub fn open_note(&self, path: &str) {
        let abs = self.index.abs(path);
        let sock = cce_ui::ipc::socket_path("cce-notes");
        if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
            if s.write_all(format!("open {}\n", abs.display()).as_bytes()).is_ok() {
                let mut reply = String::new();
                let _ = BufReader::new(s).read_line(&mut reply);
                return;
            }
        }
        let mut notes = std::process::Command::new("cce-notes");
        notes.arg("open").arg(&abs);
        let _ = spawn_detached(notes);
    }
}

/// Spawn `cmd` and reap it on a background thread, so the child never lingers
/// as a zombie once it exits. The same helper cce-mail, cce-files, cce-terminal
/// and cce-system-interface each keep; cce-ui's shared `process::spawn_detached`
/// went away in cce-ui 4e94236.
fn spawn_detached(mut cmd: std::process::Command) -> std::io::Result<()> {
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: char, text: &str, line: usize) -> Task {
        Task { status, text: text.into(), line, status_at: 0, due: None }
    }

    #[test]
    fn rows_group_open_tasks_and_keep_this_sessions_ticks() {
        let a = [task(' ', "one", 1), task('x', "old done", 2)];
        let b = [task('x', "ticked now", 4), task('x', "long done", 5)];
        let c = [task('x', "all done", 0)];
        let all: Vec<(&str, &Task)> = a
            .iter()
            .map(|t| ("A.md", t))
            .chain(b.iter().map(|t| ("dir/B.md", t)))
            .chain(c.iter().map(|t| ("C.md", t)))
            .collect();
        let ticked: HashSet<(String, usize)> = [("dir/B.md".to_string(), 4)].into_iter().collect();
        let rows = build_rows(all.into_iter(), &ticked);
        assert_eq!(
            rows,
            [
                Row::Note { path: "A.md".into(), name: "A".into() },
                Row::Task { path: "A.md".into(), line: 1, text: "one".into(), done: false, due: None },
                Row::Note { path: "dir/B.md".into(), name: "B".into() },
                Row::Task { path: "dir/B.md".into(), line: 4, text: "ticked now".into(), done: true, due: None },
            ]
        );
    }
}
